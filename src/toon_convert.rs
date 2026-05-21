//! Convert JSON tool responses to TOON (Token-Oriented Object Notation) format
//! before they reach the MCP client. See the Engineering WSpec for the full
//! design; conversion rules live in §3.4.

use serde_json::Value;
use toon_format::encode_default;

/// Convert JSON text content in a `CallToolResult` envelope to TOON where
/// possible. Returns the original value unchanged if the envelope doesn't
/// match, the response is an error, or individual entries are not
/// JSON-convertible.
pub fn toonify_call_result(mut result: Value) -> Value {
    // Don't convert error responses — models may need the exact JSON
    // structure to understand what went wrong.
    if result
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return result;
    }

    let Some(content) = result.get_mut("content").and_then(|v| v.as_array_mut()) else {
        return result;
    };

    for item in content.iter_mut() {
        // Only convert TextContent entries.
        if item.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }

        let Some(text) = item.get("text").and_then(|v| v.as_str()) else {
            continue;
        };

        // Try to parse as JSON. Non-JSON text (plain text, markdown, …)
        // passes through untouched.
        let Ok(json_val) = serde_json::from_str::<Value>(text) else {
            continue;
        };

        // Skip scalars — TOON shines on objects/arrays, not "42" or "hello".
        if !json_val.is_object() && !json_val.is_array() {
            continue;
        }

        // Encode to TOON; silently keep original JSON text on failure.
        if let Ok(toon_text) = encode_default(&json_val) {
            item["text"] = Value::String(toon_text);
        }
    }

    result
}

/// Convert a JS execution result to TOON for `wrap_meta_tool_result`.
/// Returns the TOON string, falling back to pretty-printed JSON on failure or
/// when given a scalar input.
pub fn toonify_value(val: &Value) -> String {
    if !val.is_object() && !val.is_array() {
        return serde_json::to_string_pretty(val).unwrap_or_default();
    }

    encode_default(val).unwrap_or_else(|_| serde_json::to_string_pretty(val).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use toon_format::decode_default;

    // §5 row 1: toonify_call_result converts JSON object in TextContent.text
    // to TOON.
    #[test]
    fn converts_json_object_in_text_content() {
        let input = json!({
            "content": [{
                "type": "text",
                "text": "{\"name\":\"Alice\",\"age\":30}"
            }],
            "isError": false
        });
        let out = toonify_call_result(input);
        let text = out["content"][0]["text"].as_str().unwrap();
        assert_ne!(text, "{\"name\":\"Alice\",\"age\":30}");
        // Round-trip back to JSON to confirm fidelity.
        let decoded: Value = decode_default(text).unwrap();
        assert_eq!(decoded, json!({"name": "Alice", "age": 30}));
    }

    // §5 row 2: toonify_call_result converts JSON array in TextContent.text
    // to TOON.
    #[test]
    fn converts_json_array_in_text_content() {
        let input = json!({
            "content": [{
                "type": "text",
                "text": "[{\"id\":1,\"name\":\"a\"},{\"id\":2,\"name\":\"b\"}]"
            }]
        });
        let out = toonify_call_result(input);
        let text = out["content"][0]["text"].as_str().unwrap();
        // TOON arrays start with `[N]{fields}:` — not valid JSON.
        assert!(
            serde_json::from_str::<Value>(text).is_err(),
            "expected TOON, got JSON: {text}"
        );
        let decoded: Value = decode_default(text).unwrap();
        assert_eq!(
            decoded,
            json!([{"id": 1, "name": "a"}, {"id": 2, "name": "b"}])
        );
    }

    // §5 row 3: toonify_call_result leaves plain text (non-JSON) TextContent
    // unchanged.
    #[test]
    fn leaves_plain_text_unchanged() {
        let input = json!({
            "content": [{
                "type": "text",
                "text": "Hello, world! This is # not JSON."
            }]
        });
        let out = toonify_call_result(input.clone());
        assert_eq!(out, input);
    }

    // §5 row 4: toonify_call_result leaves ImageContent entries unchanged.
    #[test]
    fn leaves_image_content_unchanged() {
        let input = json!({
            "content": [{
                "type": "image",
                "data": "iVBORw0KGgoAAAANSUhEUg==",
                "mimeType": "image/png"
            }]
        });
        let out = toonify_call_result(input.clone());
        assert_eq!(out, input);
    }

    // §5 row 5: toonify_call_result leaves isError: true responses unchanged.
    #[test]
    fn leaves_error_responses_unchanged() {
        let input = json!({
            "content": [{
                "type": "text",
                "text": "{\"error\":\"boom\",\"code\":500}"
            }],
            "isError": true
        });
        let out = toonify_call_result(input.clone());
        assert_eq!(out, input);
    }

    // §5 row 6: toonify_call_result leaves JSON scalar TextContent unchanged.
    #[test]
    fn leaves_json_scalar_text_unchanged() {
        for scalar in ["42", "\"hello\"", "true", "null", "3.14"] {
            let input = json!({
                "content": [{ "type": "text", "text": scalar }]
            });
            let out = toonify_call_result(input.clone());
            assert_eq!(out, input, "scalar {scalar} should pass through unchanged");
        }
    }

    // §5 row 7: toonify_call_result handles mixed content array
    // (text + JSON + image).
    #[test]
    fn handles_mixed_content_array() {
        let input = json!({
            "content": [
                { "type": "text", "text": "Summary line, not JSON." },
                { "type": "text", "text": "{\"k\":\"v\"}" },
                {
                    "type": "image",
                    "data": "abc",
                    "mimeType": "image/png"
                }
            ]
        });
        let out = toonify_call_result(input);
        // Plain text untouched.
        assert_eq!(out["content"][0]["text"], "Summary line, not JSON.");
        // JSON converted.
        let toon = out["content"][1]["text"].as_str().unwrap();
        assert_ne!(toon, "{\"k\":\"v\"}");
        let decoded: Value = decode_default(toon).unwrap();
        assert_eq!(decoded, json!({"k": "v"}));
        // Image untouched.
        assert_eq!(out["content"][2]["type"], "image");
        assert_eq!(out["content"][2]["data"], "abc");
    }

    // §5 row 8: toonify_call_result handles content with no text field
    // gracefully.
    #[test]
    fn handles_text_entry_without_text_field() {
        let input = json!({
            "content": [{ "type": "text" }]
        });
        let out = toonify_call_result(input.clone());
        assert_eq!(out, input);

        // Also gracefully handles missing `content` arrays entirely.
        let no_content = json!({ "isError": false });
        let out2 = toonify_call_result(no_content.clone());
        assert_eq!(out2, no_content);

        // And non-array `content` values.
        let weird = json!({ "content": "not-an-array" });
        let out3 = toonify_call_result(weird.clone());
        assert_eq!(out3, weird);
    }

    // §5 row 9: toonify_value converts JSON object to TOON string.
    #[test]
    fn toonify_value_converts_object_to_toon() {
        let val = json!({ "users": [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}] });
        let out = toonify_value(&val);
        // TOON encodes a tabular array with `[2]` header for two-row uniform
        // arrays — anything ending the prefix-only form is acceptable here as
        // long as a round-trip restores the original JSON.
        let decoded: Value = decode_default(&out).unwrap();
        assert_eq!(decoded, val);
    }

    // §5 row 10: toonify_value falls back to pretty JSON on scalar input.
    #[test]
    fn toonify_value_falls_back_on_scalar() {
        assert_eq!(toonify_value(&json!(42)), "42");
        assert_eq!(toonify_value(&json!("hello")), "\"hello\"");
        assert_eq!(toonify_value(&json!(true)), "true");
        assert_eq!(toonify_value(&json!(null)), "null");
    }

    // §5 row 13: Round-trip — decode(encode(json)) == json for
    // representative tool responses.
    #[test]
    fn round_trip_preserves_representative_payloads() {
        let cases = vec![
            json!({"name": "Alice", "age": 30, "active": true}),
            json!([1, 2, 3, 4, 5]),
            json!({
                "users": [
                    {"id": 1, "name": "Alice", "active": true},
                    {"id": 2, "name": "Bob", "active": false},
                    {"id": 3, "name": "Carol", "active": true}
                ]
            }),
            json!({"nested": {"inner": {"deep": [1, 2, 3]}}}),
            json!({"empty_obj": {}, "empty_arr": []}),
            json!({"mixed": [1, "two", true, null, {"k": "v"}]}),
        ];
        for case in cases {
            let toon = encode_default(&case).expect("encode succeeds");
            let back: Value = decode_default(&toon).expect("decode succeeds");
            assert_eq!(back, case, "round-trip lost data: {case}");
        }
    }
}
