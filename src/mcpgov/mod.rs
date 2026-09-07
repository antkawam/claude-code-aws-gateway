//! MCP / tool governance.
//!
//! This module is an **additive extension** to CCAG: all of its logic lives under
//! `src/mcpgov/`, its tables are prefixed `mcpgov_`, its migration is numbered 900,
//! and its portal UI is a separate self-injecting asset. Contact with pre-existing
//! files is deliberately minimal — one hook in `src/api/handlers.rs`, one `.merge()`
//! in `src/api/mod.rs`, a module declaration, two background tasks in `main.rs`, and a
//! single `<script>` tag in `static/index.html`.
//!
//! What it does
//! ------------
//! Claude Code's own MCP controls (`managed-mcp.json`, `allowedMcpServers`) are
//! client-side, and Anthropic's server-managed settings are skipped entirely for any
//! session with a custom `ANTHROPIC_BASE_URL` — which is exactly how CCAG is used. So
//! policy is enforced here instead, on the request path, where it needs no client
//! configuration and cannot be bypassed by editing local files.
//!
//! Two halves:
//!
//! * **Discovery** — every request carries the full `tools` array, which is a complete
//!   inventory of what the caller has connected. Observed tools are upserted into a
//!   catalog so operators approve from a real list instead of guessing.
//! * **Enforcement** — denied tool *definitions* are stripped before the request
//!   reaches Bedrock. Filtering definitions rather than invocations means the model
//!   never sees a forbidden tool, so there is nothing to intercept later and the
//!   behaviour is identical in streaming and non-streaming mode.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub mod admin;
pub mod cache;
pub mod policy;
pub mod response;
pub mod sink;
pub mod store;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// How aggressively policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Decide and record, but never modify the request. The dry-run posture used to
    /// build up the catalog before turning enforcement on.
    Observe,
    /// Allow the tool through, but record the decision as a `warned` audit event and
    /// log it. The request itself is left untouched.
    Warn,
    /// Actually strip denied tool definitions.
    Enforce,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Observe => "observe",
            Mode::Warn => "warn",
            Mode::Enforce => "enforce",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "enforce" => Mode::Enforce,
            "warn" => Mode::Warn,
            _ => Mode::Observe,
        }
    }
}

/// What to do with a tool the catalog has no decision for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Deny,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "deny" => Action::Deny,
            _ => Action::Allow,
        }
    }
}

/// Which tools a policy governs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliesTo {
    /// Only `mcp__*` tools. The safe default: builtins like Read/Write/Bash are
    /// never touched, so a misconfigured policy cannot break basic Claude Code use.
    McpOnly,
    /// Builtins are governed too.
    AllTools,
}

impl AppliesTo {
    pub fn as_str(self) -> &'static str {
        match self {
            AppliesTo::McpOnly => "mcp_only",
            AppliesTo::AllTools => "all_tools",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "all_tools" => AppliesTo::AllTools,
            _ => AppliesTo::McpOnly,
        }
    }
}

/// Approval state of a catalog entry. For tools, `Pending` also represents the
/// `inherit` state stored in the database (defer to the server's status).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    Pending,
    Approved,
    Denied,
}

impl CatalogStatus {
    pub fn parse_server(s: &str) -> Self {
        match s {
            "approved" => CatalogStatus::Approved,
            "denied" => CatalogStatus::Denied,
            _ => CatalogStatus::Pending,
        }
    }

    /// Tool rows use `inherit` where servers use `pending`; both mean "undecided
    /// here, look further up".
    pub fn parse_tool(s: &str) -> Self {
        match s {
            "approved" => CatalogStatus::Approved,
            "denied" => CatalogStatus::Denied,
            _ => CatalogStatus::Pending,
        }
    }
}

/// Servers use the same three states as tools.
pub type ServerStatus = CatalogStatus;

/// Which layer produced a decision, recorded for auditing and shown in the portal's
/// simulator so operators can see *why* a tool was allowed or denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecidedBy {
    Global,
    Team,
    User,
    /// An explicit approve/deny in the catalog.
    Catalog,
    /// No specific attribution: out of policy scope, or a fallback default.
    Unscoped,
}

impl DecidedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            DecidedBy::Global => "global",
            DecidedBy::Team => "team",
            DecidedBy::User => "user",
            DecidedBy::Catalog => "catalog",
            DecidedBy::Unscoped => "unscoped",
        }
    }
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// A tool name split into its MCP server and tool segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRef<'a> {
    /// The full name as it appeared in the request.
    pub full: &'a str,
    /// The MCP server segment, or `None` for builtins and malformed MCP names.
    pub server: Option<&'a str>,
    /// The tool segment (equals `full` for builtins).
    pub tool: &'a str,
    pub is_mcp: bool,
}

/// One row of `mcpgov_policies`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRow {
    pub scope: String,
    pub scope_ref: Option<String>,
    pub mode: Mode,
    pub default_action: Action,
    pub applies_to: AppliesTo,
    pub allow_patterns: Vec<String>,
    pub deny_patterns: Vec<String>,
    pub enabled: bool,
}

impl PolicyRow {
    pub fn scope_kind(&self) -> DecidedBy {
        match self.scope.as_str() {
            "team" => DecidedBy::Team,
            "user" => DecidedBy::User,
            "global" => DecidedBy::Global,
            _ => DecidedBy::Unscoped,
        }
    }
}

/// The merged policy that applies to one identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectivePolicy {
    pub mode: Mode,
    pub default_action: Action,
    pub applies_to: AppliesTo,
    pub allow_patterns: Vec<String>,
    pub deny_patterns: Vec<String>,
    /// The most specific scope that contributed scalars.
    pub decided_by: DecidedBy,
}

impl Default for EffectivePolicy {
    /// With no policy rows at all the overlay is inert: observe mode, allow
    /// everything, MCP-only scope. Installing the extension therefore changes no
    /// behaviour until an operator configures something.
    fn default() -> Self {
        Self {
            mode: Mode::Observe,
            default_action: Action::Allow,
            applies_to: AppliesTo::McpOnly,
            allow_patterns: Vec::new(),
            deny_patterns: Vec::new(),
            decided_by: DecidedBy::Unscoped,
        }
    }
}

/// The verdict for a single tool, before `mode` is taken into account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub allowed: bool,
    pub reason: String,
    pub decided_by: DecidedBy,
}

impl Decision {
    pub fn allow(reason: &str, decided_by: DecidedBy) -> Self {
        Self {
            allowed: true,
            reason: reason.to_string(),
            decided_by,
        }
    }

    pub fn allow_owned(reason: String, decided_by: DecidedBy) -> Self {
        Self {
            allowed: true,
            reason,
            decided_by,
        }
    }

    pub fn deny(reason: String, decided_by: DecidedBy) -> Self {
        Self {
            allowed: false,
            reason,
            decided_by,
        }
    }
}

/// Identity and request metadata needed to resolve policy and write audit rows.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    /// OIDC email or virtual-key user identity.
    pub user_identity: Option<String>,
    pub team_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub request_id: Option<String>,
}

/// How many history blocks were scrubbed, and what that implies for the caller.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct HistoryScrub {
    /// `tool_use` blocks removed from assistant turns.
    pub tool_uses: usize,
    /// `tool_result` blocks removed from user turns to keep pairing valid.
    pub tool_results: usize,
    /// Messages left with no content, which had to get a placeholder block.
    pub placeholders: usize,
}

impl HistoryScrub {
    pub fn is_empty(&self) -> bool {
        self.tool_uses == 0 && self.tool_results == 0
    }
}

/// A tool that policy did not permit.
#[derive(Debug, Clone, Serialize)]
pub struct DeniedTool {
    pub tool_name: String,
    pub server_name: Option<String>,
    pub reason: String,
    pub decided_by: String,
}

/// What enforcement did to a request.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EnforcementOutcome {
    /// Tools whose definitions were actually removed (enforce mode only).
    pub removed: Vec<DeniedTool>,
    /// Tools that policy denied but mode let through (observe / warn).
    pub flagged: Vec<DeniedTool>,
    /// True when `tool_choice` had to be relaxed because it named a removed tool.
    pub tool_choice_reset: bool,
    /// Denied tool traffic erased from the conversation transcript.
    pub history: HistoryScrub,
}

impl EnforcementOutcome {
    pub fn is_noop(&self) -> bool {
        self.removed.is_empty()
            && self.flagged.is_empty()
            && !self.tool_choice_reset
            && self.history.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Initialise the overlay: load the cache and start the background poll and
/// buffered-writer tasks. Safe to call once at startup; failures are logged and
/// leave the overlay inert rather than blocking gateway boot.
pub async fn start(pool: sqlx::PgPool) {
    cache::init(&pool).await;
    cache::start_poll_loop(pool.clone());
    sink::start(pool);
}

/// Extract the tool names declared in an Anthropic `tools` array.
///
/// Tool definitions carry their name in `name`; Anthropic server-side tools (such as
/// `web_search`) instead carry only a `type`, which we skip — those are handled by
/// CCAG's own web-search interception, not by MCP policy.
fn tool_names_of(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

/// Extract the distinct tool names the transcript shows the client has actually called.
///
/// Only assistant `tool_use` blocks count. `tool_result` blocks carry an id rather than
/// a name, so they are matched back to their call during the scrub instead.
///
/// Order is preserved and duplicates collapsed, so a long session with many repeats of
/// the same tool costs one policy decision, not one per turn.
fn tool_names_in_history(messages: &[Value]) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut names: Vec<String> = Vec::new();

    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        // Content is a plain string on turns with no tool calls; nothing to collect.
        let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            if let Some(name) = block.get("name").and_then(|n| n.as_str())
                && seen.insert(name)
            {
                names.push(name.to_string());
            }
        }
    }

    names
}

/// Apply MCP/tool policy to an outbound request.
///
/// Mutates `tools` (removing denied definitions) and `tool_choice` (relaxing it if it
/// named a removed tool) **only** in `enforce` mode. In `observe` and `warn` mode the
/// request is left byte-for-byte untouched and the decisions are only recorded.
///
/// This never fails the request: on any internal error it logs and allows, because a
/// governance overlay must not be able to take the gateway down.
pub async fn enforce(
    tools: &mut Option<Vec<Value>>,
    tool_choice: &mut Option<Value>,
    messages: &mut [Value],
    ctx: &RequestContext,
) -> EnforcementOutcome {
    let noop = EnforcementOutcome::default();

    // What the client declared this turn.
    let declared = tools.as_deref().map(tool_names_of).unwrap_or_default();

    // What the transcript proves the client can still reach. These two sets are not the
    // same, and the difference is the whole reason enforcement leaked: once the gateway
    // strips a definition, the client stops re-declaring the tool, but its history still
    // carries the earlier `tool_use` / `tool_result` pair — and the model keeps calling
    // the tool from that memory. Judging only the declarations means a continuing session
    // is never re-examined, so it keeps working forever. Both sets must be policed.
    let historical = tool_names_in_history(messages);

    if declared.is_empty() && historical.is_empty() {
        return noop;
    }

    // Only declared names feed the catalog. A name in the transcript is evidence of a
    // past call, not of a currently connected server, so recording it would resurrect
    // catalog entries for tools a developer has since removed.
    if !declared.is_empty() {
        sink::observe_tools(&declared).await;
    }

    let eff = cache::effective_policy(ctx).await;
    let (catalog_tools, catalog_servers) = cache::catalog_snapshot().await;

    let mut denied: Vec<DeniedTool> = Vec::new();
    let mut judged: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for name in declared.iter().chain(historical.iter()) {
        if !judged.insert(name.as_str()) {
            continue; // Declared and used in the same request: decide once.
        }
        let d = policy::decide(name, &eff, &catalog_tools, &catalog_servers);
        if !d.allowed {
            let parsed = policy::parse_tool_name(name);
            denied.push(DeniedTool {
                tool_name: name.clone(),
                server_name: parsed.server.map(String::from),
                reason: d.reason,
                decided_by: d.decided_by.as_str().to_string(),
            });
        }
    }

    if denied.is_empty() {
        return noop;
    }

    let decision_label = match eff.mode {
        Mode::Enforce => "blocked",
        Mode::Warn => "warned",
        Mode::Observe => "would_block",
    };
    sink::record_events(&denied, decision_label, eff.mode, ctx);

    apply(tools, tool_choice, messages, denied, eff.mode)
}

/// Erase every trace of the denied tools from the conversation transcript.
///
/// Stripping tool *definitions* alone is not enough. Once a session's history contains
/// a successful `tool_use` / `tool_result` pair, the model will keep emitting `tool_use`
/// blocks for that name from memory, even though the tool is no longer declared — and
/// the Anthropic API accepts those blocks rather than rejecting them. The client then
/// executes the call locally, because its MCP server is still connected. Verified
/// against a live gateway: definition-stripping protects fresh sessions but not
/// sessions that already used the tool.
///
/// So the transcript has to be scrubbed too. Doing it request-side keeps enforcement
/// identical for streaming and non-streaming, and avoids having to reassemble
/// `partial_json` deltas to intercept tool calls on the way back.
///
/// The API requires every `tool_use` to be answered by a `tool_result` with a matching
/// id, so both halves are removed together. A message emptied by that removal gets a
/// placeholder text block, since empty content is rejected.
fn scrub_history(messages: &mut [Value], denied: &std::collections::HashSet<&str>) -> HistoryScrub {
    let mut stats = HistoryScrub::default();
    let mut orphaned_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Pass 1 — drop the assistant's calls to denied tools, remembering their ids.
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        content.retain(|block| {
            let is_denied_tool_use = block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| denied.contains(n));
            if is_denied_tool_use {
                if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                    orphaned_ids.insert(id.to_string());
                }
                return false;
            }
            true
        });
    }
    stats.tool_uses = orphaned_ids.len();

    if orphaned_ids.is_empty() {
        return stats;
    }

    // Pass 2 — drop the results that answered them, or the request becomes invalid.
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let before = content.len();
        content.retain(|block| {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                return block
                    .get("tool_use_id")
                    .and_then(|i| i.as_str())
                    .is_none_or(|id| !orphaned_ids.contains(id));
            }
            true
        });
        stats.tool_results += before - content.len();
    }

    // Pass 3 — an empty content array is rejected by the API, so backfill.
    for msg in messages.iter_mut() {
        if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut())
            && content.is_empty()
        {
            content.push(serde_json::json!({ "type": "text", "text": "(omitted)" }));
            stats.placeholders += 1;
        }
    }

    stats
}

/// Apply a set of denials to the request, honouring `mode`.
///
/// Split out from [`enforce`] as a pure function so the mutation semantics — which are
/// the part that can actually break a request — are unit testable without a database
/// or the policy cache.
pub(crate) fn apply(
    tools: &mut Option<Vec<Value>>,
    tool_choice: &mut Option<Value>,
    messages: &mut [Value],
    denied: Vec<DeniedTool>,
    mode: Mode,
) -> EnforcementOutcome {
    let mut outcome = EnforcementOutcome::default();

    match mode {
        // Dry run and warn-only: record, but do not touch the request.
        Mode::Observe | Mode::Warn => {
            outcome.flagged = denied;
            return outcome;
        }
        Mode::Enforce => {}
    }

    let removed_names: std::collections::HashSet<&str> =
        denied.iter().map(|d| d.tool_name.as_str()).collect();

    // Remove the tool from the transcript as well as from the declarations, so the
    // model cannot re-invoke it from memory of an earlier turn.
    outcome.history = scrub_history(messages, &removed_names);

    // Strip the denied definitions.
    let kept: Vec<Value> = tools
        .as_ref()
        .map(|list| {
            list.iter()
                .filter(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .is_none_or(|n| !removed_names.contains(n))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    // An empty `tools` array is not the same as an absent one — Bedrock rejects the
    // former on some model builds, so drop the field entirely instead.
    *tools = if kept.is_empty() { None } else { Some(kept) };

    // A `tool_choice` that names a stripped tool would make Bedrock 400. Relax it to
    // `auto`, or drop it if there are no tools left at all.
    if let Some(tc) = tool_choice.as_ref() {
        let names_a_removed_tool = tc
            .get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| removed_names.contains(n));
        if names_a_removed_tool {
            *tool_choice = if tools.is_some() {
                Some(serde_json::json!({ "type": "auto" }))
            } else {
                None
            };
            outcome.tool_choice_reset = true;
        } else if tools.is_none() {
            // No tools survive, so any tool_choice is meaningless.
            *tool_choice = None;
            outcome.tool_choice_reset = true;
        }
    }

    outcome.removed = denied;
    outcome
}

/// Resolve the effective policy and per-tool decisions for an arbitrary identity,
/// without touching a request. Backs the portal's "what would user X see?" simulator.
pub async fn simulate(
    ctx: &RequestContext,
    tool_names: &[String],
) -> (EffectivePolicy, Vec<(String, Decision)>) {
    let eff = cache::effective_policy(ctx).await;
    let (catalog_tools, catalog_servers) = cache::catalog_snapshot().await;

    let names: Vec<String> = if tool_names.is_empty() {
        // Default to the whole known catalog, so the simulator is useful with no input.
        catalog_tools.keys().cloned().collect()
    } else {
        tool_names.to_vec()
    };

    let mut out: Vec<(String, Decision)> = names
        .into_iter()
        .map(|n| {
            let d = policy::decide(&n, &eff, &catalog_tools, &catalog_servers);
            (n, d)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));

    (eff, out)
}

/// Convenience re-export so callers can build a catalog map without depending on the
/// internal cache representation.
pub type CatalogMap = HashMap<String, CatalogStatus>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_names_skips_server_side_tools_without_a_name() {
        let tools = vec![
            json!({ "name": "mcp__github__create_issue", "input_schema": {} }),
            // Anthropic server-side tool: type only, no name.
            json!({ "type": "web_search_20250305" }),
            json!({ "name": "Bash", "input_schema": {} }),
        ];
        assert_eq!(
            tool_names_of(&tools),
            vec!["mcp__github__create_issue".to_string(), "Bash".to_string()]
        );
    }

    #[test]
    fn mode_and_action_round_trip() {
        for m in [Mode::Observe, Mode::Warn, Mode::Enforce] {
            assert_eq!(Mode::parse(m.as_str()), m);
        }
        for a in [Action::Allow, Action::Deny] {
            assert_eq!(Action::parse(a.as_str()), a);
        }
        for s in [AppliesTo::McpOnly, AppliesTo::AllTools] {
            assert_eq!(AppliesTo::parse(s.as_str()), s);
        }
    }

    #[test]
    fn unknown_enum_strings_fall_back_to_safe_defaults() {
        // A value written by a newer version of the extension must not escalate
        // enforcement on an older gateway.
        assert_eq!(Mode::parse("something_new"), Mode::Observe);
        assert_eq!(Action::parse("something_new"), Action::Allow);
        assert_eq!(AppliesTo::parse("something_new"), AppliesTo::McpOnly);
        assert_eq!(CatalogStatus::parse_server("weird"), CatalogStatus::Pending);
        assert_eq!(CatalogStatus::parse_tool("inherit"), CatalogStatus::Pending);
    }

    #[test]
    fn default_effective_policy_is_inert() {
        let eff = EffectivePolicy::default();
        assert_eq!(eff.mode, Mode::Observe);
        assert_eq!(eff.default_action, Action::Allow);
        assert_eq!(eff.applies_to, AppliesTo::McpOnly);
    }

    #[test]
    fn outcome_noop_detection() {
        assert!(EnforcementOutcome::default().is_noop());
        let with_flag = EnforcementOutcome {
            flagged: vec![DeniedTool {
                tool_name: "x".into(),
                server_name: None,
                reason: "r".into(),
                decided_by: "global".into(),
            }],
            ..Default::default()
        };
        assert!(!with_flag.is_noop());
    }

    // ---- apply(): the request mutation semantics -------------------------

    fn tool(name: &str) -> Value {
        json!({ "name": name, "input_schema": { "type": "object" } })
    }

    fn denial(name: &str) -> DeniedTool {
        DeniedTool {
            tool_name: name.to_string(),
            server_name: None,
            reason: "test".into(),
            decided_by: "global".into(),
        }
    }

    fn names_in(tools: &Option<Vec<Value>>) -> Vec<String> {
        tools
            .as_ref()
            .map(|l| {
                l.iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn observe_mode_leaves_the_request_byte_identical() {
        let original = vec![tool("mcp__a__x"), tool("Bash")];
        let mut tools = Some(original.clone());
        let mut tc = Some(json!({ "type": "tool", "name": "mcp__a__x" }));

        let out = apply(
            &mut tools,
            &mut tc,
            &mut [],
            vec![denial("mcp__a__x")],
            Mode::Observe,
        );

        assert_eq!(tools, Some(original));
        assert_eq!(tc, Some(json!({ "type": "tool", "name": "mcp__a__x" })));
        assert_eq!(out.flagged.len(), 1);
        assert!(out.removed.is_empty());
        assert!(!out.tool_choice_reset);
    }

    #[test]
    fn warn_mode_leaves_the_request_byte_identical() {
        let original = vec![tool("mcp__a__x")];
        let mut tools = Some(original.clone());
        let mut tc = None;

        let out = apply(
            &mut tools,
            &mut tc,
            &mut [],
            vec![denial("mcp__a__x")],
            Mode::Warn,
        );

        assert_eq!(tools, Some(original));
        assert_eq!(out.flagged.len(), 1);
        assert!(out.removed.is_empty());
    }

    #[test]
    fn enforce_mode_strips_only_the_denied_definitions() {
        let mut tools = Some(vec![
            tool("mcp__github__create_issue"),
            tool("mcp__github__delete_repo"),
            tool("mcp__evil__exfiltrate"),
            tool("Bash"),
        ]);
        let mut tc = None;

        let out = apply(
            &mut tools,
            &mut tc,
            &mut [],
            vec![
                denial("mcp__github__delete_repo"),
                denial("mcp__evil__exfiltrate"),
            ],
            Mode::Enforce,
        );

        assert_eq!(
            names_in(&tools),
            vec!["mcp__github__create_issue".to_string(), "Bash".to_string()]
        );
        assert_eq!(out.removed.len(), 2);
        assert!(out.flagged.is_empty());
    }

    #[test]
    fn stripping_every_tool_drops_the_field_instead_of_sending_an_empty_array() {
        // Bedrock rejects `"tools": []` on some model builds, so the field must go.
        let mut tools = Some(vec![tool("mcp__a__x"), tool("mcp__b__y")]);
        let mut tc = None;

        apply(
            &mut tools,
            &mut tc,
            &mut [],
            vec![denial("mcp__a__x"), denial("mcp__b__y")],
            Mode::Enforce,
        );

        assert!(
            tools.is_none(),
            "expected tools to be absent, got {tools:?}"
        );
    }

    #[test]
    fn tool_choice_naming_a_stripped_tool_is_relaxed_to_auto() {
        let mut tools = Some(vec![tool("mcp__a__x"), tool("Bash")]);
        let mut tc = Some(json!({ "type": "tool", "name": "mcp__a__x" }));

        let out = apply(
            &mut tools,
            &mut tc,
            &mut [],
            vec![denial("mcp__a__x")],
            Mode::Enforce,
        );

        assert_eq!(tc, Some(json!({ "type": "auto" })));
        assert!(out.tool_choice_reset);
    }

    #[test]
    fn tool_choice_is_dropped_when_no_tools_survive() {
        let mut tools = Some(vec![tool("mcp__a__x")]);
        let mut tc = Some(json!({ "type": "tool", "name": "mcp__a__x" }));

        let out = apply(
            &mut tools,
            &mut tc,
            &mut [],
            vec![denial("mcp__a__x")],
            Mode::Enforce,
        );

        assert!(tools.is_none());
        assert!(tc.is_none());
        assert!(out.tool_choice_reset);
    }

    #[test]
    fn unrelated_tool_choice_is_left_alone() {
        let mut tools = Some(vec![tool("mcp__a__x"), tool("Bash")]);
        let mut tc = Some(json!({ "type": "auto" }));

        let out = apply(
            &mut tools,
            &mut tc,
            &mut [],
            vec![denial("mcp__a__x")],
            Mode::Enforce,
        );

        assert_eq!(tc, Some(json!({ "type": "auto" })));
        assert!(!out.tool_choice_reset);
    }

    // ---- tool_names_in_history() -----------------------------------------

    #[test]
    fn history_names_come_only_from_assistant_tool_use_blocks() {
        let msgs = vec![
            json!({ "role": "user", "content": "hi" }),
            json!({ "role": "assistant", "content": [
                { "type": "text", "text": "ok" },
                { "type": "tool_use", "id": "a", "name": "mcp__x__one", "input": {} },
                { "type": "tool_use", "id": "b", "name": "mcp__y__two", "input": {} }
            ]}),
            // tool_result carries an id, not a name: it must not contribute a name.
            json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "a", "content": "1" }
            ]}),
            // Repeat of an earlier tool: one decision, not two.
            json!({ "role": "assistant", "content": [
                { "type": "tool_use", "id": "c", "name": "mcp__x__one", "input": {} }
            ]}),
            // String content and a missing content field must not panic.
            json!({ "role": "assistant", "content": "plain text" }),
            json!({ "role": "assistant" }),
        ];

        assert_eq!(
            tool_names_in_history(&msgs),
            vec!["mcp__x__one".to_string(), "mcp__y__two".to_string()]
        );
    }

    #[test]
    fn history_of_a_fresh_session_is_empty() {
        let msgs = vec![json!({ "role": "user", "content": "hola" })];
        assert!(tool_names_in_history(&msgs).is_empty());
    }

    // ---- scrub_history() -------------------------------------------------
    //
    // Regression cover for the gap that made enforcement leak: stripping the tool
    // definition is not enough, because the model re-invokes the tool from its memory
    // of an earlier turn and the client then executes it locally.

    fn denied_set<'a>(names: &'a [&'a str]) -> std::collections::HashSet<&'a str> {
        names.iter().copied().collect()
    }

    /// A transcript that already used a tool successfully, as a live session would have.
    fn transcript_with_tool_use() -> Vec<Value> {
        vec![
            json!({ "role": "user", "content": "resolve react" }),
            json!({
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Looking that up." },
                    { "type": "tool_use", "id": "tu_1",
                      "name": "mcp__context7__resolve-library-id",
                      "input": { "libraryName": "React" } }
                ]
            }),
            json!({
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "tu_1", "content": "/facebook/react" }
                ]
            }),
            json!({ "role": "assistant", "content": "It is /facebook/react." }),
        ]
    }

    fn block_types(msg: &Value) -> Vec<String> {
        msg.get("content")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|b| b.get("type").and_then(|t| t.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn scrub_removes_tool_use_and_its_paired_result() {
        let mut msgs = transcript_with_tool_use();
        let stats = scrub_history(
            &mut msgs,
            &denied_set(&["mcp__context7__resolve-library-id"]),
        );

        assert_eq!(stats.tool_uses, 1);
        assert_eq!(stats.tool_results, 1);

        // The assistant turn keeps its text but loses the call.
        assert_eq!(block_types(&msgs[1]), vec!["text".to_string()]);
        // The user turn that only held the result is backfilled, never left empty.
        assert_eq!(block_types(&msgs[2]), vec!["text".to_string()]);
    }

    #[test]
    fn scrub_leaves_allowed_tools_untouched() {
        let mut msgs = transcript_with_tool_use();
        let before = msgs.clone();
        let stats = scrub_history(&mut msgs, &denied_set(&["mcp__other__thing"]));

        assert!(stats.is_empty());
        assert_eq!(msgs, before);
    }

    #[test]
    fn scrub_never_leaves_content_empty() {
        // An assistant turn whose only block is the denied call.
        let mut msgs = vec![
            json!({
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "tu_1", "name": "mcp__bad__x", "input": {} }
                ]
            }),
            json!({
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "tu_1", "content": "ok" }
                ]
            }),
        ];
        let stats = scrub_history(&mut msgs, &denied_set(&["mcp__bad__x"]));

        assert_eq!(stats.placeholders, 2);
        for m in &msgs {
            let types = block_types(m);
            assert!(!types.is_empty(), "empty content is rejected by the API");
        }
    }

    #[test]
    fn scrub_keeps_pairing_valid_when_a_turn_mixes_tools() {
        // One assistant turn calling two tools, only one denied. The surviving
        // tool_use must keep its result, and the denied one must lose its own.
        let mut msgs = vec![
            json!({
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "keep", "name": "mcp__ok__a", "input": {} },
                    { "type": "tool_use", "id": "drop", "name": "mcp__bad__b", "input": {} }
                ]
            }),
            json!({
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "keep", "content": "1" },
                    { "type": "tool_result", "tool_use_id": "drop", "content": "2" }
                ]
            }),
        ];
        let stats = scrub_history(&mut msgs, &denied_set(&["mcp__bad__b"]));

        assert_eq!(stats.tool_uses, 1);
        assert_eq!(stats.tool_results, 1);

        let uses: Vec<&str> = msgs[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("id").and_then(|i| i.as_str()))
            .collect();
        let results: Vec<&str> = msgs[1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("tool_use_id").and_then(|i| i.as_str()))
            .collect();
        assert_eq!(uses, vec!["keep"]);
        assert_eq!(results, vec!["keep"]);
    }

    #[test]
    fn scrub_tolerates_string_content_and_missing_fields() {
        // Plain-string content and malformed blocks must not panic.
        let mut msgs = vec![
            json!({ "role": "user", "content": "hola" }),
            json!({ "role": "assistant", "content": "hola" }),
            json!({ "role": "assistant", "content": [ { "type": "tool_use" } ] }),
            json!({ "role": "user" }),
        ];
        let stats = scrub_history(&mut msgs, &denied_set(&["mcp__bad__x"]));
        assert!(stats.is_empty());
    }

    #[test]
    fn observe_mode_does_not_scrub_history() {
        let mut tools = Some(vec![tool("mcp__context7__resolve-library-id")]);
        let mut tc = None;
        let mut msgs = transcript_with_tool_use();
        let before = msgs.clone();

        let out = apply(
            &mut tools,
            &mut tc,
            &mut msgs,
            vec![denial("mcp__context7__resolve-library-id")],
            Mode::Observe,
        );

        assert!(out.history.is_empty());
        assert_eq!(msgs, before, "observe mode must not touch the transcript");
    }

    #[test]
    fn enforce_mode_strips_definition_and_history_together() {
        let mut tools = Some(vec![
            tool("mcp__context7__resolve-library-id"),
            tool("mcp__everything__echo"),
        ]);
        let mut tc = None;
        let mut msgs = transcript_with_tool_use();

        let out = apply(
            &mut tools,
            &mut tc,
            &mut msgs,
            vec![denial("mcp__context7__resolve-library-id")],
            Mode::Enforce,
        );

        assert_eq!(names_in(&tools), vec!["mcp__everything__echo".to_string()]);
        assert_eq!(out.history.tool_uses, 1);
        // No trace of the tool name is left anywhere in the request.
        let rendered = serde_json::to_string(&msgs).unwrap();
        assert!(!rendered.contains("mcp__context7__resolve-library-id"));
    }

    #[test]
    fn a_tool_present_only_in_history_is_still_scrubbed() {
        // The shape of a *continuing* session, and the exact case that leaked: the client
        // stopped declaring the tool because a previous turn already had it stripped, so
        // judging `tools` alone finds nothing to do and the transcript sails through.
        let mut tools = Some(vec![tool("mcp__everything__echo")]);
        let mut tc = None;
        let mut msgs = transcript_with_tool_use();

        let out = apply(
            &mut tools,
            &mut tc,
            &mut msgs,
            vec![denial("mcp__context7__resolve-library-id")],
            Mode::Enforce,
        );

        assert_eq!(out.history.tool_uses, 1);
        assert_eq!(out.history.tool_results, 1);
        // The declarations were already clean and must be left exactly as they were.
        assert_eq!(names_in(&tools), vec!["mcp__everything__echo".to_string()]);
        let rendered = serde_json::to_string(&msgs).unwrap();
        assert!(!rendered.contains("mcp__context7__resolve-library-id"));
    }

    #[test]
    fn server_side_tools_without_a_name_are_never_stripped() {
        // Anthropic's web_search tool has a `type` but no `name`. CCAG's own
        // interception owns it; MCP policy must leave it in place.
        let web_search = json!({ "type": "web_search_20250305", "name_ignored": true });
        let mut tools = Some(vec![tool("mcp__a__x"), web_search.clone()]);
        let mut tc = None;

        apply(
            &mut tools,
            &mut tc,
            &mut [],
            vec![denial("mcp__a__x")],
            Mode::Enforce,
        );

        assert_eq!(tools, Some(vec![web_search]));
    }
}
