#!/bin/bash
# E2E compatibility test: sends requests shaped exactly like Claude Code does.
# This avoids the nested-session guard by using curl instead of the claude CLI.
#
# Usage:
#   ./tests/e2e/test_cc_compat.sh [proxy_url]
#
# Default proxy_url: http://127.0.0.1:8181

set -euo pipefail

PROXY_URL="${1:-http://127.0.0.1:8181}"
PASS=0
FAIL=0

# Obtain a session token via admin login
ADMIN_USER="${ADMIN_USERNAME:-admin}"
ADMIN_PASS="${ADMIN_PASSWORD:-admin}"
LOGIN_RESP=$(curl -sf -X POST "$PROXY_URL/auth/login" \
    -H "content-type: application/json" \
    -d "{\"username\": \"$ADMIN_USER\", \"password\": \"$ADMIN_PASS\"}" 2>&1) || {
    echo "ERROR: Failed to login at $PROXY_URL/auth/login"
    exit 1
}
API_KEY=$(echo "$LOGIN_RESP" | grep -o '"token":"[^"]*"' | cut -d'"' -f4)
if [ -z "$API_KEY" ]; then
    echo "ERROR: No token in login response: $LOGIN_RESP"
    exit 1
fi
echo "Authenticated via /auth/login"

run_test() {
    local name="$1"
    local expected="$2"
    local actual="$3"

    if echo "$actual" | grep -q "$expected"; then
        echo "  PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $name"
        echo "    Expected to find: $expected"
        echo "    Got: $(echo "$actual" | head -5)"
        FAIL=$((FAIL + 1))
    fi
}

echo "=== Claude Code Compatibility Tests ==="
echo "Proxy: $PROXY_URL"
echo ""

# Test 1: Health
echo "[1/14] Health check"
RESP=$(curl -sf "$PROXY_URL/health")
run_test "health returns ok" "ok" "$RESP"

# Test 2: Simple non-streaming (CC-style headers + betas)
echo "[2/14] Non-streaming with CC-style beta headers"
RESP=$(curl -sf -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -H "anthropic-beta: claude-code-20250219,adaptive-thinking-2026-01-28,context-management-2025-06-27,prompt-caching-scope-2026-01-05,effort-2025-11-24" \
    -d '{
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 128,
        "messages": [{"role": "user", "content": "Say exactly: cc-compat-ok"}]
    }' 2>&1)
run_test "response has content" '"type":"text"' "$RESP"
run_test "response has model" '"model"' "$RESP"
run_test "response has usage" '"usage"' "$RESP"

# Test 3: Streaming with CC-style headers
echo "[3/14] Streaming with CC-style beta headers"
STREAM=$(curl -sf -N -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -H "anthropic-beta: claude-code-20250219,adaptive-thinking-2026-01-28,effort-2025-11-24" \
    -d '{
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 128,
        "stream": true,
        "messages": [{"role": "user", "content": "Say exactly: stream-compat-ok"}]
    }' 2>&1)
run_test "SSE message_start event" "event: message_start" "$STREAM"
run_test "SSE content_block_delta" "event: content_block_delta" "$STREAM"
run_test "SSE message_stop event" "event: message_stop" "$STREAM"

# Test 4: Thinking mode (CC uses this by default)
echo "[4/14] Extended thinking"
RESP=$(curl -sf -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -H "anthropic-beta: claude-code-20250219,interleaved-thinking-2025-05-14" \
    -d '{
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 4096,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [{"role": "user", "content": "What is 17 * 23?"}]
    }' 2>&1)
run_test "response has thinking block" '"type":"thinking"' "$RESP"
run_test "response has text block" '"type":"text"' "$RESP"

# Test 5: System message with cache_control (CC always sends these)
echo "[5/14] System message with cache_control (should be stripped)"
RESP=$(curl -sf -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -H "anthropic-beta: claude-code-20250219" \
    -d '{
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 128,
        "system": [{"type": "text", "text": "You are helpful.", "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "Say: cache-ok", "cache_control": {"type": "ephemeral"}}]}]
    }' 2>&1)
run_test "cache_control stripped successfully" '"type":"message"' "$RESP"

# Test 6: Token counting
echo "[6/14] Token counting"
RESP=$(curl -sf -X POST "$PROXY_URL/v1/messages/count_tokens" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -d '{"model": "claude-haiku-4-5-20251001", "messages": [{"role": "user", "content": "hello world"}]}' 2>&1)
run_test "returns input_tokens" "input_tokens" "$RESP"

# Test 7: Admin API (if DB is configured, otherwise skip)
echo "[7/14] Admin API (virtual keys)"
ADMIN_RESP=$(curl -sf -X POST "$PROXY_URL/admin/keys" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -d '{"name": "e2e-test-key"}' 2>&1) || ADMIN_RESP="SKIP"

if [ "$ADMIN_RESP" = "SKIP" ]; then
    echo "  SKIP: Admin API not available (no database?)"
else
    run_test "create key returns key" '"key":"sk-proxy-' "$ADMIN_RESP"
    run_test "create key returns id" '"id"' "$ADMIN_RESP"

    # Test using the virtual key for a request
    # Wait for key cache to refresh across instances (polls every 5s)
    sleep 6
    VKEY=$(echo "$ADMIN_RESP" | grep -o '"key":"[^"]*"' | head -1 | cut -d'"' -f4)
    if [ -n "$VKEY" ]; then
        VKEY_RESP=$(curl -sf -X POST "$PROXY_URL/v1/messages" \
            -H "content-type: application/json" \
            -H "x-api-key: $VKEY" \
            -H "anthropic-version: 2023-06-01" \
            -d '{
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32,
                "messages": [{"role": "user", "content": "Say: vkey-ok"}]
            }' 2>&1)
        run_test "virtual key auth works" '"type":"text"' "$VKEY_RESP"
    fi
fi

# Test 8: Rate limiting (create a key with rate limit, then exceed it)
echo "[8/14] Rate limiting"
if [ "$ADMIN_RESP" != "SKIP" ]; then
    RL_RESP=$(curl -sf -X POST "$PROXY_URL/admin/keys" \
        -H "content-type: application/json" \
        -H "x-api-key: $API_KEY" \
        -d '{"name": "rate-limited-key", "rate_limit_rpm": 2}' 2>&1) || RL_RESP="SKIP"

    if [ "$RL_RESP" != "SKIP" ]; then
        RL_KEY=$(echo "$RL_RESP" | grep -o '"key":"[^"]*"' | head -1 | cut -d'"' -f4)
        # Wait for key cache to refresh across instances
        sleep 6
        # Use up the 2 RPM limit with count_tokens (cheap, no Bedrock call)
        curl -sf -X POST "$PROXY_URL/v1/messages/count_tokens" \
            -H "content-type: application/json" -H "x-api-key: $RL_KEY" \
            -d '{"model": "x", "messages": [{"role":"user","content":"a"}]}' > /dev/null 2>&1
        curl -sf -X POST "$PROXY_URL/v1/messages/count_tokens" \
            -H "content-type: application/json" -H "x-api-key: $RL_KEY" \
            -d '{"model": "x", "messages": [{"role":"user","content":"b"}]}' > /dev/null 2>&1
        # Third request should be rate limited
        RL_CHECK=$(curl -s -X POST "$PROXY_URL/v1/messages/count_tokens" \
            -H "content-type: application/json" -H "x-api-key: $RL_KEY" \
            -d '{"model": "x", "messages": [{"role":"user","content":"c"}]}' 2>&1)
        run_test "rate limit enforced" "rate_limit_error" "$RL_CHECK"
    else
        echo "  SKIP: Could not create rate-limited key"
    fi
else
    echo "  SKIP: No database"
fi

# Test 9: Auth rejection
echo "[9/14] Authentication"
RESP=$(curl -s -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: bad-key" \
    -d '{"model": "x", "messages": []}' 2>&1)
run_test "rejects bad key" "authentication_error" "$RESP"

# Test 10: Multi-turn conversation
echo "[10/14] Multi-turn conversation"
RESP=$(curl -sf -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -d '{
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 128,
        "messages": [
            {"role": "user", "content": "My favorite color is blue. Remember that."},
            {"role": "assistant", "content": "Got it! Your favorite color is blue."},
            {"role": "user", "content": "What is my favorite color? Say just the color."}
        ]
    }' 2>&1)
run_test "multi-turn has content" '"type":"text"' "$RESP"
run_test "multi-turn has usage" '"usage"' "$RESP"

# Test 11: Streaming content validation
echo "[11/14] Streaming content validation"
STREAM=$(curl -sf -N -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -d '{
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 128,
        "stream": true,
        "messages": [{"role": "user", "content": "Say exactly: validation-ok"}]
    }' 2>&1)
run_test "message_start has input_tokens" "input_tokens" "$(echo "$STREAM" | grep 'event: message_start' -A1)"
run_test "message_delta has output_tokens" "output_tokens" "$(echo "$STREAM" | grep 'event: message_delta' -A1)"
# Verify SSE format: each event: line should be followed by a data: line
SSE_VALID="true"
while IFS= read -r line; do
    if echo "$line" | grep -q '^event:'; then
        # Read the next non-empty line and check it starts with data:
        read -r next_line
        if ! echo "$next_line" | grep -q '^data:'; then
            SSE_VALID="false"
            break
        fi
    fi
done <<< "$STREAM"
run_test "SSE format (event: followed by data:)" "true" "$SSE_VALID"

# Test 12: Tool use request
echo "[12/14] Tool use request"
RESP=$(curl -sf -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -d '{
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 256,
        "tools": [{
            "name": "get_weather",
            "description": "Get the current weather in a given location",
            "input_schema": {
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The city name, e.g. Sydney"
                    }
                },
                "required": ["location"]
            }
        }],
        "messages": [{"role": "user", "content": "What is the weather in Sydney? Use the get_weather tool."}]
    }' 2>&1)
run_test "tool use response has tool_use block" "tool_use" "$RESP"
run_test "tool use response mentions get_weather" "get_weather" "$RESP"

# Test 13: Large context
echo "[13/14] Large context handling"
LARGE_PAYLOAD=$(python3 -c "
import json
large_text = 'This is a repeated sentence for testing large context handling. ' * 50
print(json.dumps({
    'model': 'claude-haiku-4-5-20251001',
    'max_tokens': 64,
    'system': large_text,
    'messages': [{'role': 'user', 'content': 'Say exactly: large-ok'}]
}))")
RESP=$(curl -sf -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -d "$LARGE_PAYLOAD" 2>&1)
run_test "large context succeeds" '"type":"message"' "$RESP"

# Test 14: Error handling (invalid model)
echo "[14/14] Error handling (invalid model)"
RESP=$(curl -s -X POST "$PROXY_URL/v1/messages" \
    -H "content-type: application/json" \
    -H "x-api-key: $API_KEY" \
    -H "anthropic-version: 2023-06-01" \
    -d '{
        "model": "totally-invalid-model-name-12345",
        "max_tokens": 128,
        "messages": [{"role": "user", "content": "hello"}]
    }' 2>&1)
# Should get an error response, not a 500 crash
run_test "invalid model returns error type" '"type":"error"' "$RESP"
run_test "invalid model does not crash (has error field)" '"error"' "$RESP"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[ $FAIL -eq 0 ] && exit 0 || exit 1
