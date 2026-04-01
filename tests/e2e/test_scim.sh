#!/bin/bash
# E2E test for SCIM 2.0 provisioning endpoints.
# Exercises full SCIM lifecycle: discovery, user CRUD, group CRUD,
# Entra ID compatibility, and active-user enforcement.
#
# Usage:
#   ./tests/e2e/test_scim.sh [proxy_url]
#
# Requires: running gateway with admin login enabled, jq
# Default proxy_url: http://127.0.0.1:8080

set -euo pipefail

PROXY_URL="${1:-http://127.0.0.1:8080}"
PASS=0
FAIL=0
UNIQUE="e2e_$$_$RANDOM"

# Check for required tools
if ! command -v jq &> /dev/null; then
    echo "WARNING: jq is not installed. JSON parsing will fall back to grep."
    echo "         Install jq for more reliable test results."
    HAS_JQ=false
else
    HAS_JQ=true
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
# Usage: resp=$(scim_curl GET /scim/v2/Users)
#        body: get_body "$resp"
#        status: get_status "$resp"
scim_curl() {
    local method="$1"
    local path="$2"
    shift 2
    curl -s -w '\n%{http_code}' -X "$method" "$PROXY_URL$path" \
        -H "Authorization: Bearer $SCIM_TOKEN" \
        -H "Content-Type: application/scim+json" \
        "$@"
}

# Helper: admin curl — uses gateway session token in x-api-key header
admin_curl() {
    local method="$1"
    local path="$2"
    shift 2
    curl -s -w '\n%{http_code}' -X "$method" "$PROXY_URL$path" \
        -H "x-api-key: $API_KEY" \
        -H "Content-Type: application/json" \
        "$@"
}

get_body() { echo "$1" | sed '$d'; }
get_status() { echo "$1" | tail -1; }

echo "=== SCIM 2.0 E2E Tests ==="
echo "Proxy:  $PROXY_URL"
echo "Unique: $UNIQUE"
echo ""

# State variables (initialized empty to avoid -u unbound errors)
USER_ID=""
MEMBER_USER_ID=""
GROUP_ID=""

# ============================================================
# Setup: Login as admin and provision SCIM token
# ============================================================

echo "[Setup] Admin login"
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
echo "  Authenticated via /auth/login"

echo "[Setup] Create IDP"
IDP_RESP=$(curl -sf -X POST "$PROXY_URL/admin/idps" \
    -H "x-api-key: $API_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"name\":\"scim-e2e-$UNIQUE\",\"issuer_url\":\"https://$UNIQUE.example.com\",\"flow_type\":\"device_code\"}" 2>&1) || {
    echo "ERROR: Failed to create IDP: $IDP_RESP"
    exit 1
}

if [ "$HAS_JQ" = true ]; then
    IDP_ID=$(echo "$IDP_RESP" | jq -r '.id // empty')
else
    IDP_ID=$(echo "$IDP_RESP" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
fi
if [ -z "$IDP_ID" ]; then
    echo "ERROR: Could not extract IDP ID from: $IDP_RESP"
    exit 1
fi
echo "  Created IDP: $IDP_ID"

echo "[Setup] Enable SCIM for IDP"
SCIM_ENABLE_RESP=$(curl -sf -X PUT "$PROXY_URL/admin/idps/$IDP_ID/scim" \
    -H "x-api-key: $API_KEY" \
    -H "Content-Type: application/json" \
    -d '{"enabled":true}' 2>&1) || {
    echo "ERROR: Failed to enable SCIM: $SCIM_ENABLE_RESP"
    exit 1
}
echo "  SCIM enabled"

echo "[Setup] Create SCIM token"
TOKEN_RESP=$(curl -sf -X POST "$PROXY_URL/admin/idps/$IDP_ID/scim-tokens" \
    -H "x-api-key: $API_KEY" \
    -H "Content-Type: application/json" \
    -d '{"name":"e2e-test"}' 2>&1) || {
    echo "ERROR: Failed to create SCIM token: $TOKEN_RESP"
    exit 1
}

if [ "$HAS_JQ" = true ]; then
    SCIM_TOKEN=$(echo "$TOKEN_RESP" | jq -r '.token // empty')
else
    SCIM_TOKEN=$(echo "$TOKEN_RESP" | grep -o '"token":"[^"]*"' | head -1 | cut -d'"' -f4)
fi
if [ -z "$SCIM_TOKEN" ]; then
    echo "ERROR: Could not extract SCIM token from: $TOKEN_RESP"
    exit 1
fi
echo "  SCIM token obtained (prefix: ${SCIM_TOKEN:0:16}...)"
echo ""

# ============================================================
# [Discovery] — No auth required
# ============================================================

echo "[Discovery]"

echo "  [1/5] GET /scim/v2/ServiceProviderConfig returns 200"
SPC_RESP=$(curl -s -w '\n%{http_code}' "$PROXY_URL/scim/v2/ServiceProviderConfig")
SPC_BODY=$(get_body "$SPC_RESP")
SPC_STATUS=$(get_status "$SPC_RESP")
run_status_test "ServiceProviderConfig returns 200" "200" "$SPC_STATUS"

echo "  [2/5] ServiceProviderConfig has patch.supported"
run_test "ServiceProviderConfig has patch and supported" '"supported"' "$SPC_BODY"
run_test "ServiceProviderConfig has patch key" '"patch"' "$SPC_BODY"

echo "  [3/5] GET /scim/v2/ServiceProviderConfig Content-Type is scim+json"
SPC_CT=$(curl -sI "$PROXY_URL/scim/v2/ServiceProviderConfig" | grep -i content-type | tr -d '\r')
run_test "Content-Type contains scim+json" "scim+json" "$SPC_CT"

echo "  [4/5] GET /scim/v2/ResourceTypes has User and Group"
RT_RESP=$(curl -s "$PROXY_URL/scim/v2/ResourceTypes")
run_test "ResourceTypes has User" '"User"' "$RT_RESP"
run_test "ResourceTypes has Group" '"Group"' "$RT_RESP"

echo "  [5/5] GET /scim/v2/Schemas has user and group URIs"
SCH_RESP=$(curl -s "$PROXY_URL/scim/v2/Schemas")
run_test "Schemas has User URI" 'urn:ietf:params:scim:schemas:core:2.0:User' "$SCH_RESP"
run_test "Schemas has Group URI" 'urn:ietf:params:scim:schemas:core:2.0:Group' "$SCH_RESP"
echo ""

# ============================================================
# [User CRUD]
# ============================================================

echo "[User CRUD]"

echo "  [1/5] POST /scim/v2/Users creates user → 201"
CREATE_USER_RESP=$(scim_curl POST /scim/v2/Users \
    -d "{\"schemas\":[\"urn:ietf:params:scim:schemas:core:2.0:User\"],\"userName\":\"alice-$UNIQUE@example.com\",\"externalId\":\"ext-$UNIQUE\",\"name\":{\"givenName\":\"Alice\",\"familyName\":\"Test\"},\"active\":true}")
CREATE_USER_BODY=$(get_body "$CREATE_USER_RESP")
CREATE_USER_STATUS=$(get_status "$CREATE_USER_RESP")
run_status_test "Create user returns 201" "201" "$CREATE_USER_STATUS"

echo "  [2/5] Create user response has userName and externalId"
run_test "Response has userName" "alice-$UNIQUE@example.com" "$CREATE_USER_BODY"
run_test "Response has externalId" "ext-$UNIQUE" "$CREATE_USER_BODY"

if [ "$HAS_JQ" = true ]; then
    USER_ID=$(echo "$CREATE_USER_BODY" | jq -r '.id // empty')
else
    USER_ID=$(echo "$CREATE_USER_BODY" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
fi
if [ -z "$USER_ID" ]; then
    echo "  ERROR: Could not extract user ID — skipping remaining User tests"
    FAIL=$((FAIL + 3))
else
    echo "  Created user: $USER_ID"

    echo "  [3/5] GET /scim/v2/Users/{id} returns 200"
    GET_USER_RESP=$(scim_curl GET "/scim/v2/Users/$USER_ID")
    GET_USER_BODY=$(get_body "$GET_USER_RESP")
    GET_USER_STATUS=$(get_status "$GET_USER_RESP")
    run_status_test "Get user returns 200" "200" "$GET_USER_STATUS"
    run_test "Get user has correct userName" "alice-$UNIQUE@example.com" "$GET_USER_BODY"

    echo "  [4/5] GET /scim/v2/Users?filter=userName eq ... → totalResults >= 1"
    FILTER_RESP=$(scim_curl GET "/scim/v2/Users" --get --data-urlencode "filter=userName eq \"alice-$UNIQUE@example.com\"")
    FILTER_BODY=$(get_body "$FILTER_RESP")
    run_test "Filter returns totalResults" '"totalResults"' "$FILTER_BODY"

    echo "  [5/5] GET /scim/v2/Users → 200 with totalResults"
    LIST_USERS_RESP=$(scim_curl GET "/scim/v2/Users")
    LIST_USERS_BODY=$(get_body "$LIST_USERS_RESP")
    LIST_USERS_STATUS=$(get_status "$LIST_USERS_RESP")
    run_status_test "List users returns 200" "200" "$LIST_USERS_STATUS"
    run_test "List users has totalResults" '"totalResults"' "$LIST_USERS_BODY"
fi
echo ""

# ============================================================
# [User PATCH — Entra ID compatibility]
# ============================================================

echo "[User PATCH - Entra compat]"

if [ -z "$USER_ID" ]; then
    echo "  SKIP: No user ID available"
    FAIL=$((FAIL + 5))
else
    echo "  [1/5] PATCH displayName (op:Replace with path)"
    PATCH1_RESP=$(scim_curl PATCH "/scim/v2/Users/$USER_ID" \
        -d '{"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"Replace","path":"displayName","value":"Alice Updated"}]}')
    PATCH1_STATUS=$(get_status "$PATCH1_RESP")
    run_status_test "Patch displayName returns 200" "200" "$PATCH1_STATUS"

    echo "  [2/5] PATCH deactivate with string 'False' (Entra-style)"
    PATCH2_RESP=$(scim_curl PATCH "/scim/v2/Users/$USER_ID" \
        -d '{"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"Replace","path":"active","value":"False"}]}')
    PATCH2_BODY=$(get_body "$PATCH2_RESP")
    PATCH2_STATUS=$(get_status "$PATCH2_RESP")
    run_status_test "Patch deactivate returns 200" "200" "$PATCH2_STATUS"
    run_test "Patch deactivate sets active:false" '"active":false' "$PATCH2_BODY"

    echo "  [3/5] PATCH reactivate with string 'True' (Entra-style)"
    PATCH3_RESP=$(scim_curl PATCH "/scim/v2/Users/$USER_ID" \
        -d '{"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"Replace","path":"active","value":"True"}]}')
    PATCH3_BODY=$(get_body "$PATCH3_RESP")
    PATCH3_STATUS=$(get_status "$PATCH3_RESP")
    run_status_test "Patch reactivate returns 200" "200" "$PATCH3_STATUS"
    run_test "Patch reactivate sets active:true" '"active":true' "$PATCH3_BODY"

    echo "  [4/5] PATCH with path-less value object (Entra-style)"
    PATCH4_RESP=$(scim_curl PATCH "/scim/v2/Users/$USER_ID" \
        -d '{"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"Replace","value":{"displayName":"Alice Final"}}]}')
    PATCH4_STATUS=$(get_status "$PATCH4_RESP")
    run_status_test "Patch path-less value returns 200" "200" "$PATCH4_STATUS"

    echo "  [5/5] GET user after patch has updated displayName"
    GET_AFTER_PATCH=$(scim_curl GET "/scim/v2/Users/$USER_ID")
    GET_AFTER_PATCH_BODY=$(get_body "$GET_AFTER_PATCH")
    run_test "User has Alice Final displayName" "Alice Final" "$GET_AFTER_PATCH_BODY"
fi
echo ""

# ============================================================
# [User DELETE]
# ============================================================

echo "[User DELETE]"

if [ -z "$USER_ID" ]; then
    echo "  SKIP: No user ID available"
    FAIL=$((FAIL + 2))
else
    echo "  [1/2] DELETE /scim/v2/Users/{id} → 204"
    DELETE_USER_RESP=$(scim_curl DELETE "/scim/v2/Users/$USER_ID")
    DELETE_USER_STATUS=$(get_status "$DELETE_USER_RESP")
    run_status_test "Delete user returns 204" "204" "$DELETE_USER_STATUS"

    echo "  [2/2] GET deleted user → 200 with active:false (soft-delete)"
    GET_DELETED_RESP=$(scim_curl GET "/scim/v2/Users/$USER_ID")
    GET_DELETED_BODY=$(get_body "$GET_DELETED_RESP")
    GET_DELETED_STATUS=$(get_status "$GET_DELETED_RESP")
    run_status_test "Get soft-deleted user returns 200" "200" "$GET_DELETED_STATUS"
    run_test "Soft-deleted user has active:false" '"active":false' "$GET_DELETED_BODY"
fi
echo ""

# ============================================================
# [Group CRUD]
# ============================================================

echo "[Group CRUD]"

# Create a fresh user for group membership tests
echo "  Creating member user for group tests"
MEMBER_CREATE_RESP=$(scim_curl POST /scim/v2/Users \
    -d "{\"schemas\":[\"urn:ietf:params:scim:schemas:core:2.0:User\"],\"userName\":\"member-$UNIQUE@example.com\",\"externalId\":\"ext-member-$UNIQUE\",\"active\":true}")
MEMBER_CREATE_BODY=$(get_body "$MEMBER_CREATE_RESP")
MEMBER_CREATE_STATUS=$(get_status "$MEMBER_CREATE_RESP")

if [ "$HAS_JQ" = true ]; then
    MEMBER_USER_ID=$(echo "$MEMBER_CREATE_BODY" | jq -r '.id // empty')
else
    MEMBER_USER_ID=$(echo "$MEMBER_CREATE_BODY" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
fi
if [ -z "$MEMBER_USER_ID" ]; then
    echo "  WARNING: Could not create member user (status $MEMBER_CREATE_STATUS) — group membership tests will skip"
fi

echo "  [1/3] POST /scim/v2/Groups creates group → 201"
CREATE_GROUP_RESP=$(scim_curl POST /scim/v2/Groups \
    -d "{\"schemas\":[\"urn:ietf:params:scim:schemas:core:2.0:Group\"],\"displayName\":\"Engineering-$UNIQUE\",\"externalId\":\"grp-$UNIQUE\"}")
CREATE_GROUP_BODY=$(get_body "$CREATE_GROUP_RESP")
CREATE_GROUP_STATUS=$(get_status "$CREATE_GROUP_RESP")
run_status_test "Create group returns 201" "201" "$CREATE_GROUP_STATUS"

if [ "$HAS_JQ" = true ]; then
    GROUP_ID=$(echo "$CREATE_GROUP_BODY" | jq -r '.id // empty')
else
    GROUP_ID=$(echo "$CREATE_GROUP_BODY" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
fi
if [ -z "$GROUP_ID" ]; then
    echo "  ERROR: Could not extract group ID — skipping remaining Group tests"
    FAIL=$((FAIL + 8))
    GROUP_ID=""
else
    echo "  Created group: $GROUP_ID"

    echo "  [2/3] GET /scim/v2/Groups/{id} → 200 with displayName"
    GET_GROUP_RESP=$(scim_curl GET "/scim/v2/Groups/$GROUP_ID")
    GET_GROUP_BODY=$(get_body "$GET_GROUP_RESP")
    GET_GROUP_STATUS=$(get_status "$GET_GROUP_RESP")
    run_status_test "Get group returns 200" "200" "$GET_GROUP_STATUS"
    run_test "Get group has displayName" "Engineering-$UNIQUE" "$GET_GROUP_BODY"

    echo "  [3/3] GET /scim/v2/Groups/{id}?excludedAttributes=members → no populated members"
    GET_GROUP_EXCL_RESP=$(scim_curl GET "/scim/v2/Groups/$GROUP_ID" --get --data-urlencode "excludedAttributes=members")
    GET_GROUP_EXCL_BODY=$(get_body "$GET_GROUP_EXCL_RESP")
    GET_GROUP_EXCL_STATUS=$(get_status "$GET_GROUP_EXCL_RESP")
    run_status_test "Get group excludedAttributes returns 200" "200" "$GET_GROUP_EXCL_STATUS"
    # Members should be absent or empty when excluded
    if echo "$GET_GROUP_EXCL_BODY" | grep -q '"members":\[.\]'; then
        echo "  FAIL: excludedAttributes=members — members array is non-empty"
        FAIL=$((FAIL + 1))
    else
        echo "  PASS: excludedAttributes=members — members not present or empty"
        PASS=$((PASS + 1))
    fi
fi
echo ""

# ============================================================
# [Group PATCH]
# ============================================================

echo "[Group PATCH]"

if [ -z "$GROUP_ID" ]; then
    echo "  SKIP: No group ID available"
    FAIL=$((FAIL + 5))
elif [ -z "$MEMBER_USER_ID" ]; then
    echo "  SKIP: No member user ID available"
    FAIL=$((FAIL + 5))
else
    echo "  [1/5] PATCH add member → 204"
    PATCH_ADD_RESP=$(scim_curl PATCH "/scim/v2/Groups/$GROUP_ID" \
        -d "{\"schemas\":[\"urn:ietf:params:scim:api:messages:2.0:PatchOp\"],\"Operations\":[{\"op\":\"Add\",\"path\":\"members\",\"value\":[{\"value\":\"$MEMBER_USER_ID\"}]}]}")
    PATCH_ADD_STATUS=$(get_status "$PATCH_ADD_RESP")
    run_status_test "Patch add member returns 204" "204" "$PATCH_ADD_STATUS"

    echo "  [2/5] GET group after add has member"
    GET_AFTER_ADD_RESP=$(scim_curl GET "/scim/v2/Groups/$GROUP_ID")
    GET_AFTER_ADD_BODY=$(get_body "$GET_AFTER_ADD_RESP")
    run_test "Group has member after add" "$MEMBER_USER_ID" "$GET_AFTER_ADD_BODY"

    echo "  [3/5] PATCH remove member → 204"
    # RFC 7644 filter path for removing a specific member
    PATCH_RM_RESP=$(scim_curl PATCH "/scim/v2/Groups/$GROUP_ID" \
        -d "{\"schemas\":[\"urn:ietf:params:scim:api:messages:2.0:PatchOp\"],\"Operations\":[{\"op\":\"Remove\",\"path\":\"members[value eq \\\"$MEMBER_USER_ID\\\"]\"}]}")
    PATCH_RM_STATUS=$(get_status "$PATCH_RM_RESP")
    run_status_test "Patch remove member returns 204" "204" "$PATCH_RM_STATUS"

    echo "  [4/5] PATCH rename group (path-less Entra style) → 204"
    PATCH_RENAME_RESP=$(scim_curl PATCH "/scim/v2/Groups/$GROUP_ID" \
        -d "{\"schemas\":[\"urn:ietf:params:scim:api:messages:2.0:PatchOp\"],\"Operations\":[{\"op\":\"Replace\",\"value\":{\"displayName\":\"Eng-Renamed-$UNIQUE\"}}]}")
    PATCH_RENAME_STATUS=$(get_status "$PATCH_RENAME_RESP")
    run_status_test "Patch rename group returns 204" "204" "$PATCH_RENAME_STATUS"

    echo "  [5/5] GET group after rename has new displayName"
    GET_AFTER_RENAME_RESP=$(scim_curl GET "/scim/v2/Groups/$GROUP_ID")
    GET_AFTER_RENAME_BODY=$(get_body "$GET_AFTER_RENAME_RESP")
    run_test "Group has renamed displayName" "Eng-Renamed-$UNIQUE" "$GET_AFTER_RENAME_BODY"
fi
echo ""

# ============================================================
# [Group DELETE]
# ============================================================

echo "[Group DELETE]"

if [ -z "$GROUP_ID" ]; then
    echo "  SKIP: No group ID available"
    FAIL=$((FAIL + 1))
else
    echo "  [1/1] DELETE /scim/v2/Groups/{id} → 204"
    DELETE_GROUP_RESP=$(scim_curl DELETE "/scim/v2/Groups/$GROUP_ID")
    DELETE_GROUP_STATUS=$(get_status "$DELETE_GROUP_RESP")
    run_status_test "Delete group returns 204" "204" "$DELETE_GROUP_STATUS"
fi
echo ""

# ============================================================
# [Auth]
# ============================================================

echo "[Auth]"

echo "  [1/2] GET /scim/v2/Users without Authorization header → 401"
NO_AUTH_RESP=$(curl -s -o /dev/null -w "%{http_code}" \
    -H "Content-Type: application/scim+json" \
    "$PROXY_URL/scim/v2/Users")
run_status_test "No auth returns 401" "401" "$NO_AUTH_RESP"

echo "  [2/2] GET /scim/v2/Users with invalid token → 401"
INVALID_AUTH_RESP=$(curl -s -o /dev/null -w "%{http_code}" \
    -H "Authorization: Bearer invalid-token-xyz-$UNIQUE" \
    -H "Content-Type: application/scim+json" \
    "$PROXY_URL/scim/v2/Users")
run_status_test "Invalid token returns 401" "401" "$INVALID_AUTH_RESP"

echo ""
echo "=== SCIM E2E Results: $PASS passed, $FAIL failed ==="
[ $FAIL -eq 0 ] && exit 0 || exit 1
