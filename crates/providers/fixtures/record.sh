#!/usr/bin/env bash
# Record raw SSE streams from an OpenAI-compatible endpoint into this
# directory. The files are byte-for-byte what the server sent; the parser
# tests run against them. Re-run when the endpoint or model changes.
#
#   TENSORX_API_KEY=... ./record.sh https://api.tensorx.ai/v1 z-ai/glm-5.3
#
# Reads the key from the variable named by AIGENTIC_API_KEY_ENV
# (default TENSORX_API_KEY). Never prints it.
set -euo pipefail
base_url="${1:?base url, e.g. https://api.tensorx.ai/v1}"
model="${2:?model id}"
key_env="${AIGENTIC_API_KEY_ENV:-TENSORX_API_KEY}"
key="${!key_env:?environment variable $key_env is not set}"
dir="$(cd "$(dirname "$0")" && pwd)"

post() { # name, json body
  curl -sS -N --fail-with-body \
    -H "Authorization: Bearer $key" \
    -H "Content-Type: application/json" \
    -d "$2" "$base_url/chat/completions" > "$dir/$1.sse"
  printf '%s: %s bytes, %s events\n' "$1" "$(wc -c < "$dir/$1.sse" | tr -d ' ')" "$(grep -c '^data:' "$dir/$1.sse")"
}

tools='[{"type":"function","function":{"name":"read_file","description":"Read a UTF-8 text file","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}},{"type":"function","function":{"name":"bash","description":"Run a shell command","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}}]'

post text "$(cat <<JSON
{"model":"$model","stream":true,"stream_options":{"include_usage":true},
 "messages":[{"role":"user","content":"Reply with exactly: Hello, world."}]}
JSON
)"

post tool_calls "$(cat <<JSON
{"model":"$model","stream":true,"stream_options":{"include_usage":true},"tools":$tools,
 "messages":[{"role":"system","content":"You must use the provided tools. Do not answer from memory."},
             {"role":"user","content":"Read Cargo.toml and then list the current directory with ls -la. Call both tools now."}]}
JSON
)"

post length "$(cat <<JSON
{"model":"$model","stream":true,"stream_options":{"include_usage":true},"max_tokens":8,
 "messages":[{"role":"user","content":"Write three paragraphs about the sea."}]}
JSON
)"
