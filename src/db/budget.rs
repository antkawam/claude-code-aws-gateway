use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::budget::BudgetPeriod;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BudgetEvent {
    pub id: i64,
    pub user_identity: Option<String>,
    pub team_id: Option<Uuid>,
    pub event_type: String,
    pub threshold_percent: i32,
    pub spend_usd: f64,
    pub limit_usd: f64,
    pub percent: f64,
    pub period: String,
    pub period_start: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
}

/// Get period-aware spend for a user.
pub async fn get_user_spend(
    pool: &PgPool,
    user_identity: &str,
    period: BudgetPeriod,
) -> anyhow::Result<f64> {
    let row = sqlx::query_scalar::<_, Option<f64>>(
        &format!(
            r#"SELECT SUM(estimate_cost_usd(model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens))::float8
            FROM spend_log
            WHERE user_identity = $1
              AND recorded_at >= date_trunc('{}', now() AT TIME ZONE 'UTC')
            "#,
            period.trunc_arg()
        ),
    )
    .bind(user_identity)
    .fetch_one(pool)
    .await?;
    Ok(row.unwrap_or(0.0))
}

/// Get period-aware spend for a team (all users in the team).
pub async fn get_team_spend(
    pool: &PgPool,
    team_id: Uuid,
    period: BudgetPeriod,
) -> anyhow::Result<f64> {
    let row = sqlx::query_scalar::<_, Option<f64>>(
        &format!(
            r#"SELECT SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8
            FROM spend_log sl
            JOIN users u ON u.email = sl.user_identity
            WHERE u.team_id = $1
              AND sl.recorded_at >= date_trunc('{}', now() AT TIME ZONE 'UTC')
            "#,
            period.trunc_arg()
        ),
    )
    .bind(team_id)
    .fetch_one(pool)
    .await?;
    Ok(row.unwrap_or(0.0))
}

/// Insert a budget event (deduplicates: same threshold fires at most once per period per user/team).
#[allow(clippy::too_many_arguments)]
pub async fn insert_event(
    pool: &PgPool,
    user_identity: Option<&str>,
    team_id: Option<Uuid>,
    event_type: &str,
    threshold_percent: i32,
    spend_usd: f64,
    limit_usd: f64,
    percent: f64,
    period: &str,
    period_start: DateTime<Utc>,
) -> anyhow::Result<bool> {
    // Dedup check
    let existing = sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*) FROM budget_events
        WHERE user_identity IS NOT DISTINCT FROM $1
          AND team_id IS NOT DISTINCT FROM $2
          AND threshold_percent = $3
          AND period_start = $4"#,
    )
    .bind(user_identity)
    .bind(team_id)
    .bind(threshold_percent)
    .bind(period_start)
    .fetch_one(pool)
    .await?;

    if existing > 0 {
        return Ok(false); // Already fired this threshold this period
    }

    sqlx::query(
        r#"INSERT INTO budget_events
        (user_identity, team_id, event_type, threshold_percent, spend_usd, limit_usd, percent, period, period_start)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#,
    )
    .bind(user_identity)
    .bind(team_id)
    .bind(event_type)
    .bind(threshold_percent)
    .bind(spend_usd)
    .bind(limit_usd)
    .bind(percent)
    .bind(period)
    .bind(period_start)
    .execute(pool)
    .await?;

    Ok(true)
}

/// Get undelivered budget events for the notification delivery loop.
pub async fn get_undelivered_events(pool: &PgPool, limit: i64) -> anyhow::Result<Vec<BudgetEvent>> {
    let events = sqlx::query_as::<_, BudgetEvent>(
        "SELECT * FROM budget_events WHERE delivered_at IS NULL ORDER BY created_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(events)
}

/// Mark a budget event as delivered.
pub async fn mark_delivered(pool: &PgPool, event_id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE budget_events SET delivered_at = now() WHERE id = $1")
        .bind(event_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Get recent budget events for a user (for the budget status API).
pub async fn get_user_events(
    pool: &PgPool,
    user_identity: &str,
    limit: i64,
) -> anyhow::Result<Vec<BudgetEvent>> {
    let events = sqlx::query_as::<_, BudgetEvent>(
        "SELECT * FROM budget_events WHERE user_identity = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(user_identity)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(events)
}

/// Get team spend analytics for admin overview.
#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct TeamSpendSummary {
    pub team_id: Uuid,
    pub team_name: String,
    pub budget_amount_usd: Option<f64>,
    pub budget_period: String,
    pub default_user_budget_usd: Option<f64>,
    pub current_spend_usd: Option<f64>,
    pub user_count: Option<i64>,
}

pub async fn get_analytics_overview(pool: &PgPool) -> anyhow::Result<Vec<TeamSpendSummary>> {
    let rows = sqlx::query_as::<_, TeamSpendSummary>(
        r#"SELECT
            t.id as team_id,
            t.name as team_name,
            t.budget_amount_usd,
            t.budget_period,
            t.default_user_budget_usd,
            COALESCE(
                SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens)),
                0
            )::float8 as current_spend_usd,
            (SELECT COUNT(*) FROM users WHERE team_id = t.id)::bigint as user_count
        FROM teams t
        LEFT JOIN users u ON u.team_id = t.id
        LEFT JOIN spend_log sl ON sl.user_identity = u.email
            AND sl.recorded_at >= date_trunc(
                CASE t.budget_period
                    WHEN 'daily' THEN 'day'
                    WHEN 'weekly' THEN 'week'
                    ELSE 'month'
                END,
                now() AT TIME ZONE 'UTC'
            )
        GROUP BY t.id, t.name, t.budget_amount_usd, t.budget_period, t.default_user_budget_usd
        ORDER BY current_spend_usd DESC NULLS LAST"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Get team analytics detail: per-user spend breakdown.
#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct UserSpendInTeam {
    pub email: String,
    pub spend_limit_monthly_usd: Option<f64>,
    pub budget_period: String,
    pub current_spend_usd: Option<f64>,
    pub request_count: Option<i64>,
}

pub async fn get_team_analytics(
    pool: &PgPool,
    team_id: Uuid,
) -> anyhow::Result<Vec<UserSpendInTeam>> {
    let rows = sqlx::query_as::<_, UserSpendInTeam>(
        r#"SELECT
            u.email,
            u.spend_limit_monthly_usd,
            u.budget_period,
            COALESCE(
                SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens)),
                0
            )::float8 as current_spend_usd,
            COUNT(sl.id)::bigint as request_count
        FROM users u
        LEFT JOIN spend_log sl ON sl.user_identity = u.email
            AND sl.recorded_at >= date_trunc(
                CASE u.budget_period
                    WHEN 'daily' THEN 'day'
                    WHEN 'weekly' THEN 'week'
                    ELSE 'month'
                END,
                now() AT TIME ZONE 'UTC'
            )
        WHERE u.team_id = $1
        GROUP BY u.id, u.email, u.spend_limit_monthly_usd, u.budget_period
        ORDER BY current_spend_usd DESC NULLS LAST"#,
    )
    .bind(team_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// CSV export: all spend data for a date range.
#[derive(Debug, sqlx::FromRow)]
pub struct SpendExportRow {
    pub recorded_at: DateTime<Utc>,
    pub user_identity: Option<String>,
    pub team_name: Option<String>,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_read_tokens: i32,
    pub cache_write_tokens: i32,
    pub cost_usd: Option<f64>,
    pub duration_ms: i32,
}

pub async fn get_spend_export(pool: &PgPool, days: i32) -> anyhow::Result<Vec<SpendExportRow>> {
    let rows = sqlx::query_as::<_, SpendExportRow>(
        r#"SELECT
            sl.recorded_at,
            sl.user_identity,
            t.name as team_name,
            sl.model,
            sl.input_tokens,
            sl.output_tokens,
            sl.cache_read_tokens,
            sl.cache_write_tokens,
            estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens)::float8 as cost_usd,
            sl.duration_ms
        FROM spend_log sl
        LEFT JOIN users u ON u.email = sl.user_identity
        LEFT JOIN teams t ON t.id = u.team_id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        ORDER BY sl.recorded_at DESC"#,
    )
    .bind(days.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
