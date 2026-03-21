#!/bin/bash
# Load test: runs concurrent streaming requests against the proxy.
#
# Usage:
#   ./tests/e2e/test_load.sh [proxy_url] [concurrency]
#
# Default proxy_url: http://127.0.0.1:8181
# Default concurrency: 5

set -euo pipefail

PROXY_URL="${1:-http://127.0.0.1:8181}"
CONCURRENCY="${2:-5}"

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

TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT

echo "=== Load Test ==="
echo "Proxy:       $PROXY_URL"
echo "Concurrency: $CONCURRENCY"
echo ""

send_request() {
    local id="$1"
    local outfile="$TMPDIR/result_${id}"
    local http_code
    http_code=$(curl -s -o "$outfile" -w "%{http_code}" -N -X POST "$PROXY_URL/v1/messages" \
        -H "content-type: application/json" \
        -H "x-api-key: $API_KEY" \
        -H "anthropic-version: 2023-06-01" \
        -d '{
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "Say hello in one word."}]
        }' 2>/dev/null)
    echo "$http_code" > "$TMPDIR/status_${id}"
}

START_TIME=$(date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")

# Launch concurrent requests
for i in $(seq 1 "$CONCURRENCY"); do
    send_request "$i" &
done

# Wait for all to complete
wait

END_TIME=$(date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")

# Calculate results
SUCCEEDED=0
FAILED=0
for i in $(seq 1 "$CONCURRENCY"); do
    status_file="$TMPDIR/status_${i}"
    if [ -f "$status_file" ]; then
        code=$(cat "$status_file")
        if [ "$code" = "200" ]; then
            SUCCEEDED=$((SUCCEEDED + 1))
        else
            FAILED=$((FAILED + 1))
            echo "  Request $i failed with HTTP $code"
        fi
    else
        FAILED=$((FAILED + 1))
        echo "  Request $i: no status file (curl may have crashed)"
    fi
done

# Calculate elapsed time
ELAPSED_NS=$((END_TIME - START_TIME))
# Use python3 for float division (portable)
ELAPSED_SEC=$(python3 -c "print(f'{$ELAPSED_NS / 1e9:.2f}')")
RPS=$(python3 -c "elapsed = $ELAPSED_NS / 1e9; print(f'{$CONCURRENCY / elapsed:.2f}' if elapsed > 0 else 'N/A')")

echo ""
echo "=== Results ==="
echo "Total requests: $CONCURRENCY"
echo "Succeeded:      $SUCCEEDED"
echo "Failed:         $FAILED"
echo "Total time:     ${ELAPSED_SEC}s"
echo "Requests/sec:   $RPS"

[ $FAILED -eq 0 ] && exit 0 || exit 1
