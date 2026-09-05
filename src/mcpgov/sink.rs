//! Buffered background writers for catalog discovery and audit events.
//!
//! Both are strictly fire-and-forget. Nothing here may block, slow, or fail a
//! `/v1/messages` request: the channels are bounded and overflow is dropped with a
//! counter rather than applying backpressure to the request path. Losing a few audit
//! rows under extreme load is an acceptable trade for never adding latency.

use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::PgPool;
use tokio::sync::mpsc;

use super::{DeniedTool, Mode, RequestContext, cache, policy, store};

/// Bounded so a pathological workload cannot grow memory without limit.
const CHANNEL_CAPACITY: usize = 1024;
/// Maximum rows coalesced into a single INSERT.
const BATCH_SIZE: usize = 256;
/// How often the audit log is trimmed.
const PRUNE_INTERVAL_SECS: u64 = 3600;
/// Default audit retention, overridable with the `mcpgov_event_retention_days` setting.
const DEFAULT_RETENTION_DAYS: i64 = 30;

type EventItem = (DeniedTool, String, Mode, RequestContext);

struct Channels {
    tools_tx: mpsc::Sender<Vec<String>>,
    tools_rx: Mutex<Option<mpsc::Receiver<Vec<String>>>>,
    events_tx: mpsc::Sender<EventItem>,
    events_rx: Mutex<Option<mpsc::Receiver<EventItem>>>,
}

static CHANNELS: LazyLock<Channels> = LazyLock::new(|| {
    let (tools_tx, tools_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (events_tx, events_rx) = mpsc::channel(CHANNEL_CAPACITY);
    Channels {
        tools_tx,
        tools_rx: Mutex::new(Some(tools_rx)),
        events_tx,
        events_rx: Mutex::new(Some(events_rx)),
    }
});

static DROPPED_TOOLS: AtomicU64 = AtomicU64::new(0);
static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Number of discovery / audit items dropped because a buffer was full. Surfaced in
/// the admin summary so a silent loss is at least visible.
pub fn dropped_counts() -> (u64, u64) {
    (
        DROPPED_TOOLS.load(Ordering::Relaxed),
        DROPPED_EVENTS.load(Ordering::Relaxed),
    )
}

/// Spawn the drain and maintenance tasks. Idempotent: the receivers can only be taken
/// once, so a second call is a no-op.
pub fn start(pool: PgPool) {
    if let Some(rx) = CHANNELS.tools_rx.lock().ok().and_then(|mut g| g.take()) {
        spawn_tools_drain(pool.clone(), rx);
    }
    if let Some(rx) = CHANNELS.events_rx.lock().ok().and_then(|mut g| g.take()) {
        spawn_events_drain(pool.clone(), rx);
    }
    spawn_prune(pool);
}

/// Queue any tool names the catalog has not seen before.
///
/// The cache filters known names first, so in steady state this is a single read lock
/// and an early return — no channel traffic, no database work.
pub async fn observe_tools(names: &[String]) {
    let fresh = cache::take_unknown(names).await;
    if fresh.is_empty() {
        return;
    }
    if CHANNELS.tools_tx.try_send(fresh).is_err() {
        DROPPED_TOOLS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Queue audit rows for tools that policy denied.
pub fn record_events(denied: &[DeniedTool], decision: &str, mode: Mode, ctx: &RequestContext) {
    for d in denied {
        let item = (d.clone(), decision.to_string(), mode, ctx.clone());
        if CHANNELS.events_tx.try_send(item).is_err() {
            DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn spawn_tools_drain(pool: PgPool, mut rx: mpsc::Receiver<Vec<String>>) {
    tokio::spawn(async move {
        let mut pending: Vec<Vec<String>> = Vec::new();
        while let Some(first) = rx.recv().await {
            pending.push(first);
            // Coalesce whatever else is already queued.
            while pending.len() < BATCH_SIZE {
                match rx.try_recv() {
                    Ok(more) => pending.push(more),
                    Err(_) => break,
                }
            }

            let mut names: Vec<String> = Vec::new();
            for group in pending.drain(..) {
                for n in group {
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
            }

            let servers: Vec<Option<String>> = names
                .iter()
                .map(|n| policy::parse_tool_name(n).server.map(String::from))
                .collect();

            if let Err(e) = store::upsert_observed(&pool, &names, &servers).await {
                tracing::warn!(%e, count = names.len(), "mcpgov: catalog upsert failed");
            } else {
                tracing::debug!(count = names.len(), "mcpgov: discovered new tools");
            }
        }
    });
}

fn spawn_events_drain(pool: PgPool, mut rx: mpsc::Receiver<EventItem>) {
    tokio::spawn(async move {
        let mut batch: Vec<EventItem> = Vec::new();
        while let Some(first) = rx.recv().await {
            batch.push(first);
            while batch.len() < BATCH_SIZE {
                match rx.try_recv() {
                    Ok(more) => batch.push(more),
                    Err(_) => break,
                }
            }

            if let Err(e) = store::insert_events(&pool, &batch).await {
                tracing::warn!(%e, count = batch.len(), "mcpgov: audit insert failed");
            }
            batch.clear();
        }
    });
}

fn spawn_prune(pool: PgPool) {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(PRUNE_INTERVAL_SECS));
        // The first tick fires immediately; skip it so boot does not do maintenance.
        interval.tick().await;

        loop {
            interval.tick().await;

            let days = crate::db::settings::get_setting(&pool, "mcpgov_event_retention_days")
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|d| *d > 0)
                .unwrap_or(DEFAULT_RETENTION_DAYS);

            match store::prune_events(&pool, days).await {
                Ok(n) if n > 0 => {
                    tracing::info!(pruned = n, keep_days = days, "mcpgov: trimmed audit log")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(%e, "mcpgov: audit prune failed"),
            }
        }
    });
}
