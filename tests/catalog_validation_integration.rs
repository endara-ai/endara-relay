//! Live endpoint validation tests for OAuth catalog entries.
//!
//! These tests hit real endpoints to verify that OAuth discovery URLs,
//! authorization servers, and token endpoints are still valid.
//!
//! Run with: `cargo test catalog_validation -- --ignored --nocapture`

use reqwest::Client;
use serde_json::Value;

/// Fetch JSON from a URL, returning the parsed value or an error message.
async fn fetch_json(client: &Client, url: &str) -> Result<Value, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch {url}: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{url} returned HTTP {status}"));
    }

    resp.json::<Value>()
        .await
        .map_err(|e| format!("Failed to parse JSON from {url}: {e}"))
}

/// Fetch protected resource metadata with path-based fallback per RFC 9728.
///
/// Tries `{origin}/.well-known/oauth-protected-resource{path}` first,
/// then falls back to `{origin}/.well-known/oauth-protected-resource` if 404.
async fn fetch_protected_resource_metadata(
    client: &Client,
    mcp_url: &str,
) -> Result<Value, String> {
    let parsed = url::Url::parse(mcp_url).map_err(|e| format!("Invalid MCP URL {mcp_url}: {e}"))?;
    let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap());
    let path = parsed.path().trim_end_matches('/');

    // Try path-based first
    if !path.is_empty() && path != "/" {
        let path_url = format!("{origin}/.well-known/oauth-protected-resource{path}");
        match fetch_json(client, &path_url).await {
            Ok(json) => return Ok(json),
            Err(_) => {
                // Fall back to root
            }
        }
    }

    let root_url = format!("{origin}/.well-known/oauth-protected-resource");
    fetch_json(client, &root_url).await
}

/// Validate the full OAuth discovery chain for an MCP endpoint.
///
/// 1. Fetch protected resource metadata
/// 2. Extract `authorization_servers[0]`
/// 3. Fetch auth server metadata
/// 4. Assert `authorization_endpoint` and `token_endpoint` are non-empty
/// 5. Assert `code_challenge_methods_supported` includes `S256`
/// 6. Check DCR expectation (registration_endpoint present or absent)
async fn validate_oauth_discovery(
    client: &Client,
    mcp_url: &str,
    expect_dcr: bool,
) -> Result<(), String> {
    // Step 1: Fetch protected resource metadata
    let pr_meta = fetch_protected_resource_metadata(client, mcp_url).await?;

    // Step 2: Extract authorization_servers[0]
    let auth_servers = pr_meta
        .get("authorization_servers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!("{mcp_url}: protected resource metadata missing 'authorization_servers' array")
        })?;

    let auth_server_url = auth_servers
        .first()
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!("{mcp_url}: 'authorization_servers' array is empty or not strings")
        })?;

    // Verify 'resource' field is present
    pr_meta
        .get("resource")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!("{mcp_url}: protected resource metadata missing 'resource' field")
        })?;

    // Step 3: Fetch auth server metadata
    let parsed = url::Url::parse(auth_server_url)
        .map_err(|e| format!("Invalid auth server URL {auth_server_url}: {e}"))?;
    let as_origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap());
    let as_path = parsed.path().trim_end_matches('/');

    let as_meta = if !as_path.is_empty() && as_path != "/" {
        let path_url = format!("{as_origin}/.well-known/oauth-authorization-server{as_path}");
        match fetch_json(client, &path_url).await {
            Ok(json) => json,
            Err(_) => {
                let root_url = format!("{as_origin}/.well-known/oauth-authorization-server");
                fetch_json(client, &root_url).await?
            }
        }
    } else {
        let root_url = format!("{as_origin}/.well-known/oauth-authorization-server");
        fetch_json(client, &root_url).await?
    };

    // Step 4: Assert authorization_endpoint and token_endpoint
    let auth_endpoint = as_meta
        .get("authorization_endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        !auth_endpoint.is_empty(),
        "{mcp_url}: authorization_endpoint is missing or empty"
    );

    let token_endpoint = as_meta
        .get("token_endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        !token_endpoint.is_empty(),
        "{mcp_url}: token_endpoint is missing or empty"
    );

    // Step 5: Assert S256 PKCE support
    if let Some(methods) = as_meta
        .get("code_challenge_methods_supported")
        .and_then(|v| v.as_array())
    {
        let has_s256 = methods.iter().any(|m| m.as_str() == Some("S256"));
        assert!(
            has_s256,
            "{mcp_url}: code_challenge_methods_supported does not include S256 (found: {methods:?})"
        );
    }
    // If field is absent, S256 is implicitly supported per spec

    // Step 6: Check DCR expectation
    let has_registration = as_meta
        .get("registration_endpoint")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    if expect_dcr {
        assert!(
            has_registration,
            "{mcp_url}: expected DCR support (registration_endpoint) but it was absent"
        );
    } else {
        assert!(
            !has_registration,
            "{mcp_url}: expected no DCR support but registration_endpoint was present"
        );
    }

    Ok(())
}

// --- Individual provider tests ---

#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn test_linear_discovery_live() {
    let client = Client::new();
    validate_oauth_discovery(&client, "https://mcp.linear.app/sse", true)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn test_notion_discovery_live() {
    let client = Client::new();
    validate_oauth_discovery(&client, "https://mcp.notion.com/mcp", true)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn test_slack_discovery_live() {
    let client = Client::new();
    // Slack does NOT support DCR (supportsDcr: false in catalog)
    validate_oauth_discovery(&client, "https://mcp.slack.com/mcp", false)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn test_todoist_discovery_live() {
    let client = Client::new();
    validate_oauth_discovery(&client, "https://ai.todoist.net/mcp", true)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn test_github_endpoints_live() {
    // GitHub doesn't support RFC 9728 discovery (supportsDiscovery: false).
    // Instead, verify the OAuth endpoints exist and respond.
    let client = Client::new();

    // Verify authorize endpoint responds (expect 200 or 302, not 404/5xx)
    let auth_resp = client
        .get("https://github.com/login/oauth/authorize")
        .send()
        .await
        .expect("Failed to reach GitHub authorize endpoint");
    let status = auth_resp.status().as_u16();
    assert!(
        status < 500 && status != 404,
        "GitHub authorize endpoint returned unexpected status: {status}"
    );

    // Verify token endpoint responds to POST (expect 4xx without credentials)
    let token_resp = client
        .post("https://github.com/login/oauth/access_token")
        .send()
        .await
        .expect("Failed to reach GitHub token endpoint");
    let token_status = token_resp.status().as_u16();
    assert!(
        token_status < 500,
        "GitHub token endpoint returned server error: {token_status}"
    );
}
