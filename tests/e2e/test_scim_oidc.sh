#!/bin/bash
# Full-loop SCIM + OIDC integration test with Keycloak.
# Tests that SCIM-provisioned users can authenticate via real OIDC JWTs,
# and that SCIM deactivation blocks OIDC access.
#
# Prerequisites:
#   docker compose -f tests/e2e/keycloak-scim/docker-compose.yml up -d
#   # Wait for all services to be healthy (keycloak ~60s, gateway ~10s)
#
# Usage:
#   ./tests/e2e/test_scim_oidc.sh
#
# Environment overrides:
#   GATEWAY_URL   - default: http://localhost:8181
#   KEYCLOAK_URL  - default: http://localhost:9090
#
# NOTE: The CCAG gateway enforces HTTPS for JWKS fetching but allows http://localhost
# and http://127.0.0.1 for local testing. The IDP is therefore registered with:
#   - issuer_url:  http://localhost:9090/realms/ccag-test  (matches JWT iss claim)
#   - jwks_url:    http://localhost:9090/realms/ccag-test/protocol/openid-connect/certs
# Both URLs reach the Keycloak container via the host-mapped port 9090.
# Tokens are fetched from the same host-accessible issuer URL so the iss claim matches.

set -euo pipefail

GATEWAY_URL="${GATEWAY_URL:-http://localhost:8181}"
KEYCLOAK_URL="${KEYCLOAK_URL:-http://localhost:9090}"
KC_REALM="ccag-test"
KC_CLIENT="ccag-gateway"

PASS=0
FAIL=0

# Check for required tools
if ! command -v jq &> /dev/null; then
    echo "ERROR: jq is required but not installed."
    exit 1
fi

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
        echo "    Got: $(echo "$actual" | head -3)"
        FAIL=$((FAIL + 1))
    fi
}

run_status_test() {
    local name="$1"
    local expected="$2"
    local actual="$3"
    if [ "$actual" = "$expected" ]; then
        echo "  PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $name (expected HTTP $expected, got $actual)"
        FAIL=$((FAIL + 1))
    fi
}

# Helper: make a SCIM curl request, output body then status on last line
# Usage: resp=$(scim_curl POST /scim/v2/Users -d '...')
#        body=$(get_body "$resp")
#        status=$(get_status "$resp")
scim_curl() {
    local method="$1"
    local path="$2"
    shift 2
    curl -s -w '\n%{http_code}' -X "$method" "$GATEWAY_URL$path" \
        -H "Authorization: Bearer $SCIM_TOKEN" \
        -H "Content-Type: application/scim+json" \
        "$@"
}

# Helper: admin curl — uses gateway session token in x-api-key header
admin_curl() {
    local method="$1"
    local path="$2"
    shift 2
    curl -s -w '\n%{http_code}' -X "$method" "$GATEWAY_URL$path" \
        -H "x-api-key: $API_KEY" \
        -H "Content-Type: application/json" \
        "$@"
}

get_body() { echo "$1" | sed '$d'; }
get_status() { echo "$1" | tail -1; }

# Helper: get an OIDC access token from Keycloak via resource owner password credentials grant.
# Keycloak must have directAccessGrantsEnabled=true on the client (see realm-export.json).
get_keycloak_token() {
    local username="$1"
    local password="$2"
    local token
    token=$(curl -s -X POST "$KEYCLOAK_URL/realms/$KC_REALM/protocol/openid-connect/token" \
        -d "grant_type=password" \
        -d "client_id=$KC_CLIENT" \
        -d "username=$username" \
        -d "password=$password" \
        -d "scope=openid email" \
        | jq -r '.access_token // empty')
    echo "$token"
}

echo "=== SCIM + OIDC Full-Loop Integration Test ==="
echo "Gateway:  $GATEWAY_URL"
echo "Keycloak: $KEYCLOAK_URL"
echo ""

# State variables (initialized empty to avoid -u unbound errors)
IDP_ID=""
SCIM_TOKEN=""
API_KEY=""
ALICE_USER_ID=""
BOB_USER_ID=""
GROUP_ID=""

# ============================================================
# [Phase 0] Service health checks
# ============================================================

echo "[Phase 0] Service health checks"

echo "  [1/3] Gateway health"
GW_STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$GATEWAY_URL/health")
run_status_test "Gateway /health returns 200" "200" "$GW_STATUS"

echo "  [2/3] Keycloak realm reachable"
KC_STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$KEYCLOAK_URL/realms/$KC_REALM")
run_status_test "Keycloak realm returns 200" "200" "$KC_STATUS"

echo "  [3/3] Keycloak issues JWT for alice"
ALICE_TOKEN=$(get_keycloak_token "alice" "alice-pass")
if [ -z "$ALICE_TOKEN" ]; then
    echo "  FAIL: get_keycloak_token returned empty — Keycloak not ready or credentials wrong"
    FAIL=$((FAIL + 1))
else
    # JWTs are base64url-encoded and always start with "ey"
    run_test "Keycloak issues JWT for alice" "^ey" "$ALICE_TOKEN"
fi

echo ""

# ============================================================
# [Phase 1] Setup: register Keycloak as IDP with SCIM
# ============================================================

echo "[Phase 1] Setup: register Keycloak as IDP with SCIM"

echo "  [1/4] Admin login"
ADMIN_USER="${ADMIN_USERNAME:-admin}"
ADMIN_PASS="${ADMIN_PASSWORD:-admin}"
LOGIN_RESP=$(curl -sf -X POST "$GATEWAY_URL/auth/login" \
    -H "Content-Type: application/json" \
    -d "{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}" 2>&1) || {
    echo "ERROR: Failed to login at $GATEWAY_URL/auth/login"
    exit 1
}
API_KEY=$(echo "$LOGIN_RESP" | jq -r '.token // empty')
if [ -z "$API_KEY" ]; then
    echo "ERROR: No token in login response: $LOGIN_RESP"
    exit 1
fi
echo "  Authenticated via /auth/login"

echo "  [2/4] Create Keycloak IDP"
# issuer_url matches the JWT iss claim (Keycloak embeds the request Host in iss).
# jwks_url is set explicitly to the direct JWKS endpoint so the gateway can fetch
# it via http://localhost:9090 (allowed by the HTTPS enforcement allowlist).
IDP_RESP=$(curl -sf -X POST "$GATEWAY_URL/admin/idps" \
    -H "x-api-key: $API_KEY" \
    -H "Content-Type: application/json" \
    -d "{
        \"name\": \"keycloak-scim-test\",
        \"issuer_url\": \"$KEYCLOAK_URL/realms/$KC_REALM\",
        \"audience\": \"$KC_CLIENT\",
        \"jwks_url\": \"$KEYCLOAK_URL/realms/$KC_REALM/protocol/openid-connect/certs\",
        \"flow_type\": \"authorization_code\",
        \"auto_provision\": false,
        \"default_role\": \"member\"
    }" 2>&1) || {
    echo "ERROR: Failed to create IDP: $IDP_RESP"
    exit 1
}
IDP_ID=$(echo "$IDP_RESP" | jq -r '.id // empty')
if [ -z "$IDP_ID" ]; then
    echo "ERROR: Could not extract IDP ID from: $IDP_RESP"
    exit 1
fi
run_test "IDP created with id" "$IDP_ID" "$IDP_ID"
echo "  Created IDP: $IDP_ID"

echo "  [3/4] Enable SCIM for IDP"
SCIM_ENABLE_RESP=$(curl -sf -X PUT "$GATEWAY_URL/admin/idps/$IDP_ID/scim" \
    -H "x-api-key: $API_KEY" \
    -H "Content-Type: application/json" \
    -d '{"enabled":true}' 2>&1) || {
    echo "ERROR: Failed to enable SCIM: $SCIM_ENABLE_RESP"
    exit 1
}
echo "  SCIM enabled"

echo "  [4/4] Create SCIM token"
TOKEN_RESP=$(curl -sf -X POST "$GATEWAY_URL/admin/idps/$IDP_ID/scim-tokens" \
    -H "x-api-key: $API_KEY" \
    -H "Content-Type: application/json" \
    -d '{"name":"keycloak-e2e"}' 2>&1) || {
    echo "ERROR: Failed to create SCIM token: $TOKEN_RESP"
    exit 1
}
SCIM_TOKEN=$(echo "$TOKEN_RESP" | jq -r '.token // empty')
if [ -z "$SCIM_TOKEN" ]; then
    echo "ERROR: Could not extract SCIM token from: $TOKEN_RESP"
    exit 1
fi
run_test "SCIM token has expected prefix" "scim-ccag-" "$SCIM_TOKEN"
echo "  SCIM token obtained (prefix: ${SCIM_TOKEN:0:16}...)"

# Wait for gateway cache to reload with the new IDP (polls every 5s)
echo "  Waiting for gateway to pick up new IDP..."
for i in $(seq 1 12); do
    # Try to validate a Keycloak token — if it gets past 401 "Invalid API key",
    # the IDP has been loaded (even 403 "not provisioned" means the IDP is active)
    TEST_TOKEN=$(get_keycloak_token "alice" "alice-pass")
    STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X GET "$GATEWAY_URL/auth/me" \
        -H "Authorization: Bearer $TEST_TOKEN")
    if [ "$STATUS" != "401" ]; then
        echo "  IDP loaded after $((i * 2))s (status=$STATUS)"
        break
    fi
    sleep 2
done

echo ""

# ============================================================
# [Phase 2] OIDC login WITHOUT SCIM provisioning → expect 403
# ============================================================

echo "[Phase 2] OIDC login WITHOUT SCIM provisioning"

ALICE_TOKEN=$(get_keycloak_token "alice" "alice-pass")
if [ -z "$ALICE_TOKEN" ]; then
    echo "  SKIP: Could not get alice token from Keycloak"
    FAIL=$((FAIL + 2))
else
    RESP=$(curl -s -w '\n%{http_code}' -X GET "$GATEWAY_URL/auth/me" \
        -H "Authorization: Bearer $ALICE_TOKEN")
    STATUS=$(get_status "$RESP")
    BODY=$(get_body "$RESP")
    run_status_test "Unprovisioned user gets 403" "403" "$STATUS"
    run_test "Error mentions not provisioned" "not provisioned" "$BODY"
fi

echo ""

# ============================================================
# [Phase 3] Provision alice via SCIM → OIDC login should succeed
# ============================================================

echo "[Phase 3] Provision alice via SCIM → OIDC login should succeed"

echo "  [1/3] SCIM create alice"
ALICE_CREATE_RESP=$(scim_curl POST /scim/v2/Users \
    -d "{
        \"schemas\": [\"urn:ietf:params:scim:schemas:core:2.0:User\"],
        \"userName\": \"alice@ccag-test.example.com\",
        \"externalId\": \"alice-keycloak-id\",
        \"name\": {\"givenName\": \"Alice\", \"familyName\": \"Test\"},
        \"active\": true
    }")
ALICE_CREATE_STATUS=$(get_status "$ALICE_CREATE_RESP")
ALICE_CREATE_BODY=$(get_body "$ALICE_CREATE_RESP")
run_status_test "SCIM create alice returns 201" "201" "$ALICE_CREATE_STATUS"
ALICE_USER_ID=$(echo "$ALICE_CREATE_BODY" | jq -r '.id // empty')
if [ -z "$ALICE_USER_ID" ]; then
    echo "  ERROR: Could not extract alice user ID — skipping OIDC login tests for alice"
    FAIL=$((FAIL + 2))
else
    echo "  Alice provisioned: $ALICE_USER_ID"

    echo "  [2/3] OIDC login with provisioned alice → 200"
    ALICE_TOKEN=$(get_keycloak_token "alice" "alice-pass")
    RESP=$(curl -s -w '\n%{http_code}' -X GET "$GATEWAY_URL/auth/me" \
        -H "Authorization: Bearer $ALICE_TOKEN")
    STATUS=$(get_status "$RESP")
    BODY=$(get_body "$RESP")
    run_status_test "Provisioned alice gets 200" "200" "$STATUS"
    run_test "Response contains alice email" "alice@ccag-test.example.com" "$BODY"
fi

echo ""

# ============================================================
# [Phase 4] Deactivate alice via SCIM → OIDC login should fail
# ============================================================

echo "[Phase 4] Deactivate alice via SCIM → OIDC login should fail"

if [ -z "$ALICE_USER_ID" ]; then
    echo "  SKIP: No alice user ID available"
    FAIL=$((FAIL + 2))
else
    echo "  [1/2] SCIM PATCH deactivate alice (Entra-style string 'False')"
    DEACT_RESP=$(scim_curl PATCH "/scim/v2/Users/$ALICE_USER_ID" \
        -d '{"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"Replace","path":"active","value":"False"}]}')
    DEACT_STATUS=$(get_status "$DEACT_RESP")
    run_status_test "Deactivate alice returns 200" "200" "$DEACT_STATUS"

    echo "  [2/2] OIDC login with deactivated alice → 403"
    ALICE_TOKEN=$(get_keycloak_token "alice" "alice-pass")
    RESP=$(curl -s -w '\n%{http_code}' -X GET "$GATEWAY_URL/auth/me" \
        -H "Authorization: Bearer $ALICE_TOKEN")
    STATUS=$(get_status "$RESP")
    BODY=$(get_body "$RESP")
    run_status_test "Deactivated alice gets 403" "403" "$STATUS"
    run_test "Error mentions deactivated" "deactivated" "$BODY"
fi

echo ""

# ============================================================
# [Phase 5] Reactivate alice via SCIM → OIDC login should succeed again
# ============================================================

echo "[Phase 5] Reactivate alice via SCIM → OIDC login should succeed again"

if [ -z "$ALICE_USER_ID" ]; then
    echo "  SKIP: No alice user ID available"
    FAIL=$((FAIL + 2))
else
    echo "  [1/2] SCIM PATCH reactivate alice (boolean true)"
    REACT_RESP=$(scim_curl PATCH "/scim/v2/Users/$ALICE_USER_ID" \
        -d '{"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"Replace","path":"active","value":true}]}')
    REACT_STATUS=$(get_status "$REACT_RESP")
    run_status_test "Reactivate alice returns 200" "200" "$REACT_STATUS"

    echo "  [2/2] OIDC login with reactivated alice → 200"
    ALICE_TOKEN=$(get_keycloak_token "alice" "alice-pass")
    RESP=$(curl -s -w '\n%{http_code}' -X GET "$GATEWAY_URL/auth/me" \
        -H "Authorization: Bearer $ALICE_TOKEN")
    STATUS=$(get_status "$RESP")
    run_status_test "Reactivated alice gets 200" "200" "$STATUS"
fi

echo ""

# ============================================================
# [Phase 6] Provision bob + group assignment
# ============================================================

echo "[Phase 6] Provision bob + group assignment"

echo "  [1/4] SCIM create bob"
BOB_CREATE_RESP=$(scim_curl POST /scim/v2/Users \
    -d "{
        \"schemas\": [\"urn:ietf:params:scim:schemas:core:2.0:User\"],
        \"userName\": \"bob@ccag-test.example.com\",
        \"externalId\": \"bob-keycloak-id\",
        \"name\": {\"givenName\": \"Bob\", \"familyName\": \"Test\"},
        \"active\": true
    }")
BOB_CREATE_STATUS=$(get_status "$BOB_CREATE_RESP")
BOB_CREATE_BODY=$(get_body "$BOB_CREATE_RESP")
run_status_test "SCIM create bob returns 201" "201" "$BOB_CREATE_STATUS"
BOB_USER_ID=$(echo "$BOB_CREATE_BODY" | jq -r '.id // empty')
if [ -z "$BOB_USER_ID" ]; then
    echo "  ERROR: Could not extract bob user ID — skipping group tests"
    FAIL=$((FAIL + 3))
else
    echo "  Bob provisioned: $BOB_USER_ID"

    echo "  [2/4] Create SCIM group with alice and bob as members"
    GROUP_CREATE_RESP=$(scim_curl POST /scim/v2/Groups \
        -d "{
            \"schemas\": [\"urn:ietf:params:scim:schemas:core:2.0:Group\"],
            \"displayName\": \"Engineering\",
            \"externalId\": \"eng-group-keycloak\",
            \"members\": [{\"value\": \"$ALICE_USER_ID\"}, {\"value\": \"$BOB_USER_ID\"}]
        }")
    GROUP_CREATE_STATUS=$(get_status "$GROUP_CREATE_RESP")
    GROUP_CREATE_BODY=$(get_body "$GROUP_CREATE_RESP")
    run_status_test "SCIM create group returns 201" "201" "$GROUP_CREATE_STATUS"
    GROUP_ID=$(echo "$GROUP_CREATE_BODY" | jq -r '.id // empty')
    if [ -z "$GROUP_ID" ]; then
        echo "  ERROR: Could not extract group ID"
        FAIL=$((FAIL + 2))
    else
        echo "  Group created: $GROUP_ID"

        echo "  [3/4] Bob OIDC login succeeds (provisioned)"
        BOB_TOKEN=$(get_keycloak_token "bob" "bob-pass")
        if [ -z "$BOB_TOKEN" ]; then
            echo "  SKIP: Could not get bob token"
            FAIL=$((FAIL + 2))
        else
            RESP=$(curl -s -w '\n%{http_code}' -X GET "$GATEWAY_URL/auth/me" \
                -H "Authorization: Bearer $BOB_TOKEN")
            STATUS=$(get_status "$RESP")
            BODY=$(get_body "$RESP")
            run_status_test "Bob OIDC login gets 200" "200" "$STATUS"
            run_test "Response contains bob email" "bob@ccag-test.example.com" "$BODY"
        fi

        echo "  [4/4] Verify bob is present in admin users list"
        USERS_RESP=$(admin_curl GET "/admin/users")
        USERS_BODY=$(get_body "$USERS_RESP")
        USERS_STATUS=$(get_status "$USERS_RESP")
        run_status_test "Admin users list returns 200" "200" "$USERS_STATUS"
        run_test "Admin users list includes bob" "bob@ccag-test.example.com" "$USERS_BODY"
    fi
fi

echo ""

# ============================================================
# [Phase 7] SCIM DELETE alice → OIDC login should be blocked
# ============================================================

echo "[Phase 7] SCIM DELETE alice → OIDC should be blocked (soft-delete = inactive)"

if [ -z "$ALICE_USER_ID" ]; then
    echo "  SKIP: No alice user ID available"
    FAIL=$((FAIL + 2))
else
    echo "  [1/2] SCIM DELETE alice"
    DELETE_RESP=$(scim_curl DELETE "/scim/v2/Users/$ALICE_USER_ID")
    DELETE_STATUS=$(get_status "$DELETE_RESP")
    run_status_test "SCIM delete alice returns 204" "204" "$DELETE_STATUS"

    echo "  [2/2] OIDC login with deleted alice → 403 (soft-deleted = inactive)"
    ALICE_TOKEN=$(get_keycloak_token "alice" "alice-pass")
    RESP=$(curl -s -w '\n%{http_code}' -X GET "$GATEWAY_URL/auth/me" \
        -H "Authorization: Bearer $ALICE_TOKEN")
    STATUS=$(get_status "$RESP")
    BODY=$(get_body "$RESP")
    # A soft-deleted user has active=false, so resolve_oidc_role returns "deactivated"
    run_status_test "Soft-deleted alice gets 403" "403" "$STATUS"
    run_test "Error mentions deactivated or not provisioned" "deactivated\|not provisioned" "$BODY"
fi

echo ""
echo "=== SCIM + OIDC Results: $PASS passed, $FAIL failed ==="
[ $FAIL -eq 0 ] && exit 0 || exit 1
