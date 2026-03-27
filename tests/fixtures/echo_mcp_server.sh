#!/bin/bash
# A minimal MCP server that responds to JSON-RPC requests over stdin/stdout.
# Supports: initialize, tools/list, tools/call (echo tool)

while IFS= read -r line; do
    # Parse the method from JSON (simple grep-based extraction)
    method=$(echo "$line" | sed -n 's/.*"method"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    id=$(echo "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')

    case "$method" in
        "initialize")
            echo "{\"jsonrpc\":\"2.0\",\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"echo-mcp\",\"version\":\"0.1.0\"}},\"id\":$id}"
            ;;
        "tools/list")
            echo "{\"jsonrpc\":\"2.0\",\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echoes input back\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"message\":{\"type\":\"string\"}},\"required\":[\"message\"]}}]},\"id\":$id}"
            ;;
        "tools/call")
            # Extract the message argument
            message=$(echo "$line" | sed -n 's/.*"message"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
            echo "{\"jsonrpc\":\"2.0\",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"echo: $message\"}]},\"id\":$id}"
            ;;
        *)
            echo "{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32601,\"message\":\"Method not found\"},\"id\":$id}"
            ;;
    esac
done

