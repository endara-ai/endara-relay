use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    pub id: u64,
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    pub id: Option<u64>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Create a new JSON-RPC 2.0 request.
pub fn new_request(method: &str, params: Option<Value>, id: u64) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        method: method.to_string(),
        params,
        id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_request_serialization_roundtrip() {
        let req = new_request(
            "initialize",
            Some(json!({"protocolVersion": "2024-11-05"})),
            1,
        );
        let serialized = serde_json::to_string(&req).unwrap();
        let deserialized: JsonRpcRequest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.jsonrpc, "2.0");
        assert_eq!(deserialized.method, "initialize");
        assert_eq!(deserialized.id, 1);
        assert!(deserialized.params.is_some());
    }

    #[test]
    fn test_request_without_params() {
        let req = new_request("tools/list", None, 2);
        let serialized = serde_json::to_string(&req).unwrap();
        // params should not appear in JSON when None
        assert!(!serialized.contains("params"));
        let deserialized: JsonRpcRequest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.method, "tools/list");
        assert!(deserialized.params.is_none());
    }

    #[test]
    fn test_response_with_result() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            result: Some(json!({"tools": []})),
            error: None,
            id: Some(1),
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        let deserialized: JsonRpcResponse = serde_json::from_str(&serialized).unwrap();
        assert!(deserialized.result.is_some());
        assert!(deserialized.error.is_none());
    }

    #[test]
    fn test_response_with_error() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: "Method not found".to_string(),
                data: None,
            }),
            id: Some(1),
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        let deserialized: JsonRpcResponse = serde_json::from_str(&serialized).unwrap();
        assert!(deserialized.result.is_none());
        let err = deserialized.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    #[test]
    fn test_newline_delimited_framing() {
        // Verify that a request serializes to a single line (no embedded newlines)
        let req = new_request("test", Some(json!({"key": "value with spaces"})), 42);
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains('\n'));
        // Adding newline delimiter
        let framed = format!("{}\n", line);
        let parsed: JsonRpcRequest = serde_json::from_str(framed.trim()).unwrap();
        assert_eq!(parsed.id, 42);
    }
}
