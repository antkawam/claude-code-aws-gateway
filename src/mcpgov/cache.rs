//! In-memory policy and catalog cache.
//!
//! Deliberately a module-level singleton rather than a field on `GatewayState`. That
//! keeps the overlay self-contained: adding it requires no change to the shared state
//! struct or to the state construction in `main.rs`, which are exactly the kind of
//! densely-edited upstream lines that make rebases painful.
//!
//! Propagation across replicas reuses CCAG's existing `cache_version` counter. Any
//! operator write bumps it, and every gateway notices within 5 seconds.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};

use sqlx::PgPool;
use tokio::sync::RwLock;

use super::{CatalogStatus, EffectivePolicy, PolicyRow, RequestContext, policy, store};

/// Poll interval, matched to CCAG's own cache poll loop.
const POLL_INTERVAL_SECS: u64 = 5;

#[derive(Default)]
struct Inner {
    /// The single `global` policy row, if present.
    global: Option<PolicyRow>,
    /// Team policies keyed by team UUID rendered as a string.
    by_team: HashMap<String, PolicyRow>,
    /// User policies keyed by user identity (email).
    by_user: HashMap<String, PolicyRow>,
    /// Resolved catalog status per full tool name.
    tools: HashMap<String, CatalogStatus>,
    /// Resolved catalog status per MCP server name.
    servers: HashMap<String, CatalogStatus>,
    /// Tool names already present in the catalog, used to avoid re-queueing every
    /// known tool on every single request.
    known_tools: HashSet<String>,
}

struct Cache {
    inner: RwLock<Inner>,
    version: AtomicI64,
}

static CACHE: LazyLock<Cache> = LazyLock::new(|| Cache {
    inner: RwLock::new(Inner::default()),
    version: AtomicI64::new(-1),
});

/// Load the cache from the database. Errors are logged, not propagated: a governance
/// overlay that cannot read its own tables must leave the gateway running (and, since
/// an empty cache resolves to the inert default policy, running unmodified).
pub async fn init(pool: &PgPool) {
    if let Err(e) = reload(pool).await {
        tracing::warn!(%e, "mcpgov: initial cache load failed — overlay inert until next poll");
    }
}

pub async fn reload(pool: &PgPool) -> anyhow::Result<()> {
    let policies = store::load_policies(pool).await?;
    let servers = store::load_servers(pool).await?;
    let tools = store::load_tools(pool).await?;

    let mut next = Inner::default();

    for row in &policies {
        let p = row.to_policy();
        match row.scope.as_str() {
            "global" => next.global = Some(p),
            "team" => {
                if let Some(r) = &row.scope_ref {
                    next.by_team.insert(r.clone(), p);
                }
            }
            "user" => {
                if let Some(r) = &row.scope_ref {
                    // User identities are emails; compare case-insensitively.
                    next.by_user.insert(r.to_lowercase(), p);
                }
            }
            other => tracing::warn!(scope = other, "mcpgov: ignoring policy with unknown scope"),
        }
    }

    for s in &servers {
        next.servers.insert(
            s.server_name.clone(),
            CatalogStatus::parse_server(&s.status),
        );
    }
    for t in &tools {
        next.tools
            .insert(t.tool_name.clone(), CatalogStatus::parse_tool(&t.status));
        next.known_tools.insert(t.tool_name.clone());
    }

    let counts = (policies.len(), next.servers.len(), next.tools.len());
    *CACHE.inner.write().await = next;
    tracing::debug!(
        policies = counts.0,
        servers = counts.1,
        tools = counts.2,
        "mcpgov: cache reloaded"
    );
    Ok(())
}

/// Poll `cache_version` and reload when an operator write bumps it.
///
/// This runs as its own task rather than hooking CCAG's `start_cache_poll_loop`, so
/// the overlay adds no line inside that function's body.
pub fn start_poll_loop(pool: PgPool) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(POLL_INTERVAL_SECS));
        loop {
            interval.tick().await;

            let observed = match crate::db::settings::get_cache_version(&pool).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(%e, "mcpgov: cache version poll failed");
                    continue;
                }
            };

            let current = CACHE.version.load(Ordering::Relaxed);
            if observed != current {
                if let Err(e) = reload(&pool).await {
                    tracing::warn!(%e, "mcpgov: cache reload failed");
                    continue;
                }
                CACHE.version.store(observed, Ordering::Relaxed);
            }
        }
    });
}

/// Force an immediate reload, used by admin handlers so an operator sees their own
/// change take effect without waiting out the poll interval on this replica.
pub async fn invalidate_now(pool: &PgPool) {
    if let Err(e) = reload(pool).await {
        tracing::warn!(%e, "mcpgov: immediate reload after admin write failed");
    }
}

/// Merge the policies that apply to this identity.
pub async fn effective_policy(ctx: &RequestContext) -> EffectivePolicy {
    let inner = CACHE.inner.read().await;

    let team = ctx
        .team_id
        .and_then(|id| inner.by_team.get(&id.to_string()));
    let user = ctx
        .user_identity
        .as_ref()
        .and_then(|u| inner.by_user.get(&u.to_lowercase()));

    policy::resolve(inner.global.as_ref(), team, user)
}

/// Snapshot the catalog maps for a decision pass.
pub async fn catalog_snapshot() -> (
    HashMap<String, CatalogStatus>,
    HashMap<String, CatalogStatus>,
) {
    let inner = CACHE.inner.read().await;
    (inner.tools.clone(), inner.servers.clone())
}

/// Partition tool names into those the catalog has never seen (which need an upsert)
/// and mark them known so repeat requests do not re-queue them.
///
/// Returning early for the common "everything already known" case keeps discovery off
/// the hot path: after warm-up this is a single read lock and no allocation.
pub async fn take_unknown(names: &[String]) -> Vec<String> {
    {
        let inner = CACHE.inner.read().await;
        if names.iter().all(|n| inner.known_tools.contains(n)) {
            return Vec::new();
        }
    }

    let mut inner = CACHE.inner.write().await;
    let mut fresh = Vec::new();
    for n in names {
        if inner.known_tools.insert(n.clone()) {
            fresh.push(n.clone());
        }
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcpgov::{Action, AppliesTo, Mode};

    fn row(scope: &str, scope_ref: Option<&str>, mode: Mode) -> PolicyRow {
        PolicyRow {
            scope: scope.to_string(),
            scope_ref: scope_ref.map(String::from),
            mode,
            default_action: Action::Allow,
            applies_to: AppliesTo::McpOnly,
            allow_patterns: vec![],
            deny_patterns: vec![],
            enabled: true,
        }
    }

    /// Serialises the tests in this module.
    ///
    /// They all replace the same process-wide cache, and cargo runs tests in parallel by
    /// default, so without this each test can observe another's seed. That showed up as
    /// intermittent failures rather than consistent ones, which is the worst kind.
    /// Every test holds the guard for its whole body, not just across [`seed`].
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn seed(inner: Inner) {
        *CACHE.inner.write().await = inner;
    }

    #[tokio::test]
    async fn user_policy_beats_team_and_global() {
        let _guard = TEST_LOCK.lock().await;
        let mut inner = Inner {
            global: Some(row("global", None, Mode::Observe)),
            ..Default::default()
        };
        let team_id = uuid::Uuid::nil();
        inner
            .by_team
            .insert(team_id.to_string(), row("team", Some("t"), Mode::Warn));
        inner.by_user.insert(
            "dev@example.com".into(),
            row("user", Some("u"), Mode::Enforce),
        );
        seed(inner).await;

        let ctx = RequestContext {
            user_identity: Some("dev@example.com".into()),
            team_id: Some(team_id),
            ..Default::default()
        };
        assert_eq!(effective_policy(&ctx).await.mode, Mode::Enforce);

        // Same team, a user with no policy of their own -> team policy applies.
        let ctx2 = RequestContext {
            user_identity: Some("other@example.com".into()),
            team_id: Some(team_id),
            ..Default::default()
        };
        assert_eq!(effective_policy(&ctx2).await.mode, Mode::Warn);
    }

    #[tokio::test]
    async fn user_identity_matches_case_insensitively() {
        let _guard = TEST_LOCK.lock().await;
        let mut inner = Inner::default();
        inner.by_user.insert(
            "dev@example.com".into(),
            row("user", Some("u"), Mode::Enforce),
        );
        seed(inner).await;

        let ctx = RequestContext {
            user_identity: Some("DEV@Example.COM".into()),
            ..Default::default()
        };
        assert_eq!(effective_policy(&ctx).await.mode, Mode::Enforce);
    }

    #[tokio::test]
    async fn take_unknown_returns_each_name_once() {
        let _guard = TEST_LOCK.lock().await;
        seed(Inner::default()).await;

        let names = vec!["mcp__a__x".to_string(), "mcp__b__y".to_string()];
        let first = take_unknown(&names).await;
        assert_eq!(first.len(), 2);

        // Second call sees them as known.
        let second = take_unknown(&names).await;
        assert!(second.is_empty());

        // A new name still comes through.
        let third = take_unknown(&["mcp__c__z".to_string()]).await;
        assert_eq!(third, vec!["mcp__c__z".to_string()]);
    }
}
