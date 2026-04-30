use sqlx::PgPool;
use uuid::Uuid;

pub struct RequestLogEntry {
    pub key_id: Option<Uuid>,
    pub user_identity: Option<String>,
    pub request_id: String,
    pub model: String,
    pub streaming: bool,
    pub duration_ms: i32,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_read_tokens: i32,
    pub cache_write_tokens: i32,
    pub stop_reason: Option<String>,
    pub tool_count: i16,
    pub tool_names: Vec<String>,
    pub turn_count: i16,
    pub thinking_enabled: bool,
    pub has_system_prompt: bool,
    // Session tracking fields
    pub session_id: Option<String>,
    pub project_key: Option<String>,
    pub tool_errors: Option<serde_json::Value>,
    pub has_correction: bool,
    pub content_block_types: Vec<String>,
    pub system_prompt_hash: Option<String>,
    pub detection_flags: Option<serde_json::Value>,
    pub endpoint_id: Option<Uuid>,
    pub client_tag: Option<String>,
}

pub async fn insert_batch(pool: &PgPool, entries: &[RequestLogEntry]) -> anyhow::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut query = String::from(
        "INSERT INTO spend_log (key_id, user_identity, request_id, model, streaming, duration_ms, \
         input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
         stop_reason, tool_count, tool_names, turn_count, thinking_enabled, has_system_prompt, \
         session_id, project_key, tool_errors, has_correction, content_block_types, system_prompt_hash, \
         detection_flags, endpoint_id, client_tag) VALUES ",
    );

    let cols = 25;
    for (i, _) in entries.iter().enumerate() {
        if i > 0 {
            query.push_str(", ");
        }
        let base = i * cols;
        query.push('(');
        for j in 0..cols {
            if j > 0 {
                query.push_str(", ");
            }
            query.push_str(&format!("${}", base + j + 1));
        }
        query.push(')');
    }

    let mut q = sqlx::query(&query);
    for entry in entries {
        q = q
            .bind(entry.key_id)
            .bind(&entry.user_identity)
            .bind(&entry.request_id)
            .bind(&entry.model)
            .bind(entry.streaming)
            .bind(entry.duration_ms)
            .bind(entry.input_tokens)
            .bind(entry.output_tokens)
            .bind(entry.cache_read_tokens)
            .bind(entry.cache_write_tokens)
            .bind(&entry.stop_reason)
            .bind(entry.tool_count)
            .bind(&entry.tool_names)
            .bind(entry.turn_count)
            .bind(entry.thinking_enabled)
            .bind(entry.has_system_prompt)
            .bind(&entry.session_id)
            .bind(&entry.project_key)
            .bind(&entry.tool_errors)
            .bind(entry.has_correction)
            .bind(&entry.content_block_types)
            .bind(&entry.system_prompt_hash)
            .bind(&entry.detection_flags)
            .bind(entry.endpoint_id)
            .bind(&entry.client_tag);
    }

    q.execute(pool).await?;
    Ok(())
}

/// Get monthly spend for a user (by identity string), returns approximate USD.
pub async fn get_user_monthly_spend_usd(pool: &PgPool, user_identity: &str) -> anyhow::Result<f64> {
    let row = sqlx::query_scalar::<_, Option<f64>>(
        r#"SELECT SUM(estimate_cost_usd(model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens))::float8
        FROM spend_log
        WHERE user_identity = $1
          AND recorded_at >= date_trunc('month', now())
        "#,
    )
    .bind(user_identity)
    .fetch_one(pool)
    .await?;
    Ok(row.unwrap_or(0.0))
}
