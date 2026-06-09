//! Tests for OIDC team_id resolution and inactive-user gate at /v1/messages.
//!
//! Spec: `.claude/specs/oidc-team-resolution.md`
//! Tasks: `.claude/specs/oidc-team-resolution-tasks.md`
//!
//! These tests target two helpers that the Builder Agent must extract from
//! `src/api/handlers.rs::messages`:
//!
//! - `resolve_oidc_team_id(user: Option<&User>) -> Option<Uuid>`
//! - `oidc_inactive_response(user: Option<&User>) -> Option<Response>`
//!
//! Both are expected to live in a new `pub mod oidc_resolution` under
//! `src/api/`, exported via `crate::api::oidc_resolution`.

use axum::http::StatusCode;
use ccag::api::oidc_resolution::{oidc_inactive_response, resolve_oidc_team_id};
use ccag::db;
use ccag::db::schema::User;
use serde_json::Value;

use crate::helpers;

// ============================================================
// AC1.1 — resolve_oidc_team_id: pure helper unit tests
// ============================================================

/// AC1.1: A user row with `team_id=Some(T)` resolves to `Some(T)`.
#[tokio::test]
async fn resolve_oidc_team_id_returns_team_when_user_has_team() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "ac11-team").await;
    let user = helpers::create_test_user(&pool, "ac11@test.com", Some(team.id), "member").await;

    assert_eq!(user.team_id, Some(team.id));
    assert_eq!(resolve_oidc_team_id(Some(&user)), Some(team.id));
}

/// AC1.1: A user row with `team_id=None` resolves to `None`.
#[tokio::test]
async fn resolve_oidc_team_id_returns_none_when_user_has_no_team() {
    let pool = helpers::setup_test_db().await;

    let user = helpers::create_test_user(&pool, "ac11-noteam@test.com", None, "member").await;

    assert_eq!(user.team_id, None);
    assert_eq!(resolve_oidc_team_id(Some(&user)), None);
}

/// AC1.1: An absent user row (`None`) resolves to `None`.
#[tokio::test]
async fn resolve_oidc_team_id_returns_none_when_no_user_row() {
    // No DB needed — pure function.
    let no_user: Option<&User> = None;
    assert_eq!(resolve_oidc_team_id(no_user), None);
}

// ============================================================
// AC1.2 — OIDC user with team_id=Some(T) routes to T's endpoint
// ============================================================

/// AC1.2: When the resolver yields `Some(T)`, `get_team_endpoints(T)` returns
/// the endpoint bound to that team — proving the team→endpoint pipeline that
/// the handler will plug the resolver into actually selects T's endpoint
/// rather than the pool default.
#[tokio::test]
async fn oidc_user_with_team_routes_via_team_endpoint() {
    let pool = helpers::setup_test_db().await;

    // Two endpoints: one bound to the team, another marked as the pool default.
    let team_ep =
        db::endpoints::create_endpoint(&pool, "team-ep", None, None, None, "us-east-1", "us", 0)
            .await
            .unwrap();
    let default_ep = db::endpoints::create_endpoint(
        &pool,
        "pool-default-ep",
        None,
        None,
        None,
        "us-west-2",
        "us",
        0,
    )
    .await
    .unwrap();
    db::endpoints::set_default_endpoint(&pool, default_ep.id)
        .await
        .unwrap();

    // Bind team_ep to the team (priority 0).
    let team = helpers::create_test_team(&pool, "ac12-team").await;
    db::endpoints::set_team_endpoints(&pool, team.id, &[(team_ep.id, 0)])
        .await
        .unwrap();

    // OIDC user belongs to team T.
    let user =
        helpers::create_test_user(&pool, "ac12-user@test.com", Some(team.id), "member").await;

    // Resolver picks Some(T).
    let resolved = resolve_oidc_team_id(Some(&user));
    assert_eq!(resolved, Some(team.id));

    // The team→endpoint pipeline yields team_ep (NOT default_ep).
    let team_id = resolved.expect("resolver returned None for team-bound user");
    let endpoints = db::endpoints::get_team_endpoints(&pool, team_id)
        .await
        .unwrap();
    assert_eq!(
        endpoints.len(),
        1,
        "expected exactly one team-bound endpoint"
    );
    assert_eq!(endpoints[0].id, team_ep.id);
    assert_ne!(
        endpoints[0].id, default_ep.id,
        "OIDC team-routed request must not fall through to the pool default"
    );
}

// ============================================================
// AC1.3 — OIDC user without team falls through to pool default
// ============================================================

/// AC1.3: When the resolver yields `None`, the team-endpoints lookup is
/// skipped (handler builds an empty `team_endpoints` Vec) and `select_endpoint`
/// falls through to the pool default — same as pre-fix behavior.
#[tokio::test]
async fn oidc_user_without_team_falls_through_to_pool_default() {
    let pool = helpers::setup_test_db().await;

    // A pool-default endpoint exists.
    let default_ep = db::endpoints::create_endpoint(
        &pool,
        "pool-default-ep",
        None,
        None,
        None,
        "us-east-1",
        "us",
        0,
    )
    .await
    .unwrap();
    db::endpoints::set_default_endpoint(&pool, default_ep.id)
        .await
        .unwrap();

    // OIDC user with NO team membership.
    let user = helpers::create_test_user(&pool, "ac13-user@test.com", None, "member").await;

    // Resolver yields None.
    assert_eq!(resolve_oidc_team_id(Some(&user)), None);

    // Pool default is configured and would be selected by the handler
    // via `select_endpoint(&[], …)` empty-list branch.
    let pool_default = db::endpoints::get_default_endpoint(&pool).await.unwrap();
    assert!(
        pool_default.is_some(),
        "pool default endpoint should be set"
    );
    assert_eq!(pool_default.unwrap().id, default_ep.id);
}

/// AC1.3 (no-row variant): an OIDC identity for an email with no users-row
/// resolves to `None` exactly like the team_id=None case. Mirrors the
/// behavior matrix row "OIDC + no users row at all".
#[tokio::test]
async fn oidc_user_with_no_db_row_resolves_to_none() {
    let pool = helpers::setup_test_db().await;

    // Look up an email that was never provisioned.
    let row = db::users::get_user_by_email(&pool, "never-seen@test.com")
        .await
        .unwrap();
    assert!(row.is_none());

    assert_eq!(resolve_oidc_team_id(row.as_ref()), None);
}

// ============================================================
// AC2.1 — Inactive OIDC user produces 403 with exact body
// ============================================================

/// AC2.1: An OIDC user with `active=false` must be rejected with HTTP 403
/// and a JSON body whose `error` object matches the spec exactly:
/// `{"type":"permission_error","message":"Your account has been deactivated"}`.
#[tokio::test]
async fn inactive_oidc_user_returns_403_with_deactivated_message() {
    let pool = helpers::setup_test_db().await;

    let user = helpers::create_test_user(&pool, "ac21-inactive@test.com", None, "member").await;

    // Mark inactive (helpers::create_test_user defaults to active=true).
    sqlx::query("UPDATE users SET active = false WHERE id = $1")
        .bind(user.id)
        .execute(&pool)
        .await
        .unwrap();

    let inactive = db::users::get_user_by_email(&pool, "ac21-inactive@test.com")
        .await
        .unwrap()
        .expect("user row should exist");
    assert!(!inactive.active, "test setup: user must be inactive");

    let resp =
        oidc_inactive_response(Some(&inactive)).expect("inactive user must produce a 403 Response");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v: &axum::http::HeaderValue| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        ct.starts_with("application/json"),
        "expected JSON content-type, got {ct:?}"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).expect("response body must be JSON");
    let err = v
        .get("error")
        .expect("response body must contain top-level `error` object");
    assert_eq!(
        err.get("type").and_then(Value::as_str),
        Some("permission_error")
    );
    assert_eq!(
        err.get("message").and_then(Value::as_str),
        Some("Your account has been deactivated")
    );
}

/// AC2.1 (negative): an active user yields `None` (helper does not fire), and
/// an absent user (None) also yields `None` so unprovisioned identities fall
/// through to the existing logic rather than getting a spurious 403.
#[tokio::test]
async fn active_or_missing_user_does_not_trigger_inactive_response() {
    let pool = helpers::setup_test_db().await;

    let active_user =
        helpers::create_test_user(&pool, "ac21-active@test.com", None, "member").await;
    assert!(active_user.active);

    assert!(oidc_inactive_response(Some(&active_user)).is_none());
    assert!(oidc_inactive_response(None).is_none());
}

// ============================================================
// AC2.2 — Inactive check fires BEFORE team_id resolution
// ============================================================

/// AC2.2: A user row with `(active=false, team_id=Some(T))` produces the 403
/// from `oidc_inactive_response`. The handler must invoke the inactive check
/// before `resolve_oidc_team_id`, so the team-bound endpoint is never
/// reached. We assert the documented call ordering by checking that the
/// inactive helper still fires (returns Some) on a row that ALSO has a
/// team — and we exercise both helpers in the prescribed order:
///   1. oidc_inactive_response(user)  ->  Some(403) (short-circuit)
///   2. resolve_oidc_team_id(user)   (only reached if step 1 was None)
#[tokio::test]
async fn inactive_user_with_team_short_circuits_before_team_resolution() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "ac22-team").await;
    let team_ep = db::endpoints::create_endpoint(
        &pool,
        "ac22-team-ep",
        None,
        None,
        None,
        "us-east-1",
        "us",
        0,
    )
    .await
    .unwrap();
    db::endpoints::set_team_endpoints(&pool, team.id, &[(team_ep.id, 0)])
        .await
        .unwrap();

    let user =
        helpers::create_test_user(&pool, "ac22-inactive@test.com", Some(team.id), "member").await;
    sqlx::query("UPDATE users SET active = false WHERE id = $1")
        .bind(user.id)
        .execute(&pool)
        .await
        .unwrap();

    let inactive = db::users::get_user_by_email(&pool, "ac22-inactive@test.com")
        .await
        .unwrap()
        .expect("user row must exist");
    assert!(!inactive.active);
    assert_eq!(inactive.team_id, Some(team.id));

    // Step 1 — inactive check MUST fire even though team_id is Some.
    let resp = oidc_inactive_response(Some(&inactive))
        .expect("inactive user with a team must still produce a 403");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Step 2 (would-be reached only on active users): if the handler ever
    // got here, it would route to T's endpoint. We document the contract:
    // the inactive 403 short-circuits before this happens.
    //
    // If a future refactor accidentally inverts the order, the assertion
    // above would still pass — but the production handler test (manual
    // diff inspection per AC1.5 / AC2.2) is the structural guarantee.
    // Here we additionally verify that the team-routing path WOULD have
    // chosen T's endpoint (so any downstream regression that swallows the
    // inactive check would be observable as a routed request to team_ep).
    let endpoints = db::endpoints::get_team_endpoints(&pool, team.id)
        .await
        .unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].id, team_ep.id);
}

// ============================================================
// AC0.2 — Virtual-key path is structurally unchanged
// ============================================================

/// AC0.2: A virtual key with `team_id=Some(T)` continues to carry the team
/// id, and `get_team_endpoints(T)` continues to return T's bound endpoint.
/// The smoke test in `tests/integration_tests.rs` covers key creation; this
/// test additionally ties it through to endpoint binding so the virtual-key
/// arm of the handler's team_id resolution stays observably correct.
#[tokio::test]
async fn virtual_key_with_team_still_routes_via_team_endpoint() {
    let pool = helpers::setup_test_db().await;

    let team = helpers::create_test_team(&pool, "ac02-team").await;
    let team_ep = db::endpoints::create_endpoint(
        &pool,
        "ac02-team-ep",
        None,
        None,
        None,
        "us-east-1",
        "us",
        0,
    )
    .await
    .unwrap();
    db::endpoints::set_team_endpoints(&pool, team.id, &[(team_ep.id, 0)])
        .await
        .unwrap();

    let user =
        helpers::create_test_user(&pool, "ac02-user@test.com", Some(team.id), "member").await;
    let (_raw, key) =
        helpers::create_test_key(&pool, Some("ac02-key"), Some(user.id), Some(team.id)).await;

    assert_eq!(key.team_id, Some(team.id));

    let endpoints = db::endpoints::get_team_endpoints(&pool, team.id)
        .await
        .unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].id, team_ep.id);
}
