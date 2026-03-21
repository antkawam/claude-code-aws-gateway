use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct OrgAnalyticsFilter {
    pub days: i32,
    pub granularity: String, // "auto"|"minute"|"hour"|"day"|"week"
    pub team: Option<String>,
    pub user: Option<String>,
    pub model: Option<String>,    // comma-separated for multi-select
    pub endpoint: Option<String>, // comma-separated for multi-select
    pub from: Option<String>,     // ISO date "2026-03-01"
    pub to: Option<String>,       // ISO date "2026-03-18"
}

// ---------------------------------------------------------------------------
// Filter builder helper
// ---------------------------------------------------------------------------

struct FilterBuilder {
    clauses: Vec<String>,
    param_idx: usize,
}

impl FilterBuilder {
    fn new(start_idx: usize) -> Self {
        Self {
            clauses: Vec::new(),
            param_idx: start_idx,
        }
    }

    fn next_param(&mut self) -> String {
        let p = format!("${}", self.param_idx);
        self.param_idx += 1;
        p
    }

    fn add(&mut self, clause: String) {
        self.clauses.push(clause);
    }

    fn where_clause(&self) -> String {
        if self.clauses.is_empty() {
            String::new()
        } else {
            self.clauses.join(" ")
        }
    }
}

fn resolve_granularity(filter: &OrgAnalyticsFilter) -> &str {
    match filter.granularity.as_str() {
        "hour" => "hour",
        "day" => "day",
        "week" => "week",
        _ => {
            // auto
            if filter.days <= 7 {
                "hour"
            } else if filter.days <= 90 {
                "day"
            } else {
                "week"
            }
        }
    }
}

/// Returns true if the filter uses an absolute date range instead of relative days.
fn _uses_date_range(filter: &OrgAnalyticsFilter) -> bool {
    filter.from.is_some() && filter.to.is_some()
}

/// Build the time-range WHERE clause and its bind value(s).
/// Returns (clause, bind_values, next_param_idx).
fn _build_time_clause(filter: &OrgAnalyticsFilter) -> (String, Vec<String>, usize) {
    if let (Some(from), Some(to)) = (&filter.from, &filter.to) {
        // Absolute range: $1 = from, $2 = to
        (
            "sl.recorded_at >= $1::timestamptz AND sl.recorded_at < ($2::timestamptz + interval '1 day')".to_string(),
            vec![from.clone(), to.clone()],
            3,
        )
    } else {
        // Relative days: $1 = days string
        (
            "sl.recorded_at >= now() - ($1 || ' days')::interval".to_string(),
            vec![filter.days.to_string()],
            2,
        )
    }
}

/// Build the prior-period time clause for delta calculations.
fn _build_prior_time_clause(filter: &OrgAnalyticsFilter) -> String {
    if filter.from.is_some() && filter.to.is_some() {
        // Prior period = same duration before 'from'
        "sl.recorded_at >= $1::timestamptz - ($2::timestamptz + interval '1 day' - $1::timestamptz) AND sl.recorded_at < $1::timestamptz".to_string()
    } else {
        "sl.recorded_at >= now() - ($1 || ' days')::interval * 2 AND sl.recorded_at < now() - ($1 || ' days')::interval".to_string()
    }
}

/// Build optional filter clauses and return the bind values in order.
/// The first parameter ($1) is always the days string, already handled by the caller.
fn build_optional_filters(filter: &OrgAnalyticsFilter) -> (FilterBuilder, Vec<String>) {
    let mut fb = FilterBuilder::new(2); // $1 is days
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref team) = filter.team
        && !team.is_empty()
    {
        let teams: Vec<&str> = team
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        let has_none = teams.contains(&"__none__");
        let named: Vec<&&str> = teams.iter().filter(|t| **t != "__none__").collect();
        if has_none && named.is_empty() {
            // Only "No Team" selected
            fb.add("AND t.id IS NULL".to_string());
        } else if has_none && !named.is_empty() {
            // "No Team" + some named teams
            let placeholders: Vec<String> = named.iter().map(|_| fb.next_param()).collect();
            fb.add(format!(
                "AND (t.id IS NULL OR t.name IN ({}))",
                placeholders.join(",")
            ));
            for n in named {
                binds.push(n.to_string());
            }
        } else if named.len() == 1 {
            let p = fb.next_param();
            fb.add(format!("AND t.name = {p}"));
            binds.push(named[0].to_string());
        } else if named.len() > 1 {
            let placeholders: Vec<String> = named.iter().map(|_| fb.next_param()).collect();
            fb.add(format!("AND t.name IN ({})", placeholders.join(",")));
            for n in named {
                binds.push(n.to_string());
            }
        }
    }
    if let Some(ref user) = filter.user
        && !user.is_empty()
    {
        // Support comma-separated multi-select
        let users: Vec<&str> = user
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if users.len() == 1 {
            let p = fb.next_param();
            fb.add(format!("AND sl.user_identity = {p}"));
            binds.push(users[0].to_string());
        } else if users.len() > 1 {
            let placeholders: Vec<String> = users.iter().map(|_| fb.next_param()).collect();
            fb.add(format!(
                "AND sl.user_identity IN ({})",
                placeholders.join(",")
            ));
            for u in users {
                binds.push(u.to_string());
            }
        }
    }
    if let Some(ref model) = filter.model
        && !model.is_empty()
    {
        // Support comma-separated multi-select
        let models: Vec<&str> = model
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if models.len() == 1 {
            let p = fb.next_param();
            fb.add(format!("AND sl.model = {p}"));
            binds.push(models[0].to_string());
        } else if models.len() > 1 {
            let placeholders: Vec<String> = models.iter().map(|_| fb.next_param()).collect();
            fb.add(format!("AND sl.model IN ({})", placeholders.join(",")));
            for m in models {
                binds.push(m.to_string());
            }
        }
    }
    if let Some(ref endpoint) = filter.endpoint
        && !endpoint.is_empty()
    {
        // Support comma-separated multi-select
        let endpoints: Vec<&str> = endpoint
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if endpoints.len() == 1 {
            let p = fb.next_param();
            fb.add(format!("AND e.name = {p}"));
            binds.push(endpoints[0].to_string());
        } else if endpoints.len() > 1 {
            let placeholders: Vec<String> = endpoints.iter().map(|_| fb.next_param()).collect();
            fb.add(format!("AND e.name IN ({})", placeholders.join(",")));
            for ep in endpoints {
                binds.push(ep.to_string());
            }
        }
    }

    (fb, binds)
}

/// Bind optional filter values to a query in order.
macro_rules! bind_filters {
    ($q:expr, $binds:expr) => {{
        let mut q = $q;
        for b in $binds.iter() {
            q = q.bind(b);
        }
        q
    }};
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow, Serialize)]
pub struct OrgOverviewRow {
    pub requests: Option<i64>,
    pub unique_users: Option<i64>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub avg_duration_ms: Option<i32>,
    pub prior_requests: Option<i64>,
    pub prior_cost_usd: Option<f64>,
    pub prior_unique_users: Option<i64>,
}

#[derive(Serialize)]
pub struct OrgFilterOptions {
    pub teams: Vec<String>,
    pub users: Vec<String>,
    pub models: Vec<String>,
    pub endpoints: Vec<String>,
    pub projects: Vec<String>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct SpendTimePoint {
    pub team_name: Option<String>,
    pub bucket: DateTime<Utc>,
    pub cost_usd: Option<f64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct TeamSpendRow {
    pub team_name: Option<String>,
    pub cost_usd: Option<f64>,
    pub request_count: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct UserSpendRow {
    pub user_identity: Option<String>,
    pub team_name: Option<String>,
    pub cost_usd: Option<f64>,
    pub request_count: Option<i64>,
    pub total_tokens: Option<i64>,
    pub last_active: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct ModelSpendRow {
    pub model: String,
    pub cost_usd: Option<f64>,
    pub request_count: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct BudgetStatusRow {
    pub team_name: String,
    pub budget_usd: Option<f64>,
    pub spent_usd: Option<f64>,
    pub period: String,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct ForecastPoint {
    pub day: String,
    pub cost_usd: Option<f64>,
    pub is_forecast: bool,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct ActiveUsersPoint {
    pub bucket: DateTime<Utc>,
    pub new_users: Option<i64>,
    pub returning_users: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct HistogramBin {
    pub bucket_label: String,
    pub count: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct HeatmapCell {
    pub hour: Option<i32>,
    pub day_of_week: Option<i32>,
    pub request_count: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct ProjectActivityRow {
    pub project_key: Option<String>,
    pub request_count: Option<i64>,
    pub unique_users: Option<i64>,
    pub cost_usd: Option<f64>,
    pub last_active: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct UserSessionRow {
    pub user_identity: Option<String>,
    pub session_count: Option<i64>,
    pub request_count: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct ModelMixRow {
    pub model: String,
    pub request_count: Option<i64>,
    pub cost_usd: Option<f64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct LatencyPctRow {
    pub model: String,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct CacheRatePoint {
    pub bucket: DateTime<Utc>,
    pub cache_hit_rate: Option<f64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct TokenBreakdownRow {
    pub model: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct StopReasonRow {
    pub stop_reason: Option<String>,
    pub count: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct EndpointUtilRow {
    pub endpoint_name: Option<String>,
    pub request_count: Option<i64>,
    pub cost_usd: Option<f64>,
    pub avg_duration_ms: Option<f64>,
    pub error_rate: Option<f64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct McpServerRow {
    pub server_name: Option<String>,
    pub unique_tools: Option<i64>,
    pub usage_count: Option<i64>,
    pub unique_users: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct TopToolRow {
    pub tool_name: Option<String>,
    pub usage_count: Option<i64>,
    pub unique_users: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct McpAdoptionPoint {
    pub bucket: DateTime<Utc>,
    pub mcp_users: Option<i64>,
    pub total_users: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct ToolTotalsRow {
    pub total_tool_calls: Option<i64>,
    pub web_search_count: Option<i64>,
    pub mcp_tool_count: Option<i64>,
    pub unique_mcp_servers: Option<i64>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct OrgExportRow {
    pub recorded_at: DateTime<Utc>,
    pub user_identity: Option<String>,
    pub team_name: Option<String>,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_read_tokens: i32,
    pub cache_write_tokens: i32,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i32>,
    pub tool_count: i16,
    pub endpoint_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

pub async fn org_overview(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<OrgOverviewRow> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            COUNT(*)::bigint AS requests,
            COUNT(DISTINCT sl.user_identity)::bigint AS unique_users,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd,
            SUM(sl.input_tokens)::bigint AS input_tokens,
            SUM(sl.output_tokens)::bigint AS output_tokens,
            SUM(sl.cache_read_tokens)::bigint AS cache_read_tokens,
            SUM(sl.cache_write_tokens)::bigint AS cache_write_tokens,
            AVG(sl.duration_ms)::integer AS avg_duration_ms,
            -- Prior period
            (SELECT COUNT(*)::bigint FROM spend_log sl2
             LEFT JOIN users u2 ON sl2.user_identity = u2.email
             LEFT JOIN teams t2 ON u2.team_id = t2.id
             LEFT JOIN endpoints e2 ON sl2.endpoint_id = e2.id
             WHERE sl2.recorded_at >= now() - ($1 || ' days')::interval * 2
               AND sl2.recorded_at < now() - ($1 || ' days')::interval
            ) AS prior_requests,
            (SELECT SUM(estimate_cost_usd(sl2.model, sl2.input_tokens, sl2.output_tokens, sl2.cache_read_tokens, sl2.cache_write_tokens))::float8
             FROM spend_log sl2
             LEFT JOIN users u2 ON sl2.user_identity = u2.email
             LEFT JOIN teams t2 ON u2.team_id = t2.id
             LEFT JOIN endpoints e2 ON sl2.endpoint_id = e2.id
             WHERE sl2.recorded_at >= now() - ($1 || ' days')::interval * 2
               AND sl2.recorded_at < now() - ($1 || ' days')::interval
            ) AS prior_cost_usd,
            (SELECT COUNT(DISTINCT sl2.user_identity)::bigint FROM spend_log sl2
             LEFT JOIN users u2 ON sl2.user_identity = u2.email
             LEFT JOIN teams t2 ON u2.team_id = t2.id
             LEFT JOIN endpoints e2 ON sl2.endpoint_id = e2.id
             WHERE sl2.recorded_at >= now() - ($1 || ' days')::interval * 2
               AND sl2.recorded_at < now() - ($1 || ' days')::interval
            ) AS prior_unique_users
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}"#
    );

    let q = sqlx::query_as::<_, OrgOverviewRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    let row = q.fetch_optional(pool).await?;
    Ok(row.unwrap_or(OrgOverviewRow {
        requests: Some(0),
        unique_users: Some(0),
        cost_usd: Some(0.0),
        input_tokens: Some(0),
        output_tokens: Some(0),
        cache_read_tokens: Some(0),
        cache_write_tokens: Some(0),
        avg_duration_ms: None,
        prior_requests: Some(0),
        prior_cost_usd: Some(0.0),
        prior_unique_users: Some(0),
    }))
}

pub async fn org_filter_options(pool: &PgPool) -> anyhow::Result<OrgFilterOptions> {
    let mut teams =
        sqlx::query_scalar::<_, String>("SELECT DISTINCT name FROM teams ORDER BY name")
            .fetch_all(pool)
            .await?;
    // Check if any spend_log users have no team — if so, add a "No Team" option
    let has_unassigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM spend_log sl LEFT JOIN users u ON sl.user_identity = u.email WHERE u.team_id IS NULL LIMIT 1)",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false);
    if has_unassigned {
        teams.push("__none__".to_string());
    }
    let users = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT user_identity FROM spend_log WHERE user_identity IS NOT NULL ORDER BY user_identity",
    )
    .fetch_all(pool)
    .await?;
    let models =
        sqlx::query_scalar::<_, String>("SELECT DISTINCT model FROM spend_log ORDER BY model")
            .fetch_all(pool)
            .await?;
    let endpoints =
        sqlx::query_scalar::<_, String>("SELECT DISTINCT name FROM endpoints ORDER BY name")
            .fetch_all(pool)
            .await?;
    let projects = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT project_key FROM spend_log WHERE project_key IS NOT NULL ORDER BY project_key",
    )
    .fetch_all(pool)
    .await?;

    Ok(OrgFilterOptions {
        teams,
        users,
        models,
        endpoints,
        projects,
    })
}

// ---------------------------------------------------------------------------
// Spend tab
// ---------------------------------------------------------------------------

pub async fn org_spend_timeseries(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<SpendTimePoint>> {
    let gran = resolve_granularity(filter);
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            t.name AS team_name,
            date_trunc('{gran}', sl.recorded_at) AS bucket,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY t.name, bucket
        ORDER BY bucket"#
    );

    let q = sqlx::query_as::<_, SpendTimePoint>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_spend_by_team(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<TeamSpendRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            t.name AS team_name,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd,
            COUNT(*)::bigint AS request_count
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY t.name
        ORDER BY cost_usd DESC NULLS LAST"#
    );

    let q = sqlx::query_as::<_, TeamSpendRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_spend_by_user(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<UserSpendRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.user_identity,
            t.name AS team_name,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd,
            COUNT(*)::bigint AS request_count,
            (SUM(sl.input_tokens) + SUM(sl.output_tokens) + SUM(sl.cache_read_tokens) + SUM(sl.cache_write_tokens))::bigint AS total_tokens,
            MAX(sl.recorded_at) AS last_active
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY sl.user_identity, t.name
        ORDER BY cost_usd DESC NULLS LAST"#
    );

    let q = sqlx::query_as::<_, UserSpendRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_spend_by_model(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<ModelSpendRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.model,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd,
            COUNT(*)::bigint AS request_count
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY sl.model
        ORDER BY cost_usd DESC NULLS LAST"#
    );

    let q = sqlx::query_as::<_, ModelSpendRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_budget_status(pool: &PgPool) -> anyhow::Result<Vec<BudgetStatusRow>> {
    let rows = sqlx::query_as::<_, BudgetStatusRow>(
        r#"SELECT
            t.name AS team_name,
            t.budget_amount_usd AS budget_usd,
            COALESCE(
                SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens)),
                0
            )::float8 AS spent_usd,
            t.budget_period AS period
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
        WHERE t.budget_amount_usd IS NOT NULL
        GROUP BY t.id, t.name, t.budget_amount_usd, t.budget_period
        ORDER BY t.name"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn org_spend_forecast(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<ForecastPoint>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    // Get daily actual spend
    let sql = format!(
        r#"SELECT
            to_char(sl.recorded_at::date, 'YYYY-MM-DD') AS day,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd,
            false AS is_forecast
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY sl.recorded_at::date
        ORDER BY day"#
    );

    let q = sqlx::query_as::<_, ForecastPoint>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    let mut points: Vec<ForecastPoint> = q.fetch_all(pool).await?;

    // Simple linear projection: average daily spend × 7 days forward
    if points.len() >= 2 {
        let costs: Vec<f64> = points.iter().filter_map(|p| p.cost_usd).collect();
        let n = costs.len() as f64;
        let sum_x: f64 = (0..costs.len()).map(|i| i as f64).sum();
        let sum_y: f64 = costs.iter().sum();
        let sum_xy: f64 = costs.iter().enumerate().map(|(i, y)| i as f64 * y).sum();
        let sum_x2: f64 = (0..costs.len()).map(|i| (i as f64).powi(2)).sum();

        let denom = n * sum_x2 - sum_x * sum_x;
        if denom.abs() > f64::EPSILON {
            let slope = (n * sum_xy - sum_x * sum_y) / denom;
            let intercept = (sum_y - slope * sum_x) / n;

            // Parse last date and project forward
            if let Some(last) = points.last()
                && let Ok(last_date) = chrono::NaiveDate::parse_from_str(&last.day, "%Y-%m-%d")
            {
                let forecast_days = 7.min(filter.days);
                for d in 1..=forecast_days {
                    let x = costs.len() as f64 + d as f64 - 1.0;
                    let projected = (intercept + slope * x).max(0.0);
                    let date = last_date + chrono::Duration::days(d as i64);
                    points.push(ForecastPoint {
                        day: date.format("%Y-%m-%d").to_string(),
                        cost_usd: Some(projected),
                        is_forecast: true,
                    });
                }
            }
        }
    }

    Ok(points)
}

// ---------------------------------------------------------------------------
// Activity tab
// ---------------------------------------------------------------------------

pub async fn org_active_users_timeseries(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<ActiveUsersPoint>> {
    let gran = resolve_granularity(filter);
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"WITH user_first_seen AS (
            SELECT user_identity, MIN(recorded_at) AS first_seen
            FROM spend_log
            GROUP BY user_identity
        ),
        bucketed AS (
            SELECT
                date_trunc('{gran}', sl.recorded_at) AS bucket,
                sl.user_identity,
                ufs.first_seen
            FROM spend_log sl
            LEFT JOIN users u ON sl.user_identity = u.email
            LEFT JOIN teams t ON u.team_id = t.id
            LEFT JOIN endpoints e ON sl.endpoint_id = e.id
            LEFT JOIN user_first_seen ufs ON sl.user_identity = ufs.user_identity
            WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
            {wc}
        )
        SELECT
            bucket,
            COUNT(DISTINCT CASE WHEN first_seen >= bucket AND first_seen < bucket + interval '1 {gran}' THEN user_identity END)::bigint AS new_users,
            COUNT(DISTINCT CASE WHEN first_seen < bucket THEN user_identity END)::bigint AS returning_users
        FROM bucketed
        GROUP BY bucket
        ORDER BY bucket"#
    );

    let q = sqlx::query_as::<_, ActiveUsersPoint>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_requests_histogram(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<HistogramBin>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"WITH per_user AS (
            SELECT sl.user_identity, COUNT(*)::bigint AS req_count
            FROM spend_log sl
            LEFT JOIN users u ON sl.user_identity = u.email
            LEFT JOIN teams t ON u.team_id = t.id
            LEFT JOIN endpoints e ON sl.endpoint_id = e.id
            WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
            {wc}
            GROUP BY sl.user_identity
        )
        SELECT
            CASE
                WHEN req_count BETWEEN 1 AND 5 THEN '1-5'
                WHEN req_count BETWEEN 6 AND 10 THEN '6-10'
                WHEN req_count BETWEEN 11 AND 25 THEN '11-25'
                WHEN req_count BETWEEN 26 AND 50 THEN '26-50'
                WHEN req_count BETWEEN 51 AND 100 THEN '51-100'
                WHEN req_count BETWEEN 101 AND 500 THEN '101-500'
                ELSE '500+'
            END AS bucket_label,
            COUNT(*)::bigint AS count
        FROM per_user
        GROUP BY bucket_label
        ORDER BY MIN(req_count)"#
    );

    let q = sqlx::query_as::<_, HistogramBin>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_hourly_heatmap(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<HeatmapCell>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            EXTRACT(HOUR FROM sl.recorded_at)::integer AS hour,
            (EXTRACT(ISODOW FROM sl.recorded_at)::integer - 1) AS day_of_week,
            COUNT(*)::bigint AS request_count
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY hour, day_of_week
        ORDER BY day_of_week, hour"#
    );

    let q = sqlx::query_as::<_, HeatmapCell>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_project_activity(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<ProjectActivityRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.project_key,
            COUNT(*)::bigint AS request_count,
            COUNT(DISTINCT sl.user_identity)::bigint AS unique_users,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd,
            MAX(sl.recorded_at) AS last_active
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
          AND sl.project_key IS NOT NULL
        {wc}
        GROUP BY sl.project_key
        ORDER BY request_count DESC
        LIMIT 50"#
    );

    let q = sqlx::query_as::<_, ProjectActivityRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_sessions_by_user(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<UserSessionRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.user_identity,
            COUNT(DISTINCT sl.session_id)::bigint AS session_count,
            COUNT(*)::bigint AS request_count
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
          AND sl.session_id IS NOT NULL
        {wc}
        GROUP BY sl.user_identity
        ORDER BY session_count DESC
        LIMIT 10"#
    );

    let q = sqlx::query_as::<_, UserSessionRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

// ---------------------------------------------------------------------------
// Models tab
// ---------------------------------------------------------------------------

pub async fn org_model_mix(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<ModelMixRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.model,
            COUNT(*)::bigint AS request_count,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY sl.model
        ORDER BY cost_usd DESC NULLS LAST"#
    );

    let q = sqlx::query_as::<_, ModelMixRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_latency_percentiles(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<LatencyPctRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.model,
            percentile_cont(0.5) WITHIN GROUP (ORDER BY sl.duration_ms)::float8 AS p50,
            percentile_cont(0.95) WITHIN GROUP (ORDER BY sl.duration_ms)::float8 AS p95,
            percentile_cont(0.99) WITHIN GROUP (ORDER BY sl.duration_ms)::float8 AS p99
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
          AND sl.duration_ms IS NOT NULL
        {wc}
        GROUP BY sl.model
        ORDER BY sl.model"#
    );

    let q = sqlx::query_as::<_, LatencyPctRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_cache_rate_timeseries(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<CacheRatePoint>> {
    let gran = resolve_granularity(filter);
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            date_trunc('{gran}', sl.recorded_at) AS bucket,
            CASE
                WHEN SUM(sl.input_tokens + sl.cache_read_tokens) > 0
                THEN (SUM(sl.cache_read_tokens)::float8 / SUM(sl.input_tokens + sl.cache_read_tokens)::float8) * 100.0
                ELSE 0
            END AS cache_hit_rate
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY bucket
        ORDER BY bucket"#
    );

    let q = sqlx::query_as::<_, CacheRatePoint>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_token_breakdown(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<TokenBreakdownRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.model,
            SUM(sl.input_tokens)::bigint AS input_tokens,
            SUM(sl.output_tokens)::bigint AS output_tokens,
            SUM(sl.cache_read_tokens)::bigint AS cache_read_tokens,
            SUM(sl.cache_write_tokens)::bigint AS cache_write_tokens
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY sl.model
        ORDER BY (SUM(sl.input_tokens) + SUM(sl.output_tokens)) DESC"#
    );

    let q = sqlx::query_as::<_, TokenBreakdownRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_stop_reasons(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<StopReasonRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.stop_reason,
            COUNT(*)::bigint AS count
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY sl.stop_reason
        ORDER BY count DESC"#
    );

    let q = sqlx::query_as::<_, StopReasonRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_endpoint_utilization(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<EndpointUtilRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            e.name AS endpoint_name,
            COUNT(*)::bigint AS request_count,
            SUM(estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens))::float8 AS cost_usd,
            AVG(sl.duration_ms)::float8 AS avg_duration_ms,
            CASE WHEN COUNT(*) > 0
                THEN (SUM(CASE WHEN sl.stop_reason = 'error' THEN 1 ELSE 0 END)::float8 / COUNT(*)::float8)
                ELSE 0
            END AS error_rate
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY e.name
        ORDER BY request_count DESC"#
    );

    let q = sqlx::query_as::<_, EndpointUtilRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

// ---------------------------------------------------------------------------
// Tools tab
// ---------------------------------------------------------------------------

pub async fn org_mcp_servers(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<McpServerRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"WITH mcp_tools AS (
            SELECT sl.user_identity, tool_name
            FROM spend_log sl
            LEFT JOIN users u ON sl.user_identity = u.email
            LEFT JOIN teams t ON u.team_id = t.id
            LEFT JOIN endpoints e ON sl.endpoint_id = e.id,
            UNNEST(sl.tool_names) AS tool_name
            WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
              AND tool_name LIKE 'mcp__%'
            {wc}
        )
        SELECT
            split_part(tool_name, '__', 2) AS server_name,
            COUNT(DISTINCT tool_name)::bigint AS unique_tools,
            COUNT(*)::bigint AS usage_count,
            COUNT(DISTINCT user_identity)::bigint AS unique_users
        FROM mcp_tools
        GROUP BY server_name
        ORDER BY usage_count DESC"#
    );

    let q = sqlx::query_as::<_, McpServerRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_top_tools(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<TopToolRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            tool_name,
            COUNT(*)::bigint AS usage_count,
            COUNT(DISTINCT sl.user_identity)::bigint AS unique_users
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id,
        UNNEST(sl.tool_names) AS tool_name
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY tool_name
        ORDER BY usage_count DESC
        LIMIT 30"#
    );

    let q = sqlx::query_as::<_, TopToolRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_mcp_adoption(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<McpAdoptionPoint>> {
    let gran = resolve_granularity(filter);
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            date_trunc('{gran}', sl.recorded_at) AS bucket,
            COUNT(DISTINCT CASE WHEN EXISTS (
                SELECT 1 FROM UNNEST(sl.tool_names) tn WHERE tn LIKE 'mcp__%'
            ) THEN sl.user_identity END)::bigint AS mcp_users,
            COUNT(DISTINCT sl.user_identity)::bigint AS total_users
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        GROUP BY bucket
        ORDER BY bucket"#
    );

    let q = sqlx::query_as::<_, McpAdoptionPoint>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

pub async fn org_tool_totals(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<ToolTotalsRow> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"WITH base AS (
            SELECT sl.tool_count, sl.tool_names
            FROM spend_log sl
            LEFT JOIN users u ON sl.user_identity = u.email
            LEFT JOIN teams t ON u.team_id = t.id
            LEFT JOIN endpoints e ON sl.endpoint_id = e.id
            WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
            {wc}
        )
        SELECT
            (SELECT SUM(tool_count)::bigint FROM base) AS total_tool_calls,
            COUNT(CASE WHEN tool_name = 'web_search' THEN 1 END)::bigint AS web_search_count,
            COUNT(CASE WHEN tool_name LIKE 'mcp__%' THEN 1 END)::bigint AS mcp_tool_count,
            COUNT(DISTINCT CASE WHEN tool_name LIKE 'mcp__%' THEN split_part(tool_name, '__', 2) END)::bigint AS unique_mcp_servers
        FROM base, LATERAL UNNEST(base.tool_names) AS tool_name"#
    );

    let q = sqlx::query_as::<_, ToolTotalsRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    let row = q.fetch_optional(pool).await?;
    Ok(row.unwrap_or(ToolTotalsRow {
        total_tool_calls: Some(0),
        web_search_count: Some(0),
        mcp_tool_count: Some(0),
        unique_mcp_servers: Some(0),
    }))
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

pub async fn org_export(
    pool: &PgPool,
    filter: &OrgAnalyticsFilter,
) -> anyhow::Result<Vec<OrgExportRow>> {
    let (fb, binds) = build_optional_filters(filter);
    let wc = fb.where_clause();

    let sql = format!(
        r#"SELECT
            sl.recorded_at,
            sl.user_identity,
            t.name AS team_name,
            sl.model,
            sl.input_tokens,
            sl.output_tokens,
            sl.cache_read_tokens,
            sl.cache_write_tokens,
            estimate_cost_usd(sl.model, sl.input_tokens, sl.output_tokens, sl.cache_read_tokens, sl.cache_write_tokens)::float8 AS cost_usd,
            sl.duration_ms,
            sl.tool_count,
            e.name AS endpoint_name
        FROM spend_log sl
        LEFT JOIN users u ON sl.user_identity = u.email
        LEFT JOIN teams t ON u.team_id = t.id
        LEFT JOIN endpoints e ON sl.endpoint_id = e.id
        WHERE sl.recorded_at >= now() - ($1 || ' days')::interval
        {wc}
        ORDER BY sl.recorded_at DESC"#
    );

    let q = sqlx::query_as::<_, OrgExportRow>(&sql).bind(filter.days.to_string());
    let q = bind_filters!(q, binds);
    Ok(q.fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_filter(days: i32, gran: &str) -> OrgAnalyticsFilter {
        OrgAnalyticsFilter {
            days,
            granularity: gran.to_string(),
            team: None,
            user: None,
            model: None,
            endpoint: None,
            from: None,
            to: None,
        }
    }

    #[test]
    fn test_resolve_granularity_auto() {
        assert_eq!(resolve_granularity(&make_filter(1, "auto")), "hour");
        assert_eq!(resolve_granularity(&make_filter(7, "auto")), "hour");
        assert_eq!(resolve_granularity(&make_filter(30, "auto")), "day");
        assert_eq!(resolve_granularity(&make_filter(90, "auto")), "day");
        assert_eq!(resolve_granularity(&make_filter(91, "auto")), "week");
        assert_eq!(resolve_granularity(&make_filter(365, "auto")), "week");
    }

    #[test]
    fn test_resolve_granularity_manual_override() {
        assert_eq!(resolve_granularity(&make_filter(30, "hour")), "hour");
        assert_eq!(resolve_granularity(&make_filter(1, "day")), "day");
    }

    #[test]
    fn test_filter_builder_empty() {
        let (fb, vals) = build_optional_filters(&make_filter(7, "auto"));
        assert!(fb.where_clause().is_empty());
        assert!(vals.is_empty());
    }

    #[test]
    fn test_filter_builder_with_values() {
        let filter = OrgAnalyticsFilter {
            days: 7,
            granularity: "auto".to_string(),
            team: Some("engineering".to_string()),
            user: None,
            model: Some("claude-sonnet-4-5-20250514".to_string()),
            endpoint: None,
            from: None,
            to: None,
        };
        let (fb, vals) = build_optional_filters(&filter);
        assert_eq!(vals.len(), 2);
        assert_eq!(vals[0], "engineering");
        assert_eq!(vals[1], "claude-sonnet-4-5-20250514");
        let clause = fb.where_clause();
        assert!(clause.contains("$2"));
        assert!(clause.contains("$3"));
        assert!(clause.contains("t.name"));
        assert!(clause.contains("sl.model"));
    }

    #[test]
    fn test_filter_builder_all_fields() {
        let filter = OrgAnalyticsFilter {
            days: 7,
            granularity: "auto".to_string(),
            team: Some("team1".to_string()),
            user: Some("user@example.com".to_string()),
            model: Some("model1".to_string()),
            endpoint: Some("ep1".to_string()),
            from: None,
            to: None,
        };
        let (fb, vals) = build_optional_filters(&filter);
        assert_eq!(vals.len(), 4);
        let clause = fb.where_clause();
        assert!(clause.contains("$2"));
        assert!(clause.contains("$3"));
        assert!(clause.contains("$4"));
        assert!(clause.contains("$5"));
    }
}
