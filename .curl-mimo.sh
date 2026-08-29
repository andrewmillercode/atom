#!/bin/bash
# Mimics the tool-call scenario: a one-shot prompt with grep tool available.
curl -sN https://opencode.ai/zen/go/v1/chat/completions \
  -H "Authorization: Bearer sk-k3VQLCZPh7pBNhiMbJEFK7srPlbWDxtcnVGqQHnnHBlLPGxtSXLbUdk0hMXMfuFy" \
  -H "Content-Type: application/json" \
  -H "HTTP-Referer: https://opencode.ai/" \
  -H "X-Title: opencode" \
  -d '{
    "model": "mimo-v2.5",
    "stream": true,
    "stream_options": {"include_usage": true},
    "messages": [
      {"role": "user", "content": "Find where the project version is defined in this Rust workspace. Use the grep tool."}
    ],
    "tools": [
      {"type": "function", "function": {
        "name": "grep",
        "description": "ripgrep search",
        "parameters": {
          "type": "object",
          "properties": {
            "pattern": {"type": "string"},
            "path": {"type": "string"},
            "include": {"type": "string"}
          },
          "required": ["pattern"]
        }
      }}
    ],
    "tool_choice": "auto"
  }' | tee /Users/andrewmiller/projects/atom/.mimo-stream.txt
