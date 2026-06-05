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
}

/// SpendDb is the abstraction over the spend-log persistence layer used by the
/// flush loop. It is implemented for a `&PgPool` adapter in production and for
/// scripted mocks in tests so flush behaviour (batch + per-record fallback)
/// can be exercised without a real Postgres.
#[async_trait::async_trait]
pub trait SpendDb: Send + Sync {
    async fn insert_batch(&self, entries: &[RequestLogEntry]) -> Result<(), sqlx::Error>;
    async fn insert_one(&self, entry: &RequestLogEntry) -> Result<(), sqlx::Error>;
}

/// Pool-backed SpendDb adapter used in production.
pub struct PoolSpendDb<'a> {
    pub pool: &'a PgPool,
}

#[async_trait::async_trait]
impl<'a> SpendDb for PoolSpendDb<'a> {
    async fn insert_batch(&self, entries: &[RequestLogEntry]) -> Result<(), sqlx::Error> {
        insert_batch(self.pool, entries).await
    }

    async fn insert_one(&self, entry: &RequestLogEntry) -> Result<(), sqlx::Error> {
        insert_one(self.pool, entry).await
    }
}

/// Classify a `sqlx::Error` as transient (re-buffer for retry) vs. data
/// rejection (drop + quarantine).
///
/// Rules:
/// - `PoolTimedOut`, `Io(_)` → transient (connection-level / pool-level).
/// - `Database(_)` with a SQLSTATE starting with `22` (data exception, e.g.
///   `22P05` untranslatable character, `22021` invalid byte sequence) →
///   data rejection: NOT transient.
/// - `Database(_)` with any other code (e.g. `08xxx` connection class) or
///   no code → transient.
/// - All other `sqlx::Error` variants → transient (safe default).
///
/// Documented default: an UNKNOWN SQLSTATE (one we don't recognise as a data
/// exception) is treated as TRANSIENT — we'd rather re-buffer and retry than
/// silently drop spend data on an error class we haven't classified. This
/// matches the spec's "Decision rule for ambiguous/unknown errors": err on
/// the side of preserving data.
pub fn is_transient_db_error(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::PoolTimedOut => true,
        sqlx::Error::Io(_) => true,
        sqlx::Error::Database(db_err) => {
            match db_err.code() {
                Some(code) if code.starts_with("22") => false, // data exception class → drop
                _ => true,                                     // anything else → transient
            }
        }
        // Safe default: any other sqlx::Error variant is treated as transient.
        // Better to re-buffer and retry than silently drop spend data on an
        // error variant we haven't explicitly classified.
        _ => true,
    }
}

pub async fn insert_batch(pool: &PgPool, entries: &[RequestLogEntry]) -> Result<(), sqlx::Error> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut query = String::from(
        "INSERT INTO spend_log (key_id, user_identity, request_id, model, streaming, duration_ms, \
         input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
         stop_reason, tool_count, tool_names, turn_count, thinking_enabled, has_system_prompt, \
         session_id, project_key, tool_errors, has_correction, content_block_types, system_prompt_hash, detection_flags, endpoint_id) VALUES ",
    );

    let cols = 24;
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
            .bind(entry.endpoint_id);
    }

    q.execute(pool).await?;
    Ok(())
}

/// Insert a single spend-log row. Surfaces the raw `sqlx::Error` so the
/// flush loop can classify the failure (transient vs. data rejection) via
/// `is_transient_db_error`.
pub async fn insert_one(pool: &PgPool, entry: &RequestLogEntry) -> Result<(), sqlx::Error> {
    insert_batch(pool, std::slice::from_ref(entry)).await
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
