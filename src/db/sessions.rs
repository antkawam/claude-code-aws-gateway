use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// A materialized session summary row.
#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct Session {
    pub id: Uuid,
    pub session_id: String,
    pub user_identity: String,
    pub project_key: Option<String>,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub end_time: chrono::DateTime<chrono::Utc>,
    pub duration_minutes: Option<f64>,
    pub request_count: i32,
    pub turn_count: Option<i32>,
    pub total_cost_usd: Option<f64>,
    pub models_used: Option<Vec<String>>,
    pub tools_used: Option<serde_json::Value>,
    pub correction_count: Option<i32>,
    pub error_count: Option<i32>,
    pub cache_hit_rate: Option<f64>,
    pub facets: Option<serde_json::Value>,
    pub analyzed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub aggregated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub flag_categories: Option<Vec<String>>,
    pub flag_count: Option<i32>,
}

/// Materialize sessions from spend_log rows that have session_id set.
/// Returns the number of new sessions created.
pub async fn materialize_sessions(pool: &PgPool) -> anyhow::Result<usize> {
    // Find session_ids in spend_log that don't have a sessions row yet.
    // Only consider rows from the last 2 days to keep the query bounded.
    let rows_affected = sqlx::query(r#"
        INSERT INTO sessions (session_id, user_identity, project_key, start_time, end_time,
            duration_minutes, request_count, turn_count, total_cost_usd, models_used,
            tools_used, correction_count, error_count, cache_hit_rate,
            flag_categories, flag_count)
        SELECT
            agg.session_id,
            agg.user_identity,
            agg.project_key,
            agg.start_time,
            agg.end_time,
            agg.duration_minutes,
            agg.request_count,
            agg.turn_count,
            agg.total_cost_usd,
            agg.models_used,
            tu.tools_used,
            agg.correction_count,
            agg.error_count,
            agg.cache_hit_rate,
            df.flag_categories,
            df.flag_count
        FROM (
            SELECT
                sl.session_id,
                sl.user_identity,
                mode() WITHIN GROUP (ORDER BY sl.project_key) as project_key,
                min(sl.recorded_at) as start_time,
                max(sl.recorded_at) as end_time,
                EXTRACT(EPOCH FROM max(sl.recorded_at) - min(sl.recorded_at)) / 60.0 as duration_minutes,
                count(*) as request_count,
                max(sl.turn_count)::int as turn_count,
                SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens)) as total_cost_usd,
                array_agg(DISTINCT sl.model) as models_used,
                SUM(CASE WHEN sl.has_correction THEN 1 ELSE 0 END)::int as correction_count,
                SUM(COALESCE(jsonb_array_length(sl.tool_errors), 0))::int as error_count,
                CASE WHEN SUM(sl.input_tokens) > 0
                    THEN SUM(sl.cache_read_tokens)::float / SUM(sl.input_tokens + sl.cache_read_tokens)::float
                    ELSE 0 END as cache_hit_rate
            FROM spend_log sl
            WHERE sl.session_id IS NOT NULL
              AND sl.user_identity IS NOT NULL
              AND sl.recorded_at >= now() - interval '2 days'
              AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.session_id = sl.session_id)
            GROUP BY sl.session_id, sl.user_identity
            HAVING count(*) >= 2
        ) agg
        LEFT JOIN LATERAL (
            SELECT jsonb_object_agg(tool_name, tool_cnt) as tools_used
            FROM (
                SELECT t.tool_name, count(*) as tool_cnt
                FROM spend_log sl2, unnest(sl2.tool_names) as t(tool_name)
                WHERE sl2.session_id = agg.session_id
                GROUP BY t.tool_name
            ) tc
        ) tu ON true
        LEFT JOIN LATERAL (
            SELECT
                array_agg(DISTINCT cat) FILTER (WHERE cat IS NOT NULL) as flag_categories,
                COALESCE(SUM(cnt), 0)::int as flag_count
            FROM (
                SELECT
                    f->>'category' as cat,
                    1 as cnt
                FROM spend_log sl3,
                     jsonb_array_elements(sl3.detection_flags) as f
                WHERE sl3.session_id = agg.session_id
                  AND sl3.detection_flags IS NOT NULL
            ) fc
        ) df ON true
        ON CONFLICT (session_id) DO NOTHING
    "#)
    .execute(pool)
    .await?;

    Ok(rows_affected.rows_affected() as usize)
}

/// List sessions for a user, most recent first.
pub async fn list_sessions(
    pool: &PgPool,
    user_identity: &str,
    limit: i64,
) -> anyhow::Result<Vec<Session>> {
    let sessions = sqlx::query_as::<_, Session>(
        "SELECT * FROM sessions WHERE user_identity = $1 ORDER BY end_time DESC LIMIT $2",
    )
    .bind(user_identity)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(sessions)
}

/// Get unanalyzed sessions that have detection flags, ordered by most recent.
/// Only sessions with flag_count > 0 are sent to Tier 2 LLM analysis.
pub async fn get_unanalyzed_sessions(pool: &PgPool, limit: i64) -> anyhow::Result<Vec<Session>> {
    let sessions = sqlx::query_as::<_, Session>(
        "SELECT * FROM sessions WHERE analyzed_at IS NULL AND request_count >= 3 \
         AND COALESCE(flag_count, 0) > 0 \
         ORDER BY end_time DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(sessions)
}

/// Store facets for a session after LLM analysis.
pub async fn update_session_facets(
    pool: &PgPool,
    session_id: &str,
    facets: &serde_json::Value,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE sessions SET facets = $1, analyzed_at = now() WHERE session_id = $2")
        .bind(facets)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Get spend_log rows for a specific session (for building analysis summaries).
#[derive(Debug, sqlx::FromRow, Serialize)]
pub struct SpendLogRow {
    pub model: String,
    pub duration_ms: Option<i32>,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_read_tokens: i32,
    pub cache_write_tokens: i32,
    pub stop_reason: Option<String>,
    pub tool_count: i16,
    pub tool_names: Option<Vec<String>>,
    pub turn_count: i16,
    pub thinking_enabled: bool,
    pub tool_errors: Option<serde_json::Value>,
    pub has_correction: bool,
    pub content_block_types: Option<Vec<String>>,
    pub recorded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub detection_flags: Option<serde_json::Value>,
}

pub async fn get_session_events(
    pool: &PgPool,
    session_id: &str,
) -> anyhow::Result<Vec<SpendLogRow>> {
    let rows = sqlx::query_as::<_, SpendLogRow>(
        "SELECT model, duration_ms, input_tokens, output_tokens, cache_read_tokens, \
         cache_write_tokens, stop_reason, tool_count, tool_names, turn_count, \
         thinking_enabled, tool_errors, has_correction, content_block_types, recorded_at, \
         detection_flags \
         FROM spend_log WHERE session_id = $1 ORDER BY recorded_at",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
