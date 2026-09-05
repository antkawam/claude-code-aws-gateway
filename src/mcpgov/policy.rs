//! Pure policy engine: tool-name parsing, glob matching, scope resolution, and the
//! allow/deny decision.
//!
//! Everything here is synchronous and side-effect free so it can be unit tested
//! without a database. The DB-backed pieces live in [`super::store`] and
//! [`super::cache`].

use std::collections::HashMap;

use super::{
    Action, AppliesTo, CatalogStatus, DecidedBy, Decision, EffectivePolicy, PolicyRow,
    ServerStatus, ToolRef,
};

/// The prefix Claude Code uses to namespace MCP tools: `mcp__<server>__<tool>`.
const MCP_PREFIX: &str = "mcp__";

/// Split a tool name into its MCP server and tool parts.
///
/// Claude Code namespaces MCP tools as `mcp__<server>__<tool>`. Server names cannot
/// contain `__` (it is the delimiter), but tool names can — AgentCore Gateway, for
/// instance, names its tools `Target___tool` with three underscores. So we split on
/// the *first* `__` after the prefix and treat the remainder as the tool segment.
///
/// Non-MCP builtin tools (`Read`, `Bash`, ...) yield `server: None`.
pub fn parse_tool_name(tool_name: &str) -> ToolRef<'_> {
    match tool_name.strip_prefix(MCP_PREFIX) {
        Some(rest) => {
            let mut parts = rest.splitn(2, "__");
            let server = parts.next().unwrap_or_default();
            let tool = parts.next();
            match tool {
                // `mcp__github__create_issue` -> ("github", "create_issue")
                Some(t) if !server.is_empty() => ToolRef {
                    full: tool_name,
                    server: Some(server),
                    tool: t,
                    is_mcp: true,
                },
                // Malformed (`mcp__github`, `mcp____x`): still MCP-ish, but we cannot
                // attribute it to a server. Treated as MCP with no server so that
                // `mcp_only` policies still cover it.
                _ => ToolRef {
                    full: tool_name,
                    server: None,
                    tool: rest,
                    is_mcp: true,
                },
            }
        }
        None => ToolRef {
            full: tool_name,
            server: None,
            tool: tool_name,
            is_mcp: false,
        },
    }
}

/// Match a tool name against a glob pattern supporting `*` wildcards anywhere.
///
/// `*` matches any run of characters including none. Matching is case-sensitive,
/// because tool names are. A bare `*` matches everything.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    // Fast paths for the overwhelmingly common shapes.
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }

    // Split on '*' and walk the literal segments left to right. A leading or
    // trailing empty segment means the pattern was anchored with a wildcard there.
    let segments: Vec<&str> = pattern.split('*').collect();
    let last = segments.len() - 1;
    let mut cursor = 0usize;

    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 {
            // Pattern does not start with '*': the first segment must be a prefix.
            if !value[cursor..].starts_with(seg) {
                return false;
            }
            cursor += seg.len();
        } else if i == last {
            // Pattern does not end with '*': the last segment must be a suffix,
            // and must not overlap what we have already consumed.
            if !value[cursor..].ends_with(seg) {
                return false;
            }
        } else {
            match value[cursor..].find(seg) {
                Some(pos) => cursor += pos + seg.len(),
                None => return false,
            }
        }
    }

    true
}

fn matches_any(patterns: &[String], value: &str) -> Option<String> {
    patterns.iter().find(|p| glob_match(p, value)).cloned()
}

/// Merge the policy rows that apply to one identity into a single effective policy.
///
/// Rows are supplied most-general first (`global`, then `team`, then `user`); any of
/// them may be absent. Merge semantics deliberately mirror the Claude apps gateway's
/// managed-policy rules, so that a policy authored here can later be emitted as
/// client-side managed settings without changing meaning:
///
/// * **Scalars** (`mode`, `default_action`, `applies_to`) — most specific scope wins.
/// * **Deny lists** — union of every scope. An org-wide deny can never be dropped by
///   a narrower override.
/// * **Allow lists** — the most specific *non-empty* list replaces broader ones,
///   rather than unioning, so a team allowlist genuinely narrows rather than widens.
///
/// Disabled rows are ignored entirely.
pub fn resolve(
    global: Option<&PolicyRow>,
    team: Option<&PolicyRow>,
    user: Option<&PolicyRow>,
) -> EffectivePolicy {
    let mut eff = EffectivePolicy::default();
    let mut deny: Vec<String> = Vec::new();

    // Ordered general -> specific, so later assignments win for scalars.
    for row in [global, team, user].into_iter().flatten() {
        if !row.enabled {
            continue;
        }
        eff.mode = row.mode;
        eff.default_action = row.default_action;
        eff.applies_to = row.applies_to;
        eff.decided_by = row.scope_kind();

        for p in &row.deny_patterns {
            if !deny.contains(p) {
                deny.push(p.clone());
            }
        }
        if !row.allow_patterns.is_empty() {
            eff.allow_patterns = row.allow_patterns.clone();
        }
    }

    eff.deny_patterns = deny;
    eff
}

/// The catalog decision for a single tool: an explicit per-tool status if one exists,
/// otherwise the status inherited from its MCP server.
pub fn catalog_status(
    tool: &ToolRef<'_>,
    tools: &HashMap<String, CatalogStatus>,
    servers: &HashMap<String, ServerStatus>,
) -> CatalogStatus {
    match tools.get(tool.full) {
        Some(CatalogStatus::Approved) => return CatalogStatus::Approved,
        Some(CatalogStatus::Denied) => return CatalogStatus::Denied,
        // 'inherit' or absent: fall through to the server.
        _ => {}
    }

    match tool.server.and_then(|s| servers.get(s)) {
        Some(ServerStatus::Approved) => CatalogStatus::Approved,
        Some(ServerStatus::Denied) => CatalogStatus::Denied,
        _ => CatalogStatus::Pending,
    }
}

/// Decide whether a single tool is permitted.
///
/// Precedence, highest first. Deny always wins, matching Cedar's
/// forbid-overrides-permit model and Claude Code's own denylist semantics:
///
/// 1. A deny pattern matches -> DENY.
/// 2. The catalog marks the tool (or its server) denied -> DENY.
/// 3. An allow list is set and the tool matches -> ALLOW.
/// 4. An allow list is set and the tool does not match -> DENY (allowlists are exclusive).
/// 5. The catalog marks the tool (or its server) approved -> ALLOW.
/// 6. Nothing decided -> the policy's `default_action`.
///
/// Note this returns the *policy* verdict. It does not consider `mode`; converting a
/// verdict into an action (strip / flag / ignore) is [`Decision::apply_mode`].
pub fn decide(
    tool_name: &str,
    eff: &EffectivePolicy,
    tools: &HashMap<String, CatalogStatus>,
    servers: &HashMap<String, ServerStatus>,
) -> Decision {
    let tool = parse_tool_name(tool_name);

    // Builtin tools are out of scope unless the policy opts into governing them.
    if eff.applies_to == AppliesTo::McpOnly && !tool.is_mcp {
        return Decision::allow(
            "not an MCP tool (policy scope is mcp_only)",
            DecidedBy::Unscoped,
        );
    }

    if let Some(p) = matches_any(&eff.deny_patterns, tool_name) {
        return Decision::deny(format!("matched deny pattern `{p}`"), eff.decided_by);
    }

    let cat = catalog_status(&tool, tools, servers);
    if cat == CatalogStatus::Denied {
        return Decision::deny("denied in catalog".to_string(), DecidedBy::Catalog);
    }

    if !eff.allow_patterns.is_empty() {
        return match matches_any(&eff.allow_patterns, tool_name) {
            Some(p) => {
                Decision::allow_owned(format!("matched allow pattern `{p}`"), eff.decided_by)
            }
            None => Decision::deny("not on the allowlist".to_string(), eff.decided_by),
        };
    }

    if cat == CatalogStatus::Approved {
        return Decision::allow("approved in catalog", DecidedBy::Catalog);
    }

    match eff.default_action {
        Action::Allow => Decision::allow("not in catalog; default action is allow", eff.decided_by),
        Action::Deny => Decision::deny(
            "not in catalog; default action is deny".to_string(),
            eff.decided_by,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcpgov::Mode;

    fn pol(
        scope: &str,
        mode: Mode,
        default_action: Action,
        allow: &[&str],
        deny: &[&str],
    ) -> PolicyRow {
        PolicyRow {
            scope: scope.to_string(),
            scope_ref: if scope == "global" {
                None
            } else {
                Some("ref".to_string())
            },
            mode,
            default_action,
            applies_to: AppliesTo::McpOnly,
            allow_patterns: allow.iter().map(|s| s.to_string()).collect(),
            deny_patterns: deny.iter().map(|s| s.to_string()).collect(),
            enabled: true,
        }
    }

    fn empty_catalog() -> (
        HashMap<String, CatalogStatus>,
        HashMap<String, ServerStatus>,
    ) {
        (HashMap::new(), HashMap::new())
    }

    // ---- parse_tool_name -------------------------------------------------

    #[test]
    fn parses_standard_mcp_tool_name() {
        let t = parse_tool_name("mcp__github__create_issue");
        assert!(t.is_mcp);
        assert_eq!(t.server, Some("github"));
        assert_eq!(t.tool, "create_issue");
    }

    #[test]
    fn parses_builtin_tool_name() {
        let t = parse_tool_name("Bash");
        assert!(!t.is_mcp);
        assert_eq!(t.server, None);
        assert_eq!(t.tool, "Bash");
    }

    #[test]
    fn tool_segment_may_contain_double_underscores() {
        // AgentCore Gateway names tools `Target___tool` (three underscores); the
        // extra delimiters belong to the tool segment, not the server.
        let t = parse_tool_name("mcp__awsgw__RefundTool___process_refund");
        assert_eq!(t.server, Some("awsgw"));
        assert_eq!(t.tool, "RefundTool___process_refund");
    }

    #[test]
    fn malformed_mcp_name_has_no_server_but_is_still_mcp() {
        let t = parse_tool_name("mcp__github");
        assert!(t.is_mcp);
        assert_eq!(t.server, None);
    }

    // ---- glob_match ------------------------------------------------------

    #[test]
    fn glob_star_matches_everything() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_exact_match_without_wildcard() {
        assert!(glob_match("mcp__github__x", "mcp__github__x"));
        assert!(!glob_match("mcp__github__x", "mcp__github__y"));
    }

    #[test]
    fn glob_prefix_wildcard() {
        assert!(glob_match("mcp__github__*", "mcp__github__create_issue"));
        assert!(!glob_match("mcp__github__*", "mcp__gitlab__create_issue"));
    }

    #[test]
    fn glob_suffix_wildcard() {
        assert!(glob_match("*__delete_repo", "mcp__github__delete_repo"));
        assert!(!glob_match("*__delete_repo", "mcp__github__create_repo"));
    }

    #[test]
    fn glob_middle_wildcard() {
        assert!(glob_match("mcp__*__delete_*", "mcp__github__delete_repo"));
        assert!(!glob_match("mcp__*__delete_*", "mcp__github__create_repo"));
    }

    #[test]
    fn glob_does_not_match_partial_prefix() {
        assert!(!glob_match("mcp__github__*", "mcp__github"));
    }

    // ---- resolve ---------------------------------------------------------

    #[test]
    fn deny_patterns_union_across_scopes() {
        let g = pol(
            "global",
            Mode::Enforce,
            Action::Allow,
            &[],
            &["mcp__evil__*"],
        );
        let t = pol(
            "team",
            Mode::Enforce,
            Action::Allow,
            &[],
            &["mcp__risky__*"],
        );
        let eff = resolve(Some(&g), Some(&t), None);
        assert!(eff.deny_patterns.contains(&"mcp__evil__*".to_string()));
        assert!(eff.deny_patterns.contains(&"mcp__risky__*".to_string()));
    }

    #[test]
    fn allow_patterns_replace_rather_than_union() {
        let g = pol(
            "global",
            Mode::Enforce,
            Action::Allow,
            &["mcp__a__*", "mcp__b__*"],
            &[],
        );
        let t = pol("team", Mode::Enforce, Action::Allow, &["mcp__a__*"], &[]);
        let eff = resolve(Some(&g), Some(&t), None);
        assert_eq!(eff.allow_patterns, vec!["mcp__a__*".to_string()]);
    }

    #[test]
    fn empty_allow_list_does_not_clobber_broader_scope() {
        let g = pol("global", Mode::Enforce, Action::Allow, &["mcp__a__*"], &[]);
        let t = pol("team", Mode::Enforce, Action::Allow, &[], &[]);
        let eff = resolve(Some(&g), Some(&t), None);
        assert_eq!(eff.allow_patterns, vec!["mcp__a__*".to_string()]);
    }

    #[test]
    fn most_specific_scope_wins_for_mode() {
        let g = pol("global", Mode::Observe, Action::Allow, &[], &[]);
        let u = pol("user", Mode::Enforce, Action::Deny, &[], &[]);
        let eff = resolve(Some(&g), None, Some(&u));
        assert_eq!(eff.mode, Mode::Enforce);
        assert_eq!(eff.default_action, Action::Deny);
        assert_eq!(eff.decided_by, DecidedBy::User);
    }

    #[test]
    fn disabled_rows_are_ignored() {
        let g = pol("global", Mode::Observe, Action::Allow, &[], &[]);
        let mut t = pol("team", Mode::Enforce, Action::Deny, &[], &["mcp__x__*"]);
        t.enabled = false;
        let eff = resolve(Some(&g), Some(&t), None);
        assert_eq!(eff.mode, Mode::Observe);
        assert!(eff.deny_patterns.is_empty());
    }

    // ---- decide ----------------------------------------------------------

    #[test]
    fn builtin_tools_untouched_when_scope_is_mcp_only() {
        let (tools, servers) = empty_catalog();
        let eff = resolve(
            Some(&pol("global", Mode::Enforce, Action::Deny, &[], &["*"])),
            None,
            None,
        );
        // Even a deny-everything policy must not touch builtins in mcp_only scope.
        assert!(decide("Bash", &eff, &tools, &servers).allowed);
        assert!(decide("Read", &eff, &tools, &servers).allowed);
        // ...but MCP tools are denied.
        assert!(!decide("mcp__github__x", &eff, &tools, &servers).allowed);
    }

    #[test]
    fn all_tools_scope_governs_builtins() {
        let (tools, servers) = empty_catalog();
        let mut row = pol("global", Mode::Enforce, Action::Deny, &[], &[]);
        row.applies_to = AppliesTo::AllTools;
        let eff = resolve(Some(&row), None, None);
        assert!(!decide("Bash", &eff, &tools, &servers).allowed);
    }

    #[test]
    fn deny_pattern_beats_allow_pattern() {
        let (tools, servers) = empty_catalog();
        let eff = resolve(
            Some(&pol(
                "global",
                Mode::Enforce,
                Action::Allow,
                &["mcp__github__*"],
                &["mcp__github__delete_*"],
            )),
            None,
            None,
        );
        assert!(decide("mcp__github__create_issue", &eff, &tools, &servers).allowed);
        assert!(!decide("mcp__github__delete_repo", &eff, &tools, &servers).allowed);
    }

    #[test]
    fn deny_pattern_beats_catalog_approval() {
        let mut servers = HashMap::new();
        servers.insert("github".to_string(), ServerStatus::Approved);
        let tools = HashMap::new();
        let eff = resolve(
            Some(&pol(
                "global",
                Mode::Enforce,
                Action::Allow,
                &[],
                &["mcp__github__*"],
            )),
            None,
            None,
        );
        assert!(!decide("mcp__github__create_issue", &eff, &tools, &servers).allowed);
    }

    #[test]
    fn catalog_denial_beats_allow_pattern() {
        let mut servers = HashMap::new();
        servers.insert("github".to_string(), ServerStatus::Denied);
        let tools = HashMap::new();
        let eff = resolve(
            Some(&pol(
                "global",
                Mode::Enforce,
                Action::Allow,
                &["mcp__github__*"],
                &[],
            )),
            None,
            None,
        );
        let d = decide("mcp__github__x", &eff, &tools, &servers);
        assert!(!d.allowed);
        assert_eq!(d.decided_by, DecidedBy::Catalog);
    }

    #[test]
    fn per_tool_status_overrides_server_status() {
        let mut servers = HashMap::new();
        servers.insert("github".to_string(), ServerStatus::Approved);
        let mut tools = HashMap::new();
        tools.insert(
            "mcp__github__delete_repo".to_string(),
            CatalogStatus::Denied,
        );

        let eff = resolve(
            Some(&pol("global", Mode::Enforce, Action::Deny, &[], &[])),
            None,
            None,
        );
        assert!(decide("mcp__github__create_issue", &eff, &tools, &servers).allowed);
        assert!(!decide("mcp__github__delete_repo", &eff, &tools, &servers).allowed);
    }

    #[test]
    fn exclusive_allowlist_denies_unlisted_tools() {
        let (tools, servers) = empty_catalog();
        let eff = resolve(
            Some(&pol(
                "global",
                Mode::Enforce,
                Action::Allow,
                &["mcp__github__*"],
                &[],
            )),
            None,
            None,
        );
        // default_action is Allow, but a non-empty allowlist is exclusive.
        assert!(!decide("mcp__slack__post", &eff, &tools, &servers).allowed);
    }

    #[test]
    fn default_action_governs_unknown_tools() {
        let (tools, servers) = empty_catalog();

        let allow_eff = resolve(
            Some(&pol("global", Mode::Enforce, Action::Allow, &[], &[])),
            None,
            None,
        );
        assert!(decide("mcp__brand_new__x", &allow_eff, &tools, &servers).allowed);

        let deny_eff = resolve(
            Some(&pol("global", Mode::Enforce, Action::Deny, &[], &[])),
            None,
            None,
        );
        assert!(!decide("mcp__brand_new__x", &deny_eff, &tools, &servers).allowed);
    }

    #[test]
    fn catalog_approval_satisfies_deny_by_default() {
        let mut servers = HashMap::new();
        servers.insert("github".to_string(), ServerStatus::Approved);
        let tools = HashMap::new();
        let eff = resolve(
            Some(&pol("global", Mode::Enforce, Action::Deny, &[], &[])),
            None,
            None,
        );
        assert!(decide("mcp__github__x", &eff, &tools, &servers).allowed);
        assert!(!decide("mcp__unknown__x", &eff, &tools, &servers).allowed);
    }

    #[test]
    fn no_policy_rows_defaults_to_permissive_observe() {
        let (tools, servers) = empty_catalog();
        let eff = resolve(None, None, None);
        assert_eq!(eff.mode, Mode::Observe);
        assert!(decide("mcp__anything__x", &eff, &tools, &servers).allowed);
    }
}
