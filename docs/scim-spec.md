---
title: "SCIM 2.0 Provisioning API"
description: "SCIM 2.0 user and group provisioning for CCAG — automate user lifecycle with Okta, Entra ID, authentik, or any SCIM-compliant identity provider."
---

# SCIM 2.0 API Specification for CCAG

> **Status**: Complete (Users + Groups + Admin API + CLI + Portal UI).
>
> **Standards**: [RFC 7644](https://datatracker.ietf.org/doc/html/rfc7644) (Protocol), [RFC 7643](https://datatracker.ietf.org/doc/html/rfc7643) (Core Schema)

## Overview

CCAG implements a SCIM 2.0 service provider that enables identity providers (Okta, Entra ID, OneLogin, etc.) to automatically provision and deprovision users and manage role assignments via groups. All SCIM endpoints are mounted under `/scim/v2/`.

**Key design decision**: SCIM Groups map to **roles** (admin/member), not CCAG teams. CCAG teams are budget/routing units managed manually via portal or CLI. This separation keeps budget attribution unambiguous (one team per user) while allowing IdPs to control user lifecycle and role assignment.

### Design Principles

- **Per-IDP scoping**: Each SCIM bearer token is bound to an identity provider. Operations are scoped to users/groups managed by that IDP.
- **Soft-delete**: SCIM DELETE sets `active=false` rather than hard-deleting, preserving spend history.
- **Coexistence**: SCIM and OIDC auto-provisioning coexist. When `scim_enabled=true` for an IDP, auto-provisioning is disabled for that IDP only.
- **One-directional sync**: Changes flow from IdP to CCAG. CCAG does not push changes back.

---

## Authentication

All SCIM endpoints except discovery (`/ServiceProviderConfig`, `/ResourceTypes`, `/Schemas`) require a bearer token:

```
Authorization: Bearer scim-ccag-<64 hex chars>
```

**Token format**: `scim-ccag-` prefix + 32 random bytes hex-encoded (64 chars). Total: 74 chars.

**Token storage**: SHA-256 hash stored in `scim_tokens` table, linked to an `identity_provider` via `idp_id`.

**Validation flow**:
1. Extract token from `Authorization: Bearer <token>` header
2. Compute `SHA-256(token)` 
3. Lookup in `scim_tokens` WHERE `token_hash = hash AND enabled = true`
4. Return associated `idp_id` for operation scoping
5. Update `last_used_at` timestamp

**Error responses**:
- Missing/malformed header: `401 Unauthorized`
- Invalid/revoked token: `401 Unauthorized`

---

## Content Type

- Requests: Accept both `application/scim+json` and `application/json`
- Responses: Always `Content-Type: application/scim+json; charset=utf-8`

---

## Error Format

All errors follow RFC 7644 Section 3.12:

```json
{
  "schemas": ["urn:ietf:params:scim:api:messages:2.0:Error"],
  "scimType": "uniqueness",
  "detail": "User with userName alice@example.com already exists",
  "status": "409"
}
```

| HTTP Status | scimType | When |
|-------------|----------|------|
| 400 | `invalidValue` | Malformed request body or missing required field |
| 400 | `invalidFilter` | Unparseable filter expression |
| 401 | — | Missing or invalid bearer token |
| 404 | — | Resource not found or not in scope for this IDP |
| 409 | `uniqueness` | Duplicate userName or externalId |
| 413 | `tooMany` | Result set exceeds maxResults |
| 500 | — | Internal server error |

---

## Discovery Endpoints

These are public (no authentication required).

### GET /scim/v2/ServiceProviderConfig

Returns server capabilities.

```json
{
  "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
  "patch": { "supported": true },
  "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
  "filter": { "supported": true, "maxResults": 100 },
  "changePassword": { "supported": false },
  "sort": { "supported": false },
  "etag": { "supported": false },
  "authenticationSchemes": [
    {
      "type": "oauthbearertoken",
      "name": "OAuth Bearer Token",
      "description": "Authentication using a SCIM bearer token",
      "specUri": "https://datatracker.ietf.org/doc/html/rfc6750"
    }
  ]
}
```

### GET /scim/v2/ResourceTypes

```json
{
  "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
  "totalResults": 2,
  "Resources": [
    {
      "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
      "id": "User",
      "name": "User",
      "endpoint": "/scim/v2/Users",
      "schema": "urn:ietf:params:scim:schemas:core:2.0:User"
    },
    {
      "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"],
      "id": "Group",
      "name": "Group",
      "endpoint": "/scim/v2/Groups",
      "schema": "urn:ietf:params:scim:schemas:core:2.0:Group"
    }
  ]
}
```

### GET /scim/v2/Schemas

Returns the full User and Group schema definitions. Response is a ListResponse containing two schema resources:

- `urn:ietf:params:scim:schemas:core:2.0:User` — attributes: userName (required), name, displayName, emails, active, externalId, groups (readOnly)
- `urn:ietf:params:scim:schemas:core:2.0:Group` — attributes: displayName (required), members, externalId

---

## Filtering

Supported filter expressions (subset of RFC 7644 Section 3.4.2.2):

| Operator | Example | Description |
|----------|---------|-------------|
| `eq` | `userName eq "alice@example.com"` | Exact match (case-insensitive for strings) |
| `co` | `userName co "alice"` | Contains substring |
| `sw` | `userName sw "alice"` | Starts with |
| `and` | `userName eq "alice@example.com" and active eq true` | Logical AND |

**Filterable attributes**:
- Users: `userName`, `externalId`, `active`, `displayName`
- Groups: `displayName`, `externalId`

**Grammar** (simplified):

```
filter     = attrExpr / filter "and" filter
attrExpr   = attrPath SP compareOp SP compValue
attrPath   = ALPHA *(nameChar)
compareOp  = "eq" / "co" / "sw"
compValue  = DQUOTE *CHAR DQUOTE / "true" / "false"
```

String comparisons are case-insensitive. Boolean values are unquoted `true`/`false`.

---

## Pagination

List endpoints support:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `startIndex` | 1 | 1-based starting index |
| `count` | 100 | Maximum results per page (max: 100) |

Response (ListResponse):

```json
{
  "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
  "totalResults": 42,
  "startIndex": 1,
  "itemsPerPage": 20,
  "Resources": [...]
}
```

---

## User Resource

### Schema

```json
{
  "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "externalId": "okta-user-id-123",
  "userName": "alice@example.com",
  "name": {
    "givenName": "Alice",
    "familyName": "Smith"
  },
  "displayName": "Alice Smith",
  "emails": [
    { "value": "alice@example.com", "primary": true, "type": "work" }
  ],
  "active": true,
  "groups": [
    { "value": "team-uuid", "display": "Engineering", "$ref": "/scim/v2/Groups/team-uuid" }
  ],
  "meta": {
    "resourceType": "User",
    "created": "2024-01-15T09:30:00Z",
    "lastModified": "2024-01-15T09:30:00Z",
    "location": "/scim/v2/Users/550e8400-e29b-41d4-a716-446655440000"
  }
}
```

### Attribute Mapping

| SCIM Attribute | CCAG Column | Notes |
|----------------|-------------|-------|
| `id` | `users.id` | UUID, server-assigned, immutable |
| `externalId` | `users.external_id` | IdP's unique ID for this user |
| `userName` | `users.email` | Required. Used as primary identifier |
| `name.givenName` | `users.given_name` | |
| `name.familyName` | `users.family_name` | |
| `displayName` | `users.display_name` | |
| `emails[0].value` | `users.email` | Same as userName |
| `active` | `users.active` | Default: true |
| `groups` | Derived from `users.team_id` | Read-only in User resource |
| `meta.created` | `users.created_at` | |
| `meta.lastModified` | `users.created_at` | (no separate updated_at column) |

### POST /scim/v2/Users

Create a new user. Returns `201 Created` with `Location` header.

**Request**:
```json
{
  "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
  "userName": "alice@example.com",
  "externalId": "okta-user-id-123",
  "name": { "givenName": "Alice", "familyName": "Smith" },
  "emails": [{ "value": "alice@example.com", "primary": true }],
  "active": true
}
```

**Behavior**:
1. Validate `userName` is present and non-empty
2. Check for existing user by `external_id` (within this IDP) or by `email`
3. If exists: `409 Conflict` with `scimType: "uniqueness"`
4. Create user with `role = IDP's default_role`, `scim_managed = true`, `idp_id = token's IDP`
5. Return ScimUser with `Location: /scim/v2/Users/{id}`

### GET /scim/v2/Users

List users. Supports `filter`, `startIndex`, `count` query parameters.

**Scoping**: Only returns users where `idp_id` matches the authenticated token's IDP, OR `scim_managed = false` (for transition visibility).

**Response**: ListResponse with ScimUser resources.

### GET /scim/v2/Users/{id}

Get a single user. Returns `404` if not found or not in scope.

### PUT /scim/v2/Users/{id}

Full replacement. All mutable attributes in the request replace existing values. Missing optional attributes are cleared. `id` and `meta` are ignored in the request body.

Returns `200 OK` with updated ScimUser.

### PATCH /scim/v2/Users/{id}

Partial update. Used heavily by IdPs for deactivation and attribute changes.

**Request**:
```json
{
  "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
  "Operations": [
    { "op": "replace", "path": "active", "value": false }
  ]
}
```

**Supported operations**:

| op | path | value | Effect |
|----|------|-------|--------|
| `replace` | `active` | `true`/`false` | Activate/deactivate user |
| `replace` | `userName` | string | Change email |
| `replace` | `displayName` | string | Change display name |
| `replace` | `name.givenName` | string | Change given name |
| `replace` | `name.familyName` | string | Change family name |
| `replace` | `externalId` | string | Change external ID |

Operations are applied sequentially. If any operation fails, the entire PATCH is rolled back.

Returns `200 OK` with updated ScimUser.

### DELETE /scim/v2/Users/{id}

Soft-delete: sets `active = false`. Returns `204 No Content`.

---

## Group Resource

### Schema

```json
{
  "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
  "id": "team-uuid",
  "externalId": "okta-group-123",
  "displayName": "Engineering",
  "members": [
    { "value": "user-uuid-1", "display": "alice@example.com", "$ref": "/scim/v2/Users/user-uuid-1" },
    { "value": "user-uuid-2", "display": "bob@example.com", "$ref": "/scim/v2/Users/user-uuid-2" }
  ],
  "meta": {
    "resourceType": "Group",
    "created": "2024-01-15T09:30:00Z",
    "lastModified": "2024-01-15T09:30:00Z",
    "location": "/scim/v2/Groups/team-uuid"
  }
}
```

### Attribute Mapping

| SCIM Attribute | CCAG Column | Notes |
|----------------|-------------|-------|
| `id` | `scim_groups.id` | UUID, server-assigned |
| `externalId` | `scim_groups.external_id` | IdP's unique ID |
| `displayName` | `scim_groups.display_name` | Required |
| `members[].value` | `scim_group_members.user_id` | Many-to-many join |
| `meta.created` | `scim_groups.created_at` | |

**Important**: SCIM Groups are **not** CCAG teams. They are lightweight role-mapping groups stored in a separate `scim_groups` table. When group membership changes, CCAG re-evaluates each affected user's role based on the IDP's `scim_admin_groups` configuration.

#### Role Evaluation

When group membership changes (add/remove/replace members, delete group):

1. Get all `scim_groups` the user belongs to (for this IDP)
2. Get the IDP's `scim_admin_groups` list (JSON array of group displayNames)
3. If any of the user's groups match → set `users.role = "admin"`
4. Else → set `users.role` to IDP's `default_role` (usually `"member"`)

Configure via: `PUT /admin/idps/{id}/scim-admin-groups` or `ccag scim set-admin-groups --idp <name> --groups "group1,group2"`

### POST /scim/v2/Groups

Create a group. Returns `201 Created`.

**Request**:
```json
{
  "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"],
  "displayName": "Engineering",
  "externalId": "okta-group-123",
  "members": [
    { "value": "user-uuid-1" },
    { "value": "user-uuid-2" }
  ]
}
```

**Behavior**:
1. Create row in `scim_groups` with `display_name`, `external_id`, `idp_id`
2. Insert members into `scim_group_members` join table
3. Re-evaluate role for each member (may promote to admin)
4. Return ScimGroup with `Location` header

### GET /scim/v2/Groups

List groups. Supports `filter`, `startIndex`, `count`.

### GET /scim/v2/Groups/{id}

Get group with member list. Supports `excludedAttributes=members` query parameter (used by Entra ID) to omit the members array from the response.

### PUT /scim/v2/Groups/{id}

Full replacement including member list. Atomically replaces all members. Re-evaluates roles for both old and new members.

### PATCH /scim/v2/Groups/{id}

**Common operations from IdPs**:

```json
{
  "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
  "Operations": [
    { "op": "add", "path": "members", "value": [{ "value": "user-uuid" }] },
    { "op": "remove", "path": "members[value eq \"user-uuid\"]" }
  ]
}
```

| op | path | Effect |
|----|------|--------|
| `replace` | `displayName` | Rename group |
| `add` | `members` | Add users to group, re-evaluate roles |
| `remove` | `members[value eq "uuid"]` | Remove user from group, re-evaluate role |
| `replace` | `members` | Replace full member list, re-evaluate all roles |

**Entra ID**: Path-less format for rename: `{"op": "Replace", "value": {"displayName": "..."}}`. Expects `204 No Content` response.

**Okta**: Path-less format for rename. Members use `{"value": "user-id", "display": "user@example.com"}` format with optional `$ref: null`.

Returns `204 No Content` (Entra ID requirement; other IdPs accept both 200 and 204).

### DELETE /scim/v2/Groups/{id}

Deletes the group (CASCADE removes join table entries). Re-evaluates roles for former members (may demote from admin). Returns `204 No Content`.

---

## Active-User Enforcement

When `users.active = false`:

1. **OIDC login**: `resolve_oidc_role()` returns an error; request gets `403 Forbidden: "Your account has been deactivated"`
2. **Virtual keys**: Keys owned by inactive users are filtered out during cache reload (every 5s poll cycle)
3. **Session tokens**: Session validation checks `active` flag; inactive users get `403`
4. **Admin API**: Inactive users cannot access admin endpoints

## SCIM-Managed IDP Behavior

When `identity_providers.scim_enabled = true`:

- Auto-provisioning is disabled for that IDP
- OIDC login for users not in the DB gets `403 Forbidden: "User not provisioned. Contact your administrator."`
- Users must be created via SCIM before they can log in
- Existing users (created before SCIM was enabled) continue to work

---

## Database Schema

### New Table: scim_tokens

```sql
CREATE TABLE scim_tokens (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    idp_id UUID NOT NULL REFERENCES identity_providers(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    token_prefix TEXT NOT NULL,
    name TEXT,
    created_by TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT true,
    last_used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

### Extended: users

New columns: `active` (BOOL, default true), `external_id` (TEXT, unique where not null), `display_name` (TEXT), `given_name` (TEXT), `family_name` (TEXT), `scim_managed` (BOOL, default false), `idp_id` (UUID FK to identity_providers)

### New Table: scim_groups

```sql
CREATE TABLE scim_groups (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    external_id TEXT,
    display_name TEXT NOT NULL,
    idp_id UUID NOT NULL REFERENCES identity_providers(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

### New Table: scim_group_members

```sql
CREATE TABLE scim_group_members (
    group_id UUID NOT NULL REFERENCES scim_groups(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id)
);
```

### Extended: identity_providers

New columns: `scim_enabled` (BOOL, default false), `scim_admin_groups` (JSONB, default `[]` — array of group displayNames that grant admin role)

---

## Admin API for SCIM Management

| Method | Path | Description |
|--------|------|-------------|
| POST | `/admin/idps/{idp_id}/scim-tokens` | Generate SCIM token |
| GET | `/admin/idps/{idp_id}/scim-tokens` | List tokens |
| DELETE | `/admin/idps/{idp_id}/scim-tokens/{id}` | Revoke token |
| PUT | `/admin/idps/{idp_id}/scim` | Enable/disable SCIM |
| GET | `/admin/idps/{idp_id}/scim-admin-groups` | Get admin group mappings |
| PUT | `/admin/idps/{idp_id}/scim-admin-groups` | Set admin group mappings |
| GET | `/admin/teams/{team_id}/members` | List team members |
| POST | `/admin/teams/{team_id}/members` | Add member to team |
| DELETE | `/admin/teams/{team_id}/members/{user_id}` | Remove member from team |

## CLI Commands

```bash
ccag scim enable --idp <name-or-id>              # Enable SCIM for an IDP
ccag scim disable --idp <name-or-id>             # Disable SCIM
ccag scim create-token --idp <name> [--name lbl] # Generate bearer token (stdout)
ccag scim list-tokens --idp <name>               # List tokens
ccag scim revoke-token --idp <name> --token-id X # Revoke a token
ccag scim set-admin-groups --idp <name> --groups "g1,g2"  # Set admin groups
ccag scim status --idp <name>                    # Show SCIM config status
ccag teams add-member --team <name> --user <email>    # Manual team assignment
ccag teams remove-member --team <name> --user <email> # Remove from team
ccag teams members <team-id>                          # List team members
```

---

## Identity Provider Compatibility

CCAG's SCIM implementation is designed to work with all major identity providers. The following IdP-specific behaviors are handled:

### Microsoft Entra ID (Azure AD)

Entra ID has several well-documented deviations from the SCIM RFC:

- **Capitalized `op` values**: Sends `"Replace"`, `"Add"`, `"Remove"` instead of lowercase. CCAG normalizes to lowercase.
- **Boolean-as-string**: Sends `"value": "False"` (JSON string) instead of `"value": false` (JSON boolean) for the `active` attribute. CCAG's `parse_bool_value()` handles both formats.
- **`emails[type eq "work"].value` path**: Uses multi-valued attribute path syntax for email updates. CCAG maps this to the `userName`/email field.
- **Path-less PATCH**: Sends operations with no `path` field, where `value` is an object: `{"op": "Replace", "value": {"active": false}}`. CCAG iterates the value object and applies each key.
- **No DELETE calls**: Deactivation is always via `PATCH active=false`, not `DELETE /Users/{id}`.
- **Extra attributes in deactivation**: Deactivation PATCH may include additional attribute updates (e.g., name changes) alongside `active=false`.
- **Filter operators**: Only uses `eq` and `and`.
- **Group PATCH response**: Expects `204 No Content` for group PATCH operations.
- **`excludedAttributes=members`**: Sends this query parameter on group GET requests.

References:
- [Develop a SCIM endpoint (Microsoft Learn)](https://learn.microsoft.com/en-us/entra/identity/app-provisioning/use-scim-to-provision-users-and-groups)
- [Known issues for provisioning](https://learn.microsoft.com/en-us/entra/identity/app-provisioning/known-issues)

### Okta

Okta is more RFC-compliant but has some specific behaviors:

- **Both PUT and PATCH**: Default depends on integration type (OIN catalog uses PATCH, AIW uses PUT).
- **`op` values**: Both lowercase and capitalized forms observed. CCAG handles both.
- **Proper JSON booleans**: Sends `false`/`true` as JSON booleans (not strings).
- **Path-less PATCH for groups**: Group rename uses `{"op": "Replace", "value": {"displayName": "..."}}` (no `path`).
- **Group member add**: Uses `"op": "Add"` with `"path": "members"` and `"value": [{"value": "user-id", "display": "user@example.com"}]`.
- **Deactivation**: `PATCH active=false` (same as Entra).
- **Rate limiting**: Expects `429 Too Many Requests` with optional `Retry-After` header. Implements exponential backoff.

References:
- [Okta SCIM 2.0 Guide](https://developer.okta.com/docs/api/openapi/okta-scim/guides/scim-20)
- [SCIM Integration Concepts](https://developer.okta.com/docs/concepts/scim/faqs/)

### OneLogin

- Standard RFC-compliant SCIM 2.0 client.
- Uses lowercase `op` values.
- Proper JSON boolean values.
- Both PUT and PATCH supported.

### JumpCloud

- Standard SCIM 2.0 client.
- Full directory platform — SCIM behavior is well-structured.
- Both PUT and PATCH supported.

### PingIdentity (PingFederate / PingOne)

- Standard SCIM 2.0 client.
- Both PUT and PATCH supported.

### Google Workspace

- **Not a standard SCIM client** for outbound provisioning to external apps.
- Uses its own auto-provisioning system. Google Workspace users should use Google as an OIDC provider with CCAG's auto-provisioning or manual user management.

### Compatibility Summary

| Feature | Entra ID | Okta | OneLogin | JumpCloud | Ping |
|---------|----------|------|----------|-----------|------|
| PATCH ops | Add/Replace/Remove (capitalized) | add/replace (mixed case) | replace (lowercase) | replace | replace |
| Boolean values | String `"False"` | JSON `false` | JSON `false` | JSON `false` | JSON `false` |
| Path-less PATCH | Yes | Yes (groups) | No | No | No |
| Email path format | `emails[type eq "work"].value` | `userName` | `userName` | `userName` | `userName` |
| Deactivation | PATCH active=false | PATCH active=false | PATCH active=false | PATCH active=false | PATCH active=false |
| Group PATCH response | 204 No Content | 200 OK or 204 | 200 OK | 200 OK | 200 OK |
| Filter operators | eq, and | eq, and, co, sw | eq, and | eq | eq, and |
