## Summary

<!-- 1-3 bullets: what this PR does and why. The "why" matters more than the "what" — the diff already shows the what. -->

## New runtime I/O

<!--
List runtime I/O the change introduces. If none, write "None — pure in-process change". The CCAG release evidence gate requires this section: a runtime I/O the IaC/config doesn't grant is the bug class that motivated this template. v1.9.0 (PR #84) shipped a CDK IAM gap that made AIP overrides silently no-op for exactly this reason.

Cover at minimum:
- AWS API calls — action + resource scope (e.g. `bedrock:GetInferenceProfile` on `*`)
- Env vars — name, default, required/optional
- External network — HTTPS endpoints, queues, secrets the gateway will call out to
- Schema — new tables / columns / migrations
- Background loops or scheduled work — cadence, what it touches
- Dependencies — new crates, npm packages, system tools

If the doc adds a "Required permissions" entry but `infra/stack.ts` doesn't grant it (or vice versa), that's an automatic block.
-->

- [ ] None — pure in-process change. (Delete this checkbox if other items are added.)
- [ ] AWS API calls: ...
- [ ] Env vars: ...
- [ ] External network: ...
- [ ] Schema: ...
- [ ] Background loops: ...
- [ ] Dependencies: ...

## Test plan

<!-- Bulleted markdown checklist. Distinguish what was verified offline (mocks / unit / integration / CI) from what needs verification online (deployed environment with recorded evidence per `.claude/CLAUDE.md` → Release Evidence Gate). -->

**Offline (mocks / CI):**
- [ ] `make check` passes
- [ ] ...

**Online (deployed environment, evidence captured):**
- [ ] N/A — no `[online]` acceptance criteria. (Delete if there are any.)
- [ ] Spec `<spec-name>.md` criterion N: <verification command> → evidence captured at staging in `.claude/deploys/<sha>-evidence.md`
