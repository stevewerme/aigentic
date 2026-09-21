#!/usr/bin/env bash
# Record raw SSE streams into ./openai_compat/ or ./anthropic/. The files are byte-for-byte what the server sent; the parser
# tests run against them. Re-run when the endpoint or model changes.
#
#   TENSORX_API_KEY=...   ./record.sh https://api.tensorx.ai/v1 z-ai/glm-5.3
#   ANTHROPIC_API_KEY=... ./record.sh anthropic claude-opus-5
#
# Reads the key from the variable named by AIGENTIC_API_KEY_ENV (default
# TENSORX_API_KEY, or ANTHROPIC_API_KEY in anthropic mode). Never prints it.
set -euo pipefail
target="${1:?base url of an OpenAI-compatible endpoint, or the word anthropic}"
model="${2:?model id}"
here="$(cd "$(dirname "$0")" && pwd)"

if [ "$target" = "anthropic" ]; then
  key_env="${AIGENTIC_API_KEY_ENV:-ANTHROPIC_API_KEY}"
  key="${!key_env:?environment variable $key_env is not set}"
  dir="$here/anthropic"
  mkdir -p "$dir"
  # A system prompt above the 512-token cache minimum, so the second request
  # of each pair reads what the first wrote. tool_calls is recorded second.
  sys="$(printf 'You are a terse coding agent. Rule %d: answer precisely, prefer tools over memory, never guess file contents. ' $(seq 1 60))"
  post_a() { # name, json body
    curl -sS -N --fail-with-body \
      -H "x-api-key: $key" -H "anthropic-version: 2023-06-01" \
      -H "Content-Type: application/json" \
      -d "$2" https://api.anthropic.com/v1/messages > "$dir/$1.sse"
    printf '%s: %s bytes, %s events\n' "$1" "$(wc -c < "$dir/$1.sse" | tr -d ' ')" "$(grep -c '^event:' "$dir/$1.sse")"
  }
  atools='[{"name":"read_file","description":"Read a UTF-8 text file","input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},{"name":"bash","description":"Run a shell command","input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}]'
  post_a text "$(cat <<JSON
{"model":"$model","max_tokens":4096,"stream":true,"thinking":{"type":"adaptive"},
 "system":[{"type":"text","text":"$sys","cache_control":{"type":"ephemeral"}}],
 "messages":[{"role":"user","content":[{"type":"text","text":"Reply with exactly: Hello, world.","cache_control":{"type":"ephemeral"}}]}]}
JSON
)"
  post_a tool_calls "$(cat <<JSON
{"model":"$model","max_tokens":4096,"stream":true,"thinking":{"type":"adaptive"},"tools":$atools,
 "system":[{"type":"text","text":"$sys","cache_control":{"type":"ephemeral"}}],
 "messages":[{"role":"user","content":[{"type":"text","text":"Read Cargo.toml and then list the current directory with ls -la. Call both tools now.","cache_control":{"type":"ephemeral"}}]}]}
JSON
)"
  post_a max_tokens "$(cat <<JSON
{"model":"$model","max_tokens":8,"stream":true,
 "messages":[{"role":"user","content":"Write three paragraphs about the sea."}]}
JSON
)"
  exit 0
fi

base_url="$target"
key_env="${AIGENTIC_API_KEY_ENV:-TENSORX_API_KEY}"
key="${!key_env:?environment variable $key_env is not set}"
dir="$here/openai_compat"
mkdir -p "$dir"

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
