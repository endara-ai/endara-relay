#!/bin/bash
# A minimal MCP server that responds to JSON-RPC requests over stdin/stdout.
# Supports: initialize, tools/list, tools/call (get_image tool).
#
# The get_image tool returns a content array with a single image block:
# { type: "image", mimeType: "image/jpeg", data: <base64> } carrying a
# synthetic 322-byte JPEG (SOI FF D8 FF ... EOI FF D9). The same base64
# payload is hardcoded as JPEG_BASE64 in tests/write_file_integration.rs.

IMG_B64="/9j/4AAQSkZJRgABAQAAAQABAAADChEYHyYtNDtCSVBXXmVsc3qBiI+WnaSrsrnAx87V3OPq8fj/Bg0UGyIpMDc+RUxTWmFob3Z9hIuSmaCnrrW8w8rR2N/m7fT7AgkQFx4lLDM6QUhPVl1ka3J5gIeOlZyjqrG4v8bN1Nvi6fD3/gUMExohKC82PURLUllgZ251fIOKkZifpq20u8LJ0Nfe5ezz+gEIDxYdJCsyOUBHTlVcY2pxeH+GjZSboqmwt77FzNPa4ejv9v0ECxIZICcuNTxDSlFYX2ZtdHuCiZCXnqWss7rByM/W3eTr8vkABw4VHCMqMTg/Rk1UW2JpcHd+hYyTmqGor7a9xMvS2eDn7vX8AwoRGB8mLTQ7QklQV15lbHN6gYiPlp2kq7K5wMfO1dzj6vH4/wYNFBsiKTD/2Q=="

while IFS= read -r line; do
    # Parse the method from JSON (simple grep-based extraction)
    method=$(echo "$line" | sed -n 's/.*"method"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    id=$(echo "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')

    case "$method" in
        "initialize")
            echo "{\"jsonrpc\":\"2.0\",\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"image-mcp\",\"version\":\"0.1.0\"}},\"id\":$id}"
            ;;
        "tools/list")
            echo "{\"jsonrpc\":\"2.0\",\"result\":{\"tools\":[{\"name\":\"get_image\",\"description\":\"Returns a synthetic JPEG as an image content block\",\"inputSchema\":{\"type\":\"object\",\"properties\":{}}}]},\"id\":$id}"
            ;;
        "tools/call")
            echo "{\"jsonrpc\":\"2.0\",\"result\":{\"content\":[{\"type\":\"image\",\"mimeType\":\"image/jpeg\",\"data\":\"$IMG_B64\"}]},\"id\":$id}"
            ;;
        *)
            # Notifications (no id) must not produce a response.
            if [ -n "$id" ]; then
                echo "{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32601,\"message\":\"Method not found\"},\"id\":$id}"
            fi
            ;;
    esac
done
