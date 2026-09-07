//! Database access for the governance overlay.
//!
//! Deliberately uses sqlx's **runtime** query API (`sqlx::query_as::<_, T>("...")`)
//! rather than the `query!` macros, so the extension needs no entries in `.sqlx/` and
//! `cargo build` works offline without regenerating the upstream query cache.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::{Action, AppliesTo, DeniedTool, Mode, PolicyRow, RequestContext};

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ServerRow {
    pub id: Uuid,
    pub server_name: String,
    pub status: String,
    pub notes: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ToolRow {
    pub id: Uuid,
    pub tool_name: String,
    pub server_name: Option<String>,
    pub status: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub seen_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PolicyDbRow {
    pub id: Uuid,
    pub scope: String,
    pub scope_ref: Option<String>,
    pub mode: String,
    pub default_action: String,
    pub applies_to: String,
    pub allow_patterns: serde_json::Value,
    pub deny_patterns: serde_json::Value,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<String>,
}

fn json_to_patterns(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

impl PolicyDbRow {
    /// Convert to the engine's representation, tolerating unknown enum strings by
    /// falling back to the safe (least restrictive) variant.
    pub fn to_policy(&self) -> PolicyRow {
        PolicyRow {
            scope: self.scope.clone(),
            scope_ref: self.scope_ref.clone(),
            mode: Mode::parse(&self.mode),
            default_action: Action::parse(&self.default_action),
            applies_to: AppliesTo::parse(&self.applies_to),
            allow_patterns: json_to_patterns(&self.allow_patterns),
            deny_patterns: json_to_patterns(&self.deny_patterns),
            enabled: self.enabled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EventRow {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub tool_name: String,
    pub server_name: Option<String>,
    pub decision: String,
    pub mode: String,
    pub reason: String,
    pub decided_by: Option<String>,
    pub user_identity: Option<String>,
    pub team_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub request_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

pub async fn load_policies(pool: &PgPool) -> anyhow::Result<Vec<PolicyDbRow>> {
    let rows = sqlx::query_as::<_, PolicyDbRow>(
        "SELECT * FROM mcpgov_policies ORDER BY scope, COALESCE(scope_ref, '')",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn load_servers(pool: &PgPool) -> anyhow::Result<Vec<ServerRow>> {
    let rows = sqlx::query_as::<_, ServerRow>("SELECT * FROM mcpgov_servers ORDER BY server_name")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn load_tools(pool: &PgPool) -> anyhow::Result<Vec<ToolRow>> {
    let rows = sqlx::query_as::<_, ToolRow>("SELECT * FROM mcpgov_tools ORDER BY tool_name")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Tools filtered for the approval queue, newest-seen first.
pub async fn list_tools_filtered(
    pool: &PgPool,
    status: Option<&str>,
    server: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<ToolRow>> {
    let rows = sqlx::query_as::<_, ToolRow>(
        r#"SELECT * FROM mcpgov_tools
           WHERE ($1::text IS NULL OR status = $1)
             AND ($2::text IS NULL OR server_name = $2)
           ORDER BY last_seen DESC, tool_name
           LIMIT $3"#,
    )
    .bind(status)
    .bind(server)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_events(
    pool: &PgPool,
    decision: Option<&str>,
    user_identity: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<EventRow>> {
    let rows = sqlx::query_as::<_, EventRow>(
        r#"SELECT * FROM mcpgov_events
           WHERE ($1::text IS NULL OR decision = $1)
             AND ($2::text IS NULL OR user_identity = $2)
           ORDER BY created_at DESC, id DESC
           LIMIT $3"#,
    )
    .bind(decision)
    .bind(user_identity)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub servers_total: i64,
    pub servers_pending: i64,
    pub servers_approved: i64,
    pub servers_denied: i64,
    pub tools_total: i64,
    pub tools_pending: i64,
    pub tools_approved: i64,
    pub tools_denied: i64,
    pub events_24h_blocked: i64,
    pub events_24h_would_block: i64,
    pub events_24h_warned: i64,
}

pub async fn summary(pool: &PgPool) -> anyhow::Result<Summary> {
    let (s_total, s_pending, s_approved, s_denied): (i64, i64, i64, i64) = sqlx::query_as(
        r#"SELECT count(*),
                  count(*) FILTER (WHERE status = 'pending'),
                  count(*) FILTER (WHERE status = 'approved'),
                  count(*) FILTER (WHERE status = 'denied')
           FROM mcpgov_servers"#,
    )
    .fetch_one(pool)
    .await?;

    // Tool 'pending' is spelled `inherit` in the tools table.
    let (t_total, t_pending, t_approved, t_denied): (i64, i64, i64, i64) = sqlx::query_as(
        r#"SELECT count(*),
                  count(*) FILTER (WHERE status = 'inherit'),
                  count(*) FILTER (WHERE status = 'approved'),
                  count(*) FILTER (WHERE status = 'denied')
           FROM mcpgov_tools"#,
    )
    .fetch_one(pool)
    .await?;

    let (blocked, would_block, warned): (i64, i64, i64) = sqlx::query_as(
        r#"SELECT count(*) FILTER (WHERE decision = 'blocked'),
                  count(*) FILTER (WHERE decision = 'would_block'),
                  count(*) FILTER (WHERE decision = 'warned')
           FROM mcpgov_events
           WHERE created_at > now() - interval '24 hours'"#,
    )
    .fetch_one(pool)
    .await?;

    Ok(Summary {
        servers_total: s_total,
        servers_pending: s_pending,
        servers_approved: s_approved,
        servers_denied: s_denied,
        tools_total: t_total,
        tools_pending: t_pending,
        tools_approved: t_approved,
        tools_denied: t_denied,
        events_24h_blocked: blocked,
        events_24h_would_block: would_block,
        events_24h_warned: warned,
    })
}

// ---------------------------------------------------------------------------
// Discovery writes (must NOT bump cache_version)
// ---------------------------------------------------------------------------

/// Upsert observed tool names into the catalog.
///
/// This runs off the hot path from a buffered writer and intentionally does **not**
/// call `bump_cache_version`: discovery happens continuously, and bumping the shared
/// version counter would invalidate every unrelated cache (keys, endpoints, model
/// mappings) on every replica each time a new tool name appeared.
pub async fn upsert_observed(
    pool: &PgPool,
    names: &[String],
    servers: &[Option<String>],
) -> anyhow::Result<()> {
    if names.is_empty() {
        return Ok(());
    }

    // Servers first, so the tools rows always have a catalog parent to inherit from.
    sqlx::query(
        r#"INSERT INTO mcpgov_servers (server_name)
           SELECT DISTINCT s FROM UNNEST($1::text[]) AS s WHERE s IS NOT NULL
           ON CONFLICT (server_name) DO UPDATE SET last_seen = now()"#,
    )
    .bind(servers)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"INSERT INTO mcpgov_tools (tool_name, server_name, seen_count)
           SELECT t.name, t.server, 1
           FROM UNNEST($1::text[], $2::text[]) AS t(name, server)
           ON CONFLICT (tool_name) DO UPDATE
             SET last_seen = now(),
                 seen_count = mcpgov_tools.seen_count + 1,
                 server_name = COALESCE(mcpgov_tools.server_name, EXCLUDED.server_name)"#,
    )
    .bind(names)
    .bind(servers)
    .execute(pool)
    .await?;

    Ok(())
}

/// Bulk-insert audit events. Also off the hot path, also no cache bump.
pub async fn insert_events(
    pool: &PgPool,
    batch: &[(DeniedTool, String, Mode, RequestContext)],
) -> anyhow::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let tool_names: Vec<String> = batch.iter().map(|(d, ..)| d.tool_name.clone()).collect();
    let server_names: Vec<Option<String>> =
        batch.iter().map(|(d, ..)| d.server_name.clone()).collect();
    let decisions: Vec<String> = batch.iter().map(|(_, dec, ..)| dec.clone()).collect();
    let modes: Vec<String> = batch
        .iter()
        .map(|(_, _, m, _)| m.as_str().to_string())
        .collect();
    let reasons: Vec<String> = batch.iter().map(|(d, ..)| d.reason.clone()).collect();
    let decided_by: Vec<Option<String>> = batch
        .iter()
        .map(|(d, ..)| Some(d.decided_by.clone()))
        .collect();
    let users: Vec<Option<String>> = batch
        .iter()
        .map(|(_, _, _, c)| c.user_identity.clone())
        .collect();
    let teams: Vec<Option<Uuid>> = batch.iter().map(|(_, _, _, c)| c.team_id).collect();
    let keys: Vec<Option<Uuid>> = batch.iter().map(|(_, _, _, c)| c.key_id).collect();
    let request_ids: Vec<Option<String>> = batch
        .iter()
        .map(|(_, _, _, c)| c.request_id.clone())
        .collect();

    sqlx::query(
        r#"INSERT INTO mcpgov_events
             (tool_name, server_name, decision, mode, reason, decided_by,
              user_identity, team_id, key_id, request_id)
           SELECT * FROM UNNEST(
             $1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[],
             $7::text[], $8::uuid[], $9::uuid[], $10::text[]
           )"#,
    )
    .bind(&tool_names)
    .bind(&server_names)
    .bind(&decisions)
    .bind(&modes)
    .bind(&reasons)
    .bind(&decided_by)
    .bind(&users)
    .bind(&teams)
    .bind(&keys)
    .bind(&request_ids)
    .execute(pool)
    .await?;

    Ok(())
}

/// Trim the audit log. Called by the sink's periodic maintenance so the table cannot
/// grow without bound in observe mode, where every request may log a would_block.
pub async fn prune_events(pool: &PgPool, keep_days: i64) -> anyhow::Result<u64> {
    let res = sqlx::query(
        "DELETE FROM mcpgov_events WHERE created_at < now() - ($1 || ' days')::interval",
    )
    .bind(keep_days.to_string())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ---------------------------------------------------------------------------
// Operator writes (these DO bump cache_version so all replicas converge in <=5s)
// ---------------------------------------------------------------------------

pub async fn set_server_status(
    pool: &PgPool,
    server_name: &str,
    status: &str,
    notes: Option<&str>,
) -> anyhow::Result<bool> {
    let res = sqlx::query(
        r#"UPDATE mcpgov_servers
           SET status = $2, notes = COALESCE($3, notes), updated_at = now()
           WHERE server_name = $1"#,
    )
    .bind(server_name)
    .bind(status)
    .bind(notes)
    .execute(pool)
    .await?;

    if res.rows_affected() > 0 {
        crate::db::settings::bump_cache_version(pool).await?;
        return Ok(true);
    }
    Ok(false)
}

pub async fn set_tool_status(pool: &PgPool, tool_name: &str, status: &str) -> anyhow::Result<bool> {
    let res = sqlx::query("UPDATE mcpgov_tools SET status = $2 WHERE tool_name = $1")
        .bind(tool_name)
        .bind(status)
        .execute(pool)
        .await?;

    if res.rows_affected() > 0 {
        crate::db::settings::bump_cache_version(pool).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Approve or deny every tool belonging to a server in one shot, for bulk triage.
pub async fn set_status_for_server_tools(
    pool: &PgPool,
    server_name: &str,
    status: &str,
) -> anyhow::Result<u64> {
    let res = sqlx::query("UPDATE mcpgov_tools SET status = $2 WHERE server_name = $1")
        .bind(server_name)
        .bind(status)
        .execute(pool)
        .await?;
    crate::db::settings::bump_cache_version(pool).await?;
    Ok(res.rows_affected())
}

/// Create a server row ahead of discovery, so operators can pre-approve a catalog.
pub async fn create_server(
    pool: &PgPool,
    server_name: &str,
    status: &str,
    notes: Option<&str>,
) -> anyhow::Result<ServerRow> {
    let row = sqlx::query_as::<_, ServerRow>(
        r#"INSERT INTO mcpgov_servers (server_name, status, notes)
           VALUES ($1, $2, $3)
           ON CONFLICT (server_name) DO UPDATE
             SET status = $2, notes = COALESCE($3, mcpgov_servers.notes), updated_at = now()
           RETURNING *"#,
    )
    .bind(server_name)
    .bind(status)
    .bind(notes)
    .fetch_one(pool)
    .await?;

    crate::db::settings::bump_cache_version(pool).await?;
    Ok(row)
}

#[allow(clippy::too_many_arguments)]
pub async fn upsert_policy(
    pool: &PgPool,
    scope: &str,
    scope_ref: Option<&str>,
    mode: &str,
    default_action: &str,
    applies_to: &str,
    allow_patterns: &[String],
    deny_patterns: &[String],
    enabled: bool,
    updated_by: Option<&str>,
) -> anyhow::Result<PolicyDbRow> {
    let allow = serde_json::to_value(allow_patterns)?;
    let deny = serde_json::to_value(deny_patterns)?;

    let row = sqlx::query_as::<_, PolicyDbRow>(
        r#"INSERT INTO mcpgov_policies
             (scope, scope_ref, mode, default_action, applies_to,
              allow_patterns, deny_patterns, enabled, updated_by, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
           ON CONFLICT (scope, COALESCE(scope_ref, '')) DO UPDATE
             SET mode = $3, default_action = $4, applies_to = $5,
                 allow_patterns = $6, deny_patterns = $7, enabled = $8,
                 updated_by = $9, updated_at = now()
           RETURNING *"#,
    )
    .bind(scope)
    .bind(scope_ref)
    .bind(mode)
    .bind(default_action)
    .bind(applies_to)
    .bind(&allow)
    .bind(&deny)
    .bind(enabled)
    .bind(updated_by)
    .fetch_one(pool)
    .await?;

    crate::db::settings::bump_cache_version(pool).await?;
    Ok(row)
}

pub async fn delete_policy(pool: &PgPool, id: Uuid) -> anyhow::Result<bool> {
    // The global policy is the base layer every other scope inherits from; deleting
    // it would silently change the meaning of every team and user policy.
    let res = sqlx::query("DELETE FROM mcpgov_policies WHERE id = $1 AND scope <> 'global'")
        .bind(id)
        .execute(pool)
        .await?;

    if res.rows_affected() > 0 {
        crate::db::settings::bump_cache_version(pool).await?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_patterns_parse_string_arrays() {
        let v = serde_json::json!(["a", "b"]);
        assert_eq!(json_to_patterns(&v), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn json_patterns_tolerate_junk() {
        // Non-arrays and non-string members must not panic or poison the policy.
        assert!(json_to_patterns(&serde_json::json!(null)).is_empty());
        assert!(json_to_patterns(&serde_json::json!({"a": 1})).is_empty());
        assert_eq!(
            json_to_patterns(&serde_json::json!(["ok", 5, null])),
            vec!["ok".to_string()]
        );
    }

    #[test]
    fn policy_row_conversion_falls_back_safely() {
        let row = PolicyDbRow {
            id: Uuid::nil(),
            scope: "global".into(),
            scope_ref: None,
            mode: "bogus".into(),
            default_action: "bogus".into(),
            applies_to: "bogus".into(),
            allow_patterns: serde_json::json!([]),
            deny_patterns: serde_json::json!(["mcp__x__*"]),
            enabled: true,
            updated_at: Utc::now(),
            updated_by: None,
        };
        let p = row.to_policy();
        assert_eq!(p.mode, Mode::Observe);
        assert_eq!(p.default_action, Action::Allow);
        assert_eq!(p.applies_to, AppliesTo::McpOnly);
        assert_eq!(p.deny_patterns, vec!["mcp__x__*".to_string()]);
    }
}
