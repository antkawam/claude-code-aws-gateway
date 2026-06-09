//! Pure helpers for resolving OIDC requests' team membership and gating
//! inactive accounts at the `/v1/messages` boundary.
//!
//! Spec: `.claude/specs/oidc-team-resolution.md`
//!
//! These helpers operate on the already-fetched `users` row that the
//! `messages` handler hoists out of the OIDC arm — no DB round-trips here.

use axum::{
    body::Body,
    http::{Response, StatusCode},
};
use serde_json::json;
use uuid::Uuid;

use crate::db::schema::User;

/// Resolve the `team_id` for an OIDC-authenticated user.
///
/// Returns the user's `team_id` when a row was found, otherwise `None`.
/// Pure function — no I/O. The caller must perform the single
/// `get_user_by_email` lookup up front and pass the result here.
pub fn resolve_oidc_team_id(user: Option<&User>) -> Option<Uuid> {
    user.and_then(|u| u.team_id)
}

/// If the OIDC user has been deactivated (`active=false`), return a ready-to-send
/// 403 response. Otherwise return `None` so the caller proceeds with normal
/// dispatch.
///
/// Body shape mirrors `error_response` in `handlers.rs`:
///   { "type": "error", "error": { "type": "permission_error", "message": "..." } }
///
/// Message exactly matches the portal-side gate in `resolve_oidc_role`
/// (`handlers.rs:457-459`): "Your account has been deactivated".
pub fn oidc_inactive_response(user: Option<&User>) -> Option<Response<Body>> {
    if user.is_some_and(|u| !u.active) {
        let request_id = format!("req_{}", Uuid::new_v4().simple());
        let body = serde_json::to_string(&json!({
            "type": "error",
            "error": {
                "type": "permission_error",
                "message": "Your account has been deactivated"
            }
        }))
        .expect("static JSON serialization is infallible");
        Some(
            Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header("content-type", "application/json")
                .header("x-request-id", &request_id)
                .body(Body::from(body))
                .expect("response builder inputs are valid"),
        )
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn user(team_id: Option<Uuid>, active: bool) -> User {
        User {
            id: Uuid::new_v4(),
            email: "u@test.com".to_string(),
            team_id,
            role: "member".to_string(),
            spend_limit_monthly_usd: None,
            budget_period: "monthly".to_string(),
            created_at: Utc::now(),
            active,
            external_id: None,
            display_name: None,
            given_name: None,
            family_name: None,
            scim_managed: false,
            idp_id: None,
        }
    }

    #[test]
    fn resolves_team_when_set() {
        let team_id = Uuid::new_v4();
        let u = user(Some(team_id), true);
        assert_eq!(resolve_oidc_team_id(Some(&u)), Some(team_id));
    }

    #[test]
    fn resolves_none_when_no_team() {
        let u = user(None, true);
        assert_eq!(resolve_oidc_team_id(Some(&u)), None);
    }

    #[test]
    fn resolves_none_when_no_user() {
        assert_eq!(resolve_oidc_team_id(None), None);
    }

    #[test]
    fn inactive_user_yields_403() {
        let u = user(None, false);
        let resp = oidc_inactive_response(Some(&u)).expect("inactive yields response");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn active_user_yields_none() {
        let u = user(None, true);
        assert!(oidc_inactive_response(Some(&u)).is_none());
    }

    #[test]
    fn missing_user_yields_none() {
        assert!(oidc_inactive_response(None).is_none());
    }
}
