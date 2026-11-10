//! Response-side refusal of denied tool calls.
//!
//! # Why this exists
//!
//! Request-side filtering cannot be made airtight, and it took three attempts to accept
//! that. The overlay first stripped denied definitions from `tools[]`; a continuing
//! session kept working, because the model re-invoked the tool from the `tool_use` blocks
//! still in its transcript. The overlay then also scrubbed the transcript; a real session
//! *still* executed the tool. The reason is simple once seen: a model will emit a
//! `tool_use` block for any name it has reason to believe exists, and the client executes
//! whatever it is asked for. Merely mentioning the tool in the system prompt — which
//! Claude Code does when it enumerates connected MCP servers — is enough. Verified
//! against the live gateway: with `tools[]` clean and no history at all, a system prompt
//! naming `mcp__context7__resolve-library-id` produced a `tool_use` for exactly that.
//!
//! There is no finite set of inputs to sanitise, because the leak is the model's
//! generative capacity, not any one field. So enforcement has to happen at the only point
//! where the tool name is a fact rather than a possibility: the response.
//!
//! Request-side stripping is still worth keeping — it stops the model wanting the tool in
//! the first place, which avoids a wasted turn — but this module is what actually
//! guarantees the call never reaches the client.
//!
//! # Why this is cheaper than it looks
//!
//! The original design note claimed response interception needed a `partial_json`
//! accumulator. It does not. A tool call's name arrives in `content_block_start`, before
//! any `input_json_delta`, so the verdict is available immediately and the argument
//! deltas can simply be discarded. Only the *arguments* need reassembly, and policy does
//! not look at arguments.
//!
//! # What a refusal looks like on the wire
//!
//! A denied `tool_use` block becomes a `text` block explaining the block, and if it was
//! the only tool call in the turn `stop_reason` is relaxed from `tool_use` to `end_turn`.
//! Rewriting rather than deleting matters: dropping the block would leave a
//! `stop_reason: tool_use` with nothing to execute, and would open a gap in the streamed
//! block indices. A turn that also called permitted tools keeps `stop_reason: tool_use`,
//! so those still run.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    CatalogStatus, DeniedTool, EffectivePolicy, Mode, RequestContext, cache, policy, sink,
};

/// What the caller should emit for a stream event it just handed to the guard.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamAction {
    /// Emit the event as it now stands. It may have been mutated in place.
    Forward,
    /// Emit nothing: this event belonged to a refused tool call.
    Drop,
    /// Emit the event, then this synthetic follow-up.
    ForwardThen(Value),
}

/// Per-request policy snapshot plus the streaming state machine.
///
/// Owned and `Send + 'static` on purpose: `handle_streaming` moves its generator into a
/// spawned task, so the guard cannot borrow from `GatewayState`.
///
/// `Clone` because endpoint failover retries the whole call and each attempt needs its own
/// state machine. Cloning copies two catalog maps, which is affordable on a path that only
/// runs after an upstream throttle or 5xx.
#[derive(Clone)]
pub struct ResponseGuard {
    eff: EffectivePolicy,
    tools: HashMap<String, CatalogStatus>,
    servers: HashMap<String, CatalogStatus>,
    ctx: RequestContext,

    /// Names the request side already wrote an audit row for. Without this, denying a
    /// declared tool would log the same block twice for one request.
    already_audited: HashSet<String>,

    /// Memoised verdicts. A turn commonly retries the same tool, and a decision walks
    /// the pattern lists and both catalog maps.
    verdicts: HashMap<String, bool>,

    /// Streamed block indices whose `tool_use` was refused, so their argument deltas can
    /// be dropped.
    suppressed: HashSet<i64>,

    /// Distinct tools actually refused, for the audit trail and the log line.
    blocked: Vec<DeniedTool>,

    /// Every `tool_use` seen this response. Compared against `suppressed` to decide
    /// whether `stop_reason` may be relaxed.
    tool_uses_seen: usize,
}

/// Build a guard for this request, or `None` when there is nothing it could ever do.
///
/// Returning `None` is the common case and keeps the response path untouched: policy has
/// to be in `enforce`, and something has to be deniable at all. `observe` and `warn` are
/// recorded on the request side and must not alter a response.
///
/// `already_audited` should be the tool names the request side just logged.
pub async fn guard_for(
    ctx: &RequestContext,
    already_audited: &[DeniedTool],
) -> Option<ResponseGuard> {
    let eff = cache::effective_policy(ctx).await;
    if eff.mode != Mode::Enforce {
        return None;
    }

    let (tools, servers) = cache::catalog_snapshot().await;

    // A policy that denies nothing cannot refuse anything, so skip the whole machinery
    // rather than paying for a decision on every block of every response.
    let can_deny = !eff.deny_patterns.is_empty()
        || !eff.allow_patterns.is_empty()
        || eff.default_action == super::Action::Deny
        || tools.values().any(|s| *s == CatalogStatus::Denied)
        || servers.values().any(|s| *s == CatalogStatus::Denied);
    if !can_deny {
        return None;
    }

    Some(ResponseGuard {
        eff,
        tools,
        servers,
        ctx: ctx.clone(),
        already_audited: already_audited
            .iter()
            .map(|d| d.tool_name.clone())
            .collect(),
        verdicts: HashMap::new(),
        suppressed: HashSet::new(),
        blocked: Vec::new(),
        tool_uses_seen: 0,
    })
}

/// The text that replaces a refused call. Addressed to the developer reading the
/// transcript, since this lands in the assistant's visible output.
fn refusal_text(tool_name: &str) -> String {
    format!(
        "[MCP policy] `{tool_name}` is not permitted for your account, so the call was not executed."
    )
}

impl ResponseGuard {
    /// Distinct tools this response refused.
    pub fn blocked(&self) -> &[DeniedTool] {
        &self.blocked
    }

    /// Whether the guard changed anything, for the caller's log line.
    pub fn acted(&self) -> bool {
        !self.blocked.is_empty()
    }

    /// Decide a name, memoising and auditing the first refusal of each tool.
    fn refuse(&mut self, name: &str) -> bool {
        if let Some(known) = self.verdicts.get(name) {
            return *known;
        }

        let d = policy::decide(name, &self.eff, &self.tools, &self.servers);
        let denied = !d.allowed;
        self.verdicts.insert(name.to_string(), denied);

        if denied {
            let parsed = policy::parse_tool_name(name);
            let record = DeniedTool {
                tool_name: name.to_string(),
                server_name: parsed.server.map(String::from),
                reason: d.reason,
                decided_by: d.decided_by.as_str().to_string(),
            };
            // Only audit a name the request side did not already cover, so one refusal
            // is one row.
            if !self.already_audited.contains(name) {
                sink::record_events(
                    std::slice::from_ref(&record),
                    "blocked",
                    Mode::Enforce,
                    &self.ctx,
                );
            }
            self.blocked.push(record);
        }

        denied
    }

    /// True once every tool call in this response has been refused, meaning the client
    /// has nothing left to execute and `stop_reason: tool_use` would strand it.
    fn all_calls_refused(&self) -> bool {
        self.tool_uses_seen > 0 && self.suppressed.len() == self.tool_uses_seen
    }

    /// Refuse denied calls in a complete (non-streaming) response.
    pub fn rewrite_non_streaming(&mut self, resp: &mut Value) {
        let Some(content) = resp.get_mut("content").and_then(|c| c.as_array_mut()) else {
            return;
        };

        let mut refused = 0usize;
        let mut calls = 0usize;
        for block in content.iter_mut() {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            calls += 1;
            let Some(name) = block.get("name").and_then(|n| n.as_str()).map(String::from) else {
                continue;
            };
            if self.refuse(&name) {
                *block = json!({ "type": "text", "text": refusal_text(&name) });
                refused += 1;
            }
        }

        self.tool_uses_seen += calls;
        if refused == 0 {
            return;
        }

        // Mirror the streaming bookkeeping so `all_calls_refused` is meaningful for both
        // paths. Indices are synthetic here; only the count is used.
        for i in 0..refused {
            self.suppressed.insert(-(i as i64) - 1);
        }

        if refused == calls
            && let Some(obj) = resp.as_object_mut()
        {
            obj.insert("stop_reason".to_string(), json!("end_turn"));
        }
    }

    /// Handle one outbound SSE event.
    ///
    /// Call this after the event has been normalised and immediately before it is
    /// written to the client.
    pub fn on_stream_event(&mut self, event: &mut Value) -> StreamAction {
        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "content_block_start" => {
                let is_tool_use = event
                    .pointer("/content_block/type")
                    .and_then(|t| t.as_str())
                    == Some("tool_use");
                if !is_tool_use {
                    return StreamAction::Forward;
                }
                self.tool_uses_seen += 1;

                let Some(name) = event
                    .pointer("/content_block/name")
                    .and_then(|n| n.as_str())
                    .map(String::from)
                else {
                    return StreamAction::Forward;
                };
                if !self.refuse(&name) {
                    return StreamAction::Forward;
                }

                let index = event.get("index").and_then(|i| i.as_i64()).unwrap_or(-1);
                self.suppressed.insert(index);

                // Become an empty text block, then carry the explanation in a delta.
                // Anthropic's own text blocks open empty and accumulate, so a client that
                // renders from deltas and one that reads the opening block agree.
                if let Some(obj) = event.as_object_mut() {
                    obj.insert(
                        "content_block".to_string(),
                        json!({ "type": "text", "text": "" }),
                    );
                }
                StreamAction::ForwardThen(json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "text_delta", "text": refusal_text(&name) }
                }))
            }

            // Argument deltas for a refused call. Dropping them is safe precisely because
            // policy never inspects arguments.
            "content_block_delta" => {
                let index = event.get("index").and_then(|i| i.as_i64()).unwrap_or(-1);
                if self.suppressed.contains(&index) {
                    StreamAction::Drop
                } else {
                    StreamAction::Forward
                }
            }

            // Forwarded unchanged: the rewritten text block still has to close.
            "content_block_stop" => StreamAction::Forward,

            "message_delta" => {
                if self.all_calls_refused()
                    && let Some(delta) = event.get_mut("delta").and_then(|d| d.as_object_mut())
                    && delta.get("stop_reason").and_then(|s| s.as_str()) == Some("tool_use")
                {
                    delta.insert("stop_reason".to_string(), json!("end_turn"));
                }
                StreamAction::Forward
            }

            _ => StreamAction::Forward,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcpgov::{Action, AppliesTo, DecidedBy};

    /// A guard that denies exactly the given names, without touching the cache.
    fn guard(denied: &[&str]) -> ResponseGuard {
        ResponseGuard {
            eff: EffectivePolicy {
                mode: Mode::Enforce,
                default_action: Action::Allow,
                applies_to: AppliesTo::McpOnly,
                allow_patterns: Vec::new(),
                deny_patterns: denied.iter().map(|s| s.to_string()).collect(),
                decided_by: DecidedBy::Global,
            },
            tools: HashMap::new(),
            servers: HashMap::new(),
            ctx: RequestContext::default(),
            already_audited: HashSet::new(),
            verdicts: HashMap::new(),
            suppressed: HashSet::new(),
            blocked: Vec::new(),
            tool_uses_seen: 0,
        }
    }

    fn start_event(index: i64, name: &str) -> Value {
        json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "tool_use", "id": "toolu_1", "name": name, "input": {} }
        })
    }

    // ---- non-streaming ---------------------------------------------------

    #[test]
    fn refused_call_becomes_text_and_relaxes_stop_reason() {
        let mut g = guard(&["mcp__context7__*"]);
        let mut resp = json!({
            "content": [
                { "type": "text", "text": "Looking that up." },
                { "type": "tool_use", "id": "t1",
                  "name": "mcp__context7__resolve-library-id", "input": { "libraryName": "React" } }
            ],
            "stop_reason": "tool_use"
        });

        g.rewrite_non_streaming(&mut resp);

        assert_eq!(resp["stop_reason"], "end_turn");
        assert_eq!(resp["content"][1]["type"], "text");
        assert_eq!(g.blocked().len(), 1);
        // The name survives only inside the explanation, never as a callable block.
        assert!(
            resp["content"][1]["text"]
                .as_str()
                .unwrap()
                .contains("not permitted")
        );
    }

    #[test]
    fn a_permitted_call_alongside_a_refused_one_keeps_stop_reason() {
        // The client must still execute the tool it is allowed to use.
        let mut g = guard(&["mcp__context7__*"]);
        let mut resp = json!({
            "content": [
                { "type": "tool_use", "id": "t1", "name": "mcp__context7__x", "input": {} },
                { "type": "tool_use", "id": "t2", "name": "mcp__everything__echo", "input": {} }
            ],
            "stop_reason": "tool_use"
        });

        g.rewrite_non_streaming(&mut resp);

        assert_eq!(
            resp["stop_reason"], "tool_use",
            "the allowed call still needs running"
        );
        assert_eq!(resp["content"][0]["type"], "text");
        assert_eq!(resp["content"][1]["type"], "tool_use");
    }

    #[test]
    fn a_response_with_no_denied_calls_is_untouched() {
        let mut g = guard(&["mcp__context7__*"]);
        let mut resp = json!({
            "content": [ { "type": "tool_use", "id": "t1", "name": "mcp__everything__echo", "input": {} } ],
            "stop_reason": "tool_use"
        });
        let before = resp.clone();

        g.rewrite_non_streaming(&mut resp);

        assert_eq!(resp, before);
        assert!(!g.acted());
    }

    #[test]
    fn missing_or_malformed_content_does_not_panic() {
        let mut g = guard(&["mcp__a__*"]);
        let mut no_content = json!({ "stop_reason": "end_turn" });
        g.rewrite_non_streaming(&mut no_content);

        let mut junk = json!({ "content": [ { "type": "tool_use" }, 42, "str" ] });
        g.rewrite_non_streaming(&mut junk);
        assert!(!g.acted());
    }

    // ---- streaming -------------------------------------------------------

    #[test]
    fn refused_stream_block_is_rewritten_and_its_deltas_dropped() {
        let mut g = guard(&["mcp__context7__*"]);

        let mut start = start_event(1, "mcp__context7__resolve-library-id");
        let action = g.on_stream_event(&mut start);

        // The block is no longer a tool call.
        assert_eq!(start["content_block"]["type"], "text");
        assert_eq!(start["content_block"]["text"], "");
        // ... and the explanation follows as a delta.
        match action {
            StreamAction::ForwardThen(extra) => {
                assert_eq!(extra["index"], 1);
                assert_eq!(extra["delta"]["type"], "text_delta");
                assert!(
                    extra["delta"]["text"]
                        .as_str()
                        .unwrap()
                        .contains("not permitted")
                );
            }
            other => panic!("expected ForwardThen, got {other:?}"),
        }

        // Argument deltas for that index are discarded.
        let mut delta = json!({
            "type": "content_block_delta", "index": 1,
            "delta": { "type": "input_json_delta", "partial_json": "{\"libraryName\":" }
        });
        assert_eq!(g.on_stream_event(&mut delta), StreamAction::Drop);

        // The block still closes, or the client waits forever.
        let mut stop = json!({ "type": "content_block_stop", "index": 1 });
        assert_eq!(g.on_stream_event(&mut stop), StreamAction::Forward);

        // Nothing is left to run, so the turn ends.
        let mut md = json!({
            "type": "message_delta",
            "delta": { "stop_reason": "tool_use", "stop_sequence": null }
        });
        assert_eq!(g.on_stream_event(&mut md), StreamAction::Forward);
        assert_eq!(md["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn deltas_of_a_permitted_block_are_forwarded() {
        let mut g = guard(&["mcp__context7__*"]);

        let mut start = start_event(0, "mcp__everything__echo");
        assert_eq!(g.on_stream_event(&mut start), StreamAction::Forward);
        assert_eq!(
            start["content_block"]["type"], "tool_use",
            "must stay callable"
        );

        let mut delta = json!({
            "type": "content_block_delta", "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": "{}" }
        });
        assert_eq!(g.on_stream_event(&mut delta), StreamAction::Forward);

        let mut md = json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } });
        g.on_stream_event(&mut md);
        assert_eq!(
            md["delta"]["stop_reason"], "tool_use",
            "the allowed call must survive"
        );
    }

    #[test]
    fn a_mixed_turn_keeps_stop_reason_and_only_drops_the_refused_index() {
        let mut g = guard(&["mcp__context7__*"]);

        let mut allowed = start_event(0, "mcp__everything__echo");
        g.on_stream_event(&mut allowed);
        let mut refused = start_event(1, "mcp__context7__x");
        g.on_stream_event(&mut refused);

        let mut d0 = json!({ "type": "content_block_delta", "index": 0, "delta": {} });
        let mut d1 = json!({ "type": "content_block_delta", "index": 1, "delta": {} });
        assert_eq!(g.on_stream_event(&mut d0), StreamAction::Forward);
        assert_eq!(g.on_stream_event(&mut d1), StreamAction::Drop);

        let mut md = json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } });
        g.on_stream_event(&mut md);
        assert_eq!(md["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn text_blocks_and_unrelated_events_pass_through_untouched() {
        let mut g = guard(&["mcp__a__*"]);
        for mut ev in [
            json!({ "type": "message_start", "message": { "id": "m1" } }),
            json!({ "type": "content_block_start", "index": 0,
                    "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "ping" }),
            json!({ "type": "message_stop" }),
        ] {
            let before = ev.clone();
            assert_eq!(g.on_stream_event(&mut ev), StreamAction::Forward);
            assert_eq!(ev, before);
        }
        assert!(!g.acted());
    }

    #[test]
    fn a_repeated_name_is_decided_and_audited_once() {
        // Three calls to the same tool in one turn must all be refused, but they are one
        // policy decision and one audit row -- otherwise a model that retries in a loop
        // would flood the audit log.
        let mut g = guard(&["mcp__context7__*"]);
        for i in 0..3 {
            let mut ev = start_event(i, "mcp__context7__x");
            g.on_stream_event(&mut ev);
            assert!(g.suppressed.contains(&i), "every call is still suppressed");
        }
        assert_eq!(g.verdicts.len(), 1, "judged once");
        assert_eq!(g.blocked().len(), 1, "audited once");
        assert_eq!(g.tool_uses_seen, 3);
        assert!(g.all_calls_refused());
    }

    #[test]
    fn stop_reason_is_left_alone_when_no_call_was_refused() {
        let mut g = guard(&["mcp__nothing__*"]);
        let mut md = json!({ "type": "message_delta", "delta": { "stop_reason": "max_tokens" } });
        g.on_stream_event(&mut md);
        assert_eq!(md["delta"]["stop_reason"], "max_tokens");
    }
}
