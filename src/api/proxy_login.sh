#!/bin/bash
# apiKeyHelper for Claude Code — browser-based OIDC login.
# Works with any IDP configured on the proxy (Okta, Azure AD, etc.)
#
# Flow:
#   1. Check for cached token (valid JWT not yet expired)
#   2. If expired/missing, open browser for IDP login (with lock to prevent tab flood)
#   3. Poll proxy until login completes
#   4. Cache token alongside the script (mode 600)
#
# Usage: proxy-login.sh [host]   (host passed by apiKeyHelper, defaults to env/hardcoded)
# Installed by: curl -s https://<proxy>/auth/setup | bash

set -euo pipefail

# Host can be passed as $1 (from apiKeyHelper), env var, or default
PROXY_HOST="${1:-${CC_PROXY_HOST:-localhost}}"
PROXY_URL="https://${PROXY_HOST}"
# Token storage: always under ~/.claude/tokens/ (avoids git concerns)
# Token files are scoped by host to prevent cross-environment token reuse.
# Project-scoped scripts also include a project slug.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOKEN_DIR="${HOME}/.claude/tokens"
HOST_SLUG=$(echo "$PROXY_HOST" | tr '.:/' '-')
mkdir -p "${TOKEN_DIR}"
if [ "$SCRIPT_DIR" = "${HOME}/.claude" ]; then
    # User-scoped: token file per host
    TOKEN_FILE="${TOKEN_DIR}/proxy-token.${HOST_SLUG}"
else
    # Project-scoped: slug from project directory + host
    PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
    PROJECT_SLUG=$(echo "$PROJECT_DIR" | sed "s|${HOME}/||" | tr '/' '-')
    TOKEN_FILE="${TOKEN_DIR}/proxy-token.${HOST_SLUG}.${PROJECT_SLUG}"
fi
LOCK_FILE="${TOKEN_FILE}.lock"
FAIL_FILE="${TOKEN_FILE}.fail"
# Refresh 60s before expiry to avoid mid-request failures
TOKEN_REFRESH_MARGIN=60
# Don't retry browser flow within this window after a failure
FAIL_COOLDOWN_SECS=30

# Decode JWT payload and extract a field (no external deps)
jwt_field() {
    local token="$1" field="$2"
    local payload
    payload=$(echo "$token" | cut -d. -f2)
    local pad=$(( 4 - ${#payload} % 4 ))
    [ "$pad" -lt 4 ] && payload="${payload}$(printf '%0.s=' $(seq 1 $pad))"
    echo "$payload" | base64 -d 2>/dev/null | grep -o "\"${field}\":[0-9]*" | cut -d: -f2
}

# Check if a cached token is still valid
token_valid() {
    local token="$1"
    [ -z "$token" ] && return 1
    local exp
    exp=$(jwt_field "$token" "exp")
    [ -z "$exp" ] && return 1
    local now
    now=$(date +%s)
    [ "$now" -lt $(( exp - TOKEN_REFRESH_MARGIN )) ]
}

# Try cached token first
CACHED_TOKEN=""
if [ -f "$TOKEN_FILE" ]; then
    CACHED_TOKEN=$(cat "$TOKEN_FILE")
    if token_valid "$CACHED_TOKEN"; then
        echo "$CACHED_TOKEN"
        exit 0
    fi
fi

# Check if we recently failed — don't spam browser tabs
if [ -f "$FAIL_FILE" ]; then
    if [[ "$OSTYPE" == "darwin"* ]]; then
        fail_age=$(( $(date +%s) - $(stat -f%m "$FAIL_FILE") ))
    else
        fail_age=$(( $(date +%s) - $(stat -c%Y "$FAIL_FILE") ))
    fi
    if [ "$fail_age" -lt "$FAIL_COOLDOWN_SECS" ]; then
        # Still output cached token if we have one — let the gateway decide
        # if it's truly invalid rather than sending CC an empty string
        [ -n "$CACHED_TOKEN" ] && echo "$CACHED_TOKEN"
        exit 1
    fi
    rm -f "$FAIL_FILE"
fi

# Lock to prevent multiple concurrent browser flows.
if mkdir "$LOCK_FILE" 2>/dev/null; then
    trap 'rmdir "$LOCK_FILE" 2>/dev/null' EXIT
else
    # Another instance holds the lock — wait for token file
    for i in $(seq 1 60); do
        if [ -f "$TOKEN_FILE" ]; then
            TOKEN=$(cat "$TOKEN_FILE")
            if token_valid "$TOKEN"; then
                echo "$TOKEN"
                exit 0
            fi
        fi
        if ! [ -d "$LOCK_FILE" ]; then
            [ -n "$CACHED_TOKEN" ] && echo "$CACHED_TOKEN"
            exit 1
        fi
        sleep 2
    done
    [ -n "$CACHED_TOKEN" ] && echo "$CACHED_TOKEN"
    exit 1
fi

# Start browser login flow
SESSION_ID=$(uuidgen | tr '[:upper:]' '[:lower:]')
AUTH_URL="${PROXY_URL}/auth/cli/login?session=${SESSION_ID}"

if [[ "$OSTYPE" == "darwin"* ]]; then
    open "$AUTH_URL" 2>/dev/null
elif command -v xdg-open &>/dev/null; then
    xdg-open "$AUTH_URL" 2>/dev/null
elif command -v wslview &>/dev/null; then
    wslview "$AUTH_URL" 2>/dev/null
fi

# Poll for token (2s interval, 2 min timeout)
for i in $(seq 1 60); do
    RESPONSE=$(curl -sk "${PROXY_URL}/auth/cli/poll?session=${SESSION_ID}" 2>/dev/null || echo '{}')
    STATUS=$(echo "$RESPONSE" | grep -o '"status":"[^"]*"' | head -1 | cut -d'"' -f4)

    case "$STATUS" in
        complete)
            TOKEN=$(echo "$RESPONSE" | sed 's/.*"token":"//' | sed 's/".*//')
            if [ -n "$TOKEN" ] && [ "$TOKEN" != "$RESPONSE" ]; then
                mkdir -p "$(dirname "$TOKEN_FILE")"
                echo -n "$TOKEN" > "$TOKEN_FILE"
                chmod 600 "$TOKEN_FILE"
                rm -f "$FAIL_FILE"
                echo "$TOKEN"
                exit 0
            fi
            touch "$FAIL_FILE"
            [ -n "$CACHED_TOKEN" ] && echo "$CACHED_TOKEN"
            exit 1
            ;;
        expired|not_found)
            touch "$FAIL_FILE"
            [ -n "$CACHED_TOKEN" ] && echo "$CACHED_TOKEN"
            exit 1
            ;;
        pending|"")
            sleep 2
            ;;
        *)
            sleep 2
            ;;
    esac
done

touch "$FAIL_FILE"
[ -n "$CACHED_TOKEN" ] && echo "$CACHED_TOKEN"
exit 1
