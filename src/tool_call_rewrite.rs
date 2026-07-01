//! `tools/call` result URI rewriting (enumerated slots #2 and #3).
//!
//! STRICT DD1: this walker only touches the MCP-defined locations where a
//! URI is a resource reference the client will hand back to `resources/read`.
//! Arbitrary tree-walking is forbidden — URL-shaped data in `text`/`image`/
//! `audio` blocks and in `structuredContent` must round-trip untouched.
//!
//! DD3: this pass operates on `resource`/`resource_link` content blocks and
//! `_meta` UI pointers; [`crate::toon_convert::toonify_call_result`] operates
//! on `type=="text"` blocks. The two passes touch disjoint slots, so the
//! pipeline composes in either order with identical output (idempotency).

use serde_json::{Map, Value};

use crate::resource_uri::maybe_encode_resource_uri;

/// Wrap the two MCP Apps UI pointer slots on a `_meta` object in place:
/// `_meta.ui.resourceUri` and the alias `_meta["openai/outputTemplate"]`.
///
/// Shared between slot #1 (`tools/list` tool descriptor `_meta`, applied by
/// [`crate::registry::AdapterRegistry::build_catalog`]) and slot #2
/// (`tools/call` result `_meta`, applied by [`rewrite_tool_call_result`]) so
/// both call sites encode through the same code path — a divergence would let
/// a pointer the client received from `tools/list` fail to reverse on
/// `resources/read`. Strict DD1: only these two enumerated fields are
/// touched; arbitrary `_meta` siblings (including `ui.csp`) pass through
/// untouched. Idempotent only relative to non-URI passes — re-wrapping nests
/// the wrapper (mirrors [`maybe_encode_resource_uri`]).
pub(crate) fn rewrite_meta_ui_pointers(meta: &mut Map<String, Value>, endpoint: &str) {
    if let Some(ui) = meta.get_mut("ui").and_then(|v| v.as_object_mut()) {
        if let Some(uri) = ui.get("resourceUri").and_then(|v| v.as_str()) {
            let wrapped = maybe_encode_resource_uri(endpoint, uri, false);
            ui.insert("resourceUri".to_string(), Value::String(wrapped));
        }
    }
    if let Some(uri) = meta.get("openai/outputTemplate").and_then(|v| v.as_str()) {
        let wrapped = maybe_encode_resource_uri(endpoint, uri, false);
        meta.insert("openai/outputTemplate".to_string(), Value::String(wrapped));
    }
}

/// Rewrite the enumerated URI slots of a `tools/call` result to namespace them
/// to `endpoint`.
///
/// Touched slots (DD1):
/// - `result._meta.ui.resourceUri` (slot #2)
/// - `result._meta["openai/outputTemplate"]` (slot #2 alias)
/// - `result.content[]` where `type=="resource"` -> `resource.uri` (slot #3)
/// - `result.content[]` where `type=="resource_link"` -> top-level `uri` (slot #3)
///
/// Everything else passes through verbatim. `skip_wrap` mirrors
/// [`crate::registry::AdapterRegistry::list_resources`]'s DD5 trigger
/// (`active_count <= 1`): a true value short-circuits the whole walk so
/// single-endpoint mode is a strict byte-for-byte no-op.
pub fn rewrite_tool_call_result(mut result: Value, endpoint: &str, skip_wrap: bool) -> Value {
    if skip_wrap {
        return result;
    }

    // Slot #2: `_meta.ui.resourceUri` and the alias `_meta["openai/outputTemplate"]`.
    // Delegated to the shared helper so slot #1 (`tools/list` descriptor) and
    // slot #2 (`tools/call` result) encode through identical logic.
    if let Some(meta) = result.get_mut("_meta").and_then(|v| v.as_object_mut()) {
        rewrite_meta_ui_pointers(meta, endpoint);
    }

    // Slot #3: `content[]` where `type=="resource"` -> `resource.uri`; where
    // `type=="resource_link"` -> top-level `uri`. Iterate by type discriminant
    // and ignore everything else (text/image/audio/...) per DD1.
    if let Some(content) = result.get_mut("content").and_then(|v| v.as_array_mut()) {
        for item in content.iter_mut() {
            let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "resource" => {
                    if let Some(resource) = item.get_mut("resource").and_then(|v| v.as_object_mut())
                    {
                        if let Some(uri) = resource.get("uri").and_then(|v| v.as_str()) {
                            let wrapped = maybe_encode_resource_uri(endpoint, uri, false);
                            resource.insert("uri".to_string(), Value::String(wrapped));
                        }
                    }
                }
                "resource_link" => {
                    if let Some(obj) = item.as_object_mut() {
                        if let Some(uri) = obj.get("uri").and_then(|v| v.as_str()) {
                            let wrapped = maybe_encode_resource_uri(endpoint, uri, false);
                            obj.insert("uri".to_string(), Value::String(wrapped));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    result
}

/// Rewrite the enumerated URI slots of a `prompts/get` result (slot #9) to
/// namespace them to `endpoint`.
///
/// Touched slots (STRICT DD1):
/// - `result.messages[].content` where `type=="resource"` -> `resource.uri`
/// - `result.messages[].content` where `type=="resource_link"` -> top-level `uri`
///
/// Mirrors [`rewrite_tool_call_result`]'s slot #3 walk: iterates by `type`
/// discriminant and ignores everything else (text/image/audio/...) so URL-
/// shaped data in `text` blocks round-trips byte-for-byte. The MCP
/// `PromptMessage` shape allows `content` to be either a single content
/// block (object) or an array of blocks; both shapes are handled. Sibling
/// fields on each message (`role`, etc.) and on the result (`description`)
/// pass through untouched.
///
/// `skip_wrap` mirrors the DD5 single-endpoint trigger
/// (`active_count <= 1`): a true value short-circuits the whole walk so
/// single-endpoint mode is a strict byte-for-byte no-op. Re-uses the same
/// [`maybe_encode_resource_uri`] primitive as slots #2/#3 so a pointer the
/// client receives from `prompts/get` reverses on `resources/read` through
/// the identical decoder — a divergence would let a prompt-borne pointer
/// fail to read back.
pub fn rewrite_prompt_get_result(mut result: Value, endpoint: &str, skip_wrap: bool) -> Value {
    if skip_wrap {
        return result;
    }

    if let Some(messages) = result.get_mut("messages").and_then(|v| v.as_array_mut()) {
        for message in messages.iter_mut() {
            let Some(content) = message.get_mut("content") else {
                continue;
            };
            match content {
                Value::Array(items) => {
                    for item in items.iter_mut() {
                        rewrite_prompt_message_content_block(item, endpoint);
                    }
                }
                Value::Object(_) => {
                    rewrite_prompt_message_content_block(content, endpoint);
                }
                _ => {}
            }
        }
    }

    result
}

/// Wrap the URI slot inside a single `messages[].content` block when its
/// `type` is `resource` (rewrites `resource.uri`) or `resource_link`
/// (rewrites the top-level `uri`). DD1: all other types — including the
/// MCP-defined `text`/`image`/`audio` blocks — pass through verbatim.
fn rewrite_prompt_message_content_block(item: &mut Value, endpoint: &str) {
    let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "resource" => {
            if let Some(resource) = item.get_mut("resource").and_then(|v| v.as_object_mut()) {
                if let Some(uri) = resource.get("uri").and_then(|v| v.as_str()) {
                    let wrapped = maybe_encode_resource_uri(endpoint, uri, false);
                    resource.insert("uri".to_string(), Value::String(wrapped));
                }
            }
        }
        "resource_link" => {
            if let Some(obj) = item.as_object_mut() {
                if let Some(uri) = obj.get("uri").and_then(|v| v.as_str()) {
                    let wrapped = maybe_encode_resource_uri(endpoint, uri, false);
                    obj.insert("uri".to_string(), Value::String(wrapped));
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_uri::{decode_resource_uri, encode_resource_uri};
    use crate::toon_convert::toonify_call_result;
    use serde_json::json;

    fn sample_result() -> Value {
        json!({
            "_meta": {
                "ui": { "resourceUri": "ui://app/main" },
                "openai/outputTemplate": "ui://app/template",
                "ui.csp": { "origins": ["https://cdn.example.com"] }
            },
            "content": [
                { "type": "text", "text": "see https://example.com/x for details" },
                {
                    "type": "resource",
                    "resource": { "uri": "ui://app/inline", "mimeType": "text/html" }
                },
                { "type": "resource_link", "uri": "ui://app/link", "name": "Open" },
                { "type": "image", "data": "BASE64", "mimeType": "image/png" },
                { "type": "audio", "data": "BASE64", "mimeType": "audio/mp3" }
            ],
            "structuredContent": {
                "url": "https://example.com/data",
                "uri": "ui://app/inside-structured"
            }
        })
    }

    #[test]
    fn rewrites_meta_ui_resource_uri() {
        let out = rewrite_tool_call_result(sample_result(), "work", false);
        let got = out["_meta"]["ui"]["resourceUri"].as_str().unwrap();
        let (ep, orig) = decode_resource_uri(got).unwrap();
        assert_eq!(ep, "work");
        assert_eq!(orig, "ui://app/main");
    }

    #[test]
    fn rewrites_meta_openai_output_template_alias() {
        let out = rewrite_tool_call_result(sample_result(), "work", false);
        let got = out["_meta"]["openai/outputTemplate"].as_str().unwrap();
        let (ep, orig) = decode_resource_uri(got).unwrap();
        assert_eq!(ep, "work");
        assert_eq!(orig, "ui://app/template");
    }

    #[test]
    fn rewrites_content_resource_uri() {
        let out = rewrite_tool_call_result(sample_result(), "work", false);
        let got = out["content"][1]["resource"]["uri"].as_str().unwrap();
        let (ep, orig) = decode_resource_uri(got).unwrap();
        assert_eq!(ep, "work");
        assert_eq!(orig, "ui://app/inline");
        // mimeType siblings inside the resource block survive verbatim.
        assert_eq!(out["content"][1]["resource"]["mimeType"], "text/html");
    }

    #[test]
    fn rewrites_content_resource_link_uri() {
        let out = rewrite_tool_call_result(sample_result(), "work", false);
        let got = out["content"][2]["uri"].as_str().unwrap();
        let (ep, orig) = decode_resource_uri(got).unwrap();
        assert_eq!(ep, "work");
        assert_eq!(orig, "ui://app/link");
        assert_eq!(out["content"][2]["name"], "Open");
    }

    #[test]
    fn leaves_text_image_audio_untouched() {
        let out = rewrite_tool_call_result(sample_result(), "work", false);
        // Text block: URL-shaped data inside `text` is a DD1 regression target.
        assert_eq!(
            out["content"][0]["text"],
            "see https://example.com/x for details"
        );
        // Image / audio: data and mimeType untouched.
        assert_eq!(out["content"][3]["data"], "BASE64");
        assert_eq!(out["content"][3]["mimeType"], "image/png");
        assert_eq!(out["content"][4]["data"], "BASE64");
        assert_eq!(out["content"][4]["mimeType"], "audio/mp3");
    }

    #[test]
    fn leaves_structured_content_untouched() {
        let out = rewrite_tool_call_result(sample_result(), "work", false);
        // Heuristic rewriting of `structuredContent` is a non-goal (v1).
        assert_eq!(out["structuredContent"]["url"], "https://example.com/data");
        assert_eq!(
            out["structuredContent"]["uri"],
            "ui://app/inside-structured"
        );
    }

    #[test]
    fn leaves_meta_ui_csp_untouched() {
        let out = rewrite_tool_call_result(sample_result(), "work", false);
        // `_meta.ui.csp` origins are passed through (left untouched).
        assert_eq!(
            out["_meta"]["ui.csp"]["origins"][0],
            "https://cdn.example.com"
        );
    }

    #[test]
    fn skip_wrap_passes_through_byte_for_byte() {
        // DD5 single-endpoint mode: `skip_wrap = true` is a strict no-op.
        let original = sample_result();
        let out = rewrite_tool_call_result(original.clone(), "work", true);
        assert_eq!(out, original);
    }

    #[test]
    fn empty_result_is_noop() {
        let out = rewrite_tool_call_result(json!({}), "work", false);
        assert_eq!(out, json!({}));
    }

    #[test]
    fn missing_meta_subfields_pass_through() {
        // `_meta` present but no `ui` and no alias: result is structurally
        // unchanged. The walker must not insert defaulted keys.
        let input = json!({
            "_meta": { "other": "value" },
            "content": []
        });
        let out = rewrite_tool_call_result(input.clone(), "work", false);
        assert_eq!(out, input);
    }

    #[test]
    fn order_uri_then_toon_equals_toon_then_uri() {
        // DD3 idempotency / disjoint-block-type composition: running
        // URI-rewrite then TOON must produce the same final result as TOON
        // then URI-rewrite, because the two passes touch disjoint slots.
        let mut input = sample_result();
        // Make the text block JSON-parseable so TOON actually transforms it.
        input["content"][0]["text"] =
            Value::String(serde_json::to_string(&json!({"rows": [{"id": 1}]})).unwrap());

        let uri_then_toon =
            toonify_call_result(rewrite_tool_call_result(input.clone(), "work", false));
        let toon_then_uri =
            rewrite_tool_call_result(toonify_call_result(input.clone()), "work", false);
        assert_eq!(uri_then_toon, toon_then_uri);
    }

    #[test]
    fn rewrite_is_idempotent_only_relative_to_disjoint_passes() {
        // Per DD3 the rewrite is idempotent vs TOON / non-URI passes, NOT vs
        // itself — applying it twice nests the wrapper (mirrors
        // `encode_resource_uri`'s documented behavior). Verify the nested form
        // still decodes cleanly to the once-wrapped form so a double-call
        // doesn't silently corrupt data.
        let once = rewrite_tool_call_result(sample_result(), "work", false);
        let once_uri = once["content"][2]["uri"].as_str().unwrap().to_string();
        let twice = rewrite_tool_call_result(once, "work", false);
        let twice_uri = twice["content"][2]["uri"].as_str().unwrap();
        let (ep_outer, orig_outer) = decode_resource_uri(twice_uri).unwrap();
        assert_eq!(ep_outer, "work");
        assert_eq!(orig_outer, once_uri);
    }

    #[test]
    fn unknown_content_types_pass_through() {
        // Strict DD1: any block whose `type` is not `resource`/`resource_link`
        // is left untouched, even if it has a `uri` field.
        let input = json!({
            "content": [
                { "type": "exotic", "uri": "ui://x/y", "data": "z" }
            ]
        });
        let out = rewrite_tool_call_result(input.clone(), "work", false);
        assert_eq!(out, input);
    }

    #[test]
    fn non_string_uri_is_noop() {
        // A malformed upstream that puts a non-string in `uri` must not
        // crash; the walker leaves it untouched.
        let input = json!({
            "content": [
                { "type": "resource_link", "uri": 42 },
                { "type": "resource", "resource": { "uri": null } }
            ]
        });
        let out = rewrite_tool_call_result(input.clone(), "work", false);
        assert_eq!(out, input);
    }

    #[test]
    fn wrapped_endpoint_matches_explicit_encode() {
        // Sanity: the wrapped value emitted by the rewriter is exactly what
        // `encode_resource_uri(endpoint, original)` produces — no implicit
        // template handling (slot #3 is a URI, not a URI template).
        let out = rewrite_tool_call_result(sample_result(), "ep1", false);
        assert_eq!(
            out["content"][2]["uri"].as_str().unwrap(),
            encode_resource_uri("ep1", "ui://app/link")
        );
    }

    // ---- T10 — `prompts/get` result URI rewriting (slot #9) ---------------

    /// Sample `prompts/get` result covering every shape slot #9 must visit:
    /// an array-content message with text/resource/resource_link blocks
    /// (DD1: only the resource/resource_link blocks wrap), an object-content
    /// message carrying a single `resource` block (PromptMessage allows the
    /// content field to be a single block, not just an array), and an
    /// object-content `text` block (DD1: must round-trip verbatim).
    fn sample_prompt_get_result() -> Value {
        json!({
            "description": "see ui://app/main",
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "see https://example.com/x for details" },
                        {
                            "type": "resource",
                            "resource": { "uri": "ui://app/inline", "mimeType": "text/html" }
                        },
                        { "type": "resource_link", "uri": "ui://app/link", "name": "Open" },
                        { "type": "image", "data": "BASE64", "mimeType": "image/png" }
                    ]
                },
                {
                    "role": "user",
                    "content": {
                        "type": "resource",
                        "resource": { "uri": "ui://app/single", "mimeType": "text/html" }
                    }
                },
                {
                    "role": "user",
                    "content": { "type": "text", "text": "plain prompt body" }
                }
            ]
        })
    }

    #[test]
    fn rewrites_array_content_resource_and_resource_link() {
        let out = rewrite_prompt_get_result(sample_prompt_get_result(), "work", false);
        let res_uri = out["messages"][0]["content"][1]["resource"]["uri"]
            .as_str()
            .unwrap();
        let (ep, orig) = decode_resource_uri(res_uri).unwrap();
        assert_eq!(ep, "work");
        assert_eq!(orig, "ui://app/inline");
        // Sibling fields inside the resource block survive verbatim.
        assert_eq!(
            out["messages"][0]["content"][1]["resource"]["mimeType"],
            "text/html"
        );

        let link_uri = out["messages"][0]["content"][2]["uri"].as_str().unwrap();
        let (ep, orig) = decode_resource_uri(link_uri).unwrap();
        assert_eq!(ep, "work");
        assert_eq!(orig, "ui://app/link");
        assert_eq!(out["messages"][0]["content"][2]["name"], "Open");
    }

    #[test]
    fn rewrites_object_content_single_resource_block() {
        // PromptMessage allows `content` to be a single block (not an array).
        // Slot #9 must wrap it just like the array shape.
        let out = rewrite_prompt_get_result(sample_prompt_get_result(), "work", false);
        let uri = out["messages"][1]["content"]["resource"]["uri"]
            .as_str()
            .unwrap();
        let (ep, orig) = decode_resource_uri(uri).unwrap();
        assert_eq!(ep, "work");
        assert_eq!(orig, "ui://app/single");
    }

    #[test]
    fn leaves_text_image_and_top_level_description_untouched() {
        let out = rewrite_prompt_get_result(sample_prompt_get_result(), "work", false);
        // Text block inside an array-content message: URL-shaped data in
        // `text` is a DD1 regression target.
        assert_eq!(
            out["messages"][0]["content"][0]["text"],
            "see https://example.com/x for details"
        );
        // Image block: data and mimeType untouched.
        assert_eq!(out["messages"][0]["content"][3]["data"], "BASE64");
        assert_eq!(out["messages"][0]["content"][3]["mimeType"], "image/png");
        // Object-content `text` block (no array): untouched.
        assert_eq!(out["messages"][2]["content"]["type"], "text");
        assert_eq!(out["messages"][2]["content"]["text"], "plain prompt body");
        // Top-level result `description` is not an enumerated URI slot.
        assert_eq!(out["description"], "see ui://app/main");
        // Roles round-trip.
        assert_eq!(out["messages"][0]["role"], "assistant");
        assert_eq!(out["messages"][1]["role"], "user");
    }

    #[test]
    fn prompt_get_skip_wrap_passes_through_byte_for_byte() {
        // DD5 single-endpoint mode: `skip_wrap = true` is a strict no-op.
        let original = sample_prompt_get_result();
        let out = rewrite_prompt_get_result(original.clone(), "work", true);
        assert_eq!(out, original);
    }

    #[test]
    fn prompt_get_empty_result_is_noop() {
        let out = rewrite_prompt_get_result(json!({}), "work", false);
        assert_eq!(out, json!({}));
    }

    #[test]
    fn prompt_get_missing_messages_passes_through() {
        let input = json!({ "description": "no messages here" });
        let out = rewrite_prompt_get_result(input.clone(), "work", false);
        assert_eq!(out, input);
    }

    #[test]
    fn prompt_get_unknown_content_types_pass_through() {
        // Strict DD1: any block whose `type` is not `resource`/`resource_link`
        // is left untouched, even if it has a `uri` field.
        let input = json!({
            "messages": [{
                "role": "user",
                "content": [{ "type": "exotic", "uri": "ui://x/y", "data": "z" }]
            }]
        });
        let out = rewrite_prompt_get_result(input.clone(), "work", false);
        assert_eq!(out, input);
    }

    #[test]
    fn prompt_get_non_string_uri_is_noop() {
        // A malformed upstream that puts a non-string in `uri` must not
        // crash; the walker leaves it untouched.
        let input = json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "resource_link", "uri": 42 },
                    { "type": "resource", "resource": { "uri": null } }
                ]
            }]
        });
        let out = rewrite_prompt_get_result(input.clone(), "work", false);
        assert_eq!(out, input);
    }

    #[test]
    fn prompt_get_wrapped_endpoint_matches_explicit_encode() {
        // Sanity: the wrapped value emitted by the rewriter is exactly what
        // `encode_resource_uri(endpoint, original)` produces — slot #9 uses
        // the same wrapper primitive as slot #3 so a pointer reverses on
        // `resources/read` through the identical decoder.
        let out = rewrite_prompt_get_result(sample_prompt_get_result(), "ep1", false);
        assert_eq!(
            out["messages"][0]["content"][2]["uri"].as_str().unwrap(),
            encode_resource_uri("ep1", "ui://app/link")
        );
    }
}
