---
id: ADR-027
title: Owner workflow closure, bounded L1 subagent fan-out, and Tachi long-horizon delegation
date: 2026-11-20
status: accepted
relates-to:
  - ADR-013
  - ADR-016
  - ADR-017
  - ADR-018
  - ADR-019
  - ADR-020
  - ADR-023
  - ADR-024
  - ADR-025
  - ADR-026
  - crates/reiclaw-api/src/sleep_records.rs
  - crates/reiclaw-api/src/subagent_v1.rs
  - crates/reiclaw-config/src/attention.rs
  - crates/reiclaw-gateway/src/api_attention.rs
  - crates/reiclaw-gateway/src/api_review.rs
  - crates/reiclaw-gateway/src/attention.rs
  - crates/reiclaw-gateway/src/delegation_notifications/mod.rs
  - crates/reiclaw-gateway/src/lib.rs
  - crates/reiclaw-infra/src/bridge_outbox.rs
  - crates/reiclaw-runtime/src/agent/loop_/
  - crates/reiclaw-runtime/src/agent/sleep_compactor/
  - crates/reiclaw-runtime/src/cron/
  - crates/reiclaw-runtime/src/subagent_v1/
  - crates/reiclaw-tools/src/mcp_deferred.rs
  - crates/reiclaw-tools/src/tachi_delegation.rs
  - crates/reiclaw-tools/src/tachi_staff.rs
---

# ADR-027: Owner Workflow Closure, Bounded L1 Subagent Fan-Out, and Tachi Long-Horizon Delegation

## Status

Proposed and accepted by the owner on 2026-11-20. Acceptance establishes the architectural boundaries, ADR amendments, and implementation slice sequence below; it does not by itself change runtime behavior, weaken any protected boundary in `AGENTS.md`, delete code, or reorder `#374`. Each implementation slice requires its own bounded issue, PR review, and validation evidence.

This record establishes the following relationships with prior accepted ADRs:

- **Amends [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) §3 (`L1` vs. `L2` work split):** expands in-body **L1** from a zero-tool single-shot reasoning call into a **bounded, turn-scoped parallel subagent pool** (`ToolOperation::Read` only, zero side effects, zero approvals, zero recursion, zero durable task rows, built over the existing confined `ToolLoop`), while focusing **L2 (`tachi_staff`)** on **long-horizon software engineering and durable/sandboxed external execution** (worktree-isolated coding, dual-track `Claude Code` + `Codex` design/review/implementation, test/sandbox gates, PR preparation, and cross-session runs).
- **Preserves** the single-body identity, owner-governed proposal/approval model, edge-Client/Node model, and `#197` Spawn-Wall safety guarantees in [ADR-013](./ADR-013-channels-as-gateway-clients.md), [ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md), [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md), [ADR-018](./ADR-018-soul-character-model.md), [ADR-019](./ADR-019-one-body-many-hands.md), and [ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md).
- **Closes last-mile runtime and learning seams** in [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) (live gateway activation of `GroundedContinuationResolver` and loopback SSE wake-ups, explicit operator-gated promotion of reviewed `DailyDigest` action candidates into bounded dispatches, and overnight `04:00` Auto Dream distillation of owner attention and candidate feedback into review-gated triage rules).
- **Enforces** the pure-Rust kernel scope and complexity budget in [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md): continuous monitoring workflows, feedback-driven attention calibration, and shared external productivity services attach through existing scheduling, outbox, review-inbox, and deferred MCP primitives without adding domain web applications, duplicate task ledgers, or in-process SaaS adapters to the kernel.

## Context

External "FounderOS" personal operating topologies and proactive self-learning companions such as [Linger](https://github.com/simplexai-labs/linger) highlight six daily owner workflows that belong together in practice:

1. low-friction capture across voice, quick notes, and messaging channels;
2. parallel everyday investigation, personal history synthesis, and multi-option comparison without polluting the main conversation window;
3. long-horizon engineering loops where one harness designs and reviews (`Claude Code`) while another implements and verifies (`Codex`) inside isolated sandboxes;
4. continuous 24-hour background monitoring of upstream repositories or third-party dependencies with human impact confirmation before code changes run;
5. overnight self-learning from how proactive notifications and action candidates actually landed with the owner, quietly calibrating push timing and relevance thresholds without model retraining;
6. least-privilege access to shared external services (GitHub, Google Drive, Gmail, Google Calendar, and knowledge bases).

Assembling those workflows as point-to-point SaaS integrations (for example, a standalone cloud voice bot writing diaries to Notion, a separate note app or Discord bot wired straight into Codex, and coding workers reading the same cloud workspace) fragments identity, leaks personal notes to coding workers, and lacks a canonical run ledger.

Conversely, auditing ReiClaw at `7a322a49f9` (after [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) Wave 1 and Wave 2 slices `#751` through `#771`) reveals six concrete architectural and wiring gaps inside our own single-body stack:

1. **Over-constrained L1 (`subagent_v1`) vs. heavy out-of-process L2 (`tachi_staff`).** Under the #197 Spawn Wall, `ReasoningSubagentTool` (`crates/reiclaw-runtime/src/subagent_v1/mod.rs`) was locked to zero tools (`NonEmptyToolPolicy`), a single LLM completion, and an `8 KiB` input cap. Meanwhile, `tachi_staff` launches heavy external CLI harnesses (`codex`, `claude`, `opencode`) that are strictly forbidden from reading in-body private stores (`sessions.db`, `memorycore.db`, personal MCPs) and incur cross-process setup and polling overhead. That left a critical unserved middle:
   - sweeping the owner's own multi-month transcripts, memories, or read-only personal MCPs forced the main agent to run 10+ tool calls directly inside the foreground conversation, bloating its context window;
   - fanning out 3-4 parallel web/documentation lookups or option comparisons inside a 10-second turn either bloated the main prompt or suffered awkward asynchronous CLI dispatch overhead in Tachi;
   - `crates/reiclaw-runtime/src/agent/sleep_compactor/mod.rs` (`run_confined_loop`) already had to build a confined in-process `ToolLoop` over `run_tool_call_loop_with_security` to read `sessions.db` and `memorycore.db`, demonstrating that a bounded read-only in-process subagent loop is already a core runtime need.
2. **Gateway proxy continuation and wake-up are implemented but unwired at startup.** `TachiStaffClient::continue_run` and `finish_run` (`crates/reiclaw-tools/src/tachi_staff.rs`), `ProxyContinuationGate` (`crates/reiclaw-tools/src/tachi_delegation.rs`), and `GroundedContinuationResolver` plus `DelegationNotificationTask::wake_handle` (`crates/reiclaw-gateway/src/delegation_notifications/mod.rs`) are implemented and unit-tested. However, `run_gateway` (`crates/reiclaw-gateway/src/lib.rs`) still calls `delegation_notifications::start(..., None, ...)`, leaving `resolver = None` and `wake = None` under `#[allow(dead_code)]`.
3. **Reviewing an `ActionCandidate` stops at status mutation.** `POST /api/review/action-candidates/{agent}/{day_key}/resolve` (`crates/reiclaw-gateway/src/api_review.rs`) persists `ActionCandidateStatus::Approved` to `memorycore.db` and returns `200 OK`, without offering an operator-gated promotion path into `TachiDelegation`, forcing the owner to re-enter approved engineering tasks manually in chat.
4. **24-hour impact monitoring lacks a standardized zero-bloat recipe.** ReiClaw already owns `cron/`, `heartbeat/`, `EpisodeRecord`, `DailyDigest`, and deduplicated `BridgeOutbox` delivery, while [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) forbids adding an embedded monitoring web app or new SQLite store inside the kernel.
5. **Owner attention and candidate outcomes are recorded but never distilled (`BridgeOutbox` and `DailyDigest` open feedback loop).** `BridgeOutbox` (`crates/reiclaw-infra/src/bridge_outbox.rs` and `crates/reiclaw-gateway/src/api_attention.rs`) already persists authenticated owner actions (`dismiss`, `snooze`, `mute`, `unmute`) keyed by `(bridge, recipient, source_kind, source_id)` across a 30-day or 10,000-row terminal receipt window, and `DailyDigest` (`crates/reiclaw-api/src/sleep_records.rs`) records `ActionCandidateStatus::Approved` vs. `Dismissed`. Yet the `04:00` Auto Dream worker (`crates/reiclaw-runtime/src/agent/sleep_compactor/`) ignores these outcome ledgers, leaving noisy sentinels or low-value candidate categories to repeat until the owner hand-edits `[gateway.attention]` (`crates/reiclaw-config/src/attention.rs`) or mutes each source manually.
6. **Shared external services coexist with unretired overlapping kernel builtins.** External productivity integrations fit deferred MCP and Tachi Vault (`crates/reiclaw-tools/src/mcp_deferred.rs`), whereas the kernel still carries overlapping builtin tools (`git_operations.rs`, `backup_tool.rs`, `data_management.rs`, `pipeline.rs`, `llm_task.rs`, `tunnel/`, and `api_skills.rs`) slated for retirement under [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) Waves 2 and 3.

## Decision

### 1. All capture surfaces terminate at the body; never wire clients directly to harnesses

Voice carriers ([ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md)), mobile quick-capture views, messaging bridges ([ADR-013](./ADR-013-channels-as-gateway-clients.md), [ADR-019](./ADR-019-one-body-many-hands.md)), and desktop workbenches ([ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)) attach exclusively to ReiClaw's gateway. No client, note surface, or messaging bot dispatches work directly to Codex, Claude Code, or Tachi behind the body's back:

- Fleeting thoughts, spoken notes, and asynchronous requests enter through `/ws/chat` (or operator-authenticated gateway intake) into `sessions.db` and `memorycore.db`.
- External cloud notes (such as Notion or Google Docs) may be read or updated as scoped external resources (§7), never used as ReiClaw's canonical memory or cross-agent session store.

### 2. Bounded L1 parallel subagent pool vs. L2 Tachi long-horizon delegation (amending ADR-017 §3)

[ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) §3 is amended so the main agent orchestrates everyday multi-step research and personal recall through **in-body L1 subagents**, reserving **Tachi (L2)** for **long-horizon software development and durable external execution**:

| Level | Who does the work | Permitted capabilities and lifecycle | Uses Tachi |
| --- | --- | --- | --- |
| **L0 Direct** | Main body's own turn: immediate chat, 1-3 quick lookups, Node intents, and owner-approved mutations | Full configured agent policy (`Read` and gated `Act`); owns Soul, User Model, and user wording | never |
| **L1 Subagent Pool** | Ephemeral in-process child runs (`1..=N` parallel tasks in the active turn): closed-book Advisor (`reasoning_subagent`) or bounded read-only Scout/Recall workers over the shared `ToolLoop` | **Strictly `ToolOperation::Read` subset** of the parent's admitted tools (`session_search`, `episode_read`, `memory_recall`, `web_fetch`, `web_search_tool`, admitted read-only MCPs) or zero-tool Advisor; foreground turn-scoped only, zero durable task rows | never |
| **L2 Long-Horizon Delegate** | External CLI harnesses (`Codex`, `Claude Code`, `OpenCode`) managed by **Tachi** through `tachi_staff` over MCP | Git worktrees, code edits, compiler/test/Docker sandboxes, dual-track `Claude Code` review + `Codex` implementation loops, PR preparation, and durable runs surviving client detach or restart | always |

To preserve every security guarantee of the `#197` Spawn Wall and [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) §5 ("one pipeline per concern, zero second ledger"), in-body L1 subagents enforce six hard invariants:

1. **Subagents research and draft; only the main body acts (`Read`-only confinement):** An L1 subagent receives only an explicit, policy-filtered `ToolOperation::Read` slice of the parent agent's already-admitted tools. Shell, process, file-write/edit, git, cron, memory-write/forget, messaging, and `tachi_*` tools are structurally excluded. When follow-up mutations or long-horizon coding are needed, the subagent returns a structured draft or specification in its report for the parent main agent to execute or delegate.
2. **Zero approval prompts from children:** Any tool that would require interactive approval (`ApprovalManager::needs_approval`) is excluded at child registry assembly and denied immediately if reached, preventing concurrent child runs from racing approval cards on owner surfaces.
3. **Turn-scoped concurrency, fate-sharing, and zero task tables:** The parent may fan out up to a bounded number of concurrent L1 tasks per turn (for example, `1..=4` via `tokio::task::JoinSet`), each subject to a strict iteration cap (default `6-8` tool rounds), token ceiling, and wall-clock timeout (default `<= 30s`), wired to a child token of the parent turn's `CancellationToken`. Aborting the foreground turn cancels all in-flight L1 children immediately. No SQLite task table, background queue, or detached coordinator is created; anything requiring detached durable execution belongs to L2 Tachi.
4. **No recursion (`depth == 0` root-only):** Retaining `LineageRef` (`SA-9` / `D1`), only a depth-0 parent turn may spawn L1 subagents; child tool registries exclude all subagent spawn tools.
5. **Scoped privacy and context hygiene:** Raw intermediate tool outputs stay inside the ephemeral child `ToolLoop` and are discarded at child exit; only bounded structured findings (`SubAgentReportV1` or compact digest) return to the parent transcript. Device-bound public voice turns ([ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md) A2) keep `exclude_memory = true`, so a child spawned from a public device turn inherits zero private memory or session-search tools. Raw `soul.db` bytes never cross into child prompts.
6. **Single loop implementation:** Read-only L1 scout/recall runs reuse `run_tool_call_loop_with_security` (extracting the shared confined-loop helper already proven in `crates/reiclaw-runtime/src/agent/sleep_compactor/mod.rs`) and `OpaqueModelBinding` (supporting fast-route models for parallel scouting and advisor models for closed-book synthesis) rather than introducing a second agent loop.

### 3. Wire `GroundedContinuationResolver` and loopback SSE wake-up in `run_gateway`

For L2 long-horizon engineering runs in Tachi, `crates/reiclaw-gateway/src/lib.rs` activates the [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) §8 observer loop in production when `[tachi]` is enabled:

1. **Per-agent resolver resolution without mutable global state:** Rather than binding `GroundedContinuationResolver` to a single boot-time seed model, the observer resolves the owning agent's configured fast or primary model route from the validated config snapshot on each `INPUT_REQUIRED` evaluation. If the owning agent has no valid model provider configured, resolution returns `Ok(None)` and escalates cleanly to the owner via `BridgeOutbox`.
2. **Preserved safety invariants:** Automatic `tachi_staff(action = "continue")` remains strictly subject to `ProxyContinuationGate`:
   - `AUTH_REQUIRED` states, permission prompts, spend decisions, and `closure_kind = "partial"` runs are never auto-answered;
   - `soul.db`, `user_model.db`, and protected identity markers remain physically excluded from `GroundedContinuationResolver`;
   - automatic continuations stay capped at `MAX_PROXY_CONTINUATIONS_PER_DISPATCH` (3) per `dispatch_id`, persisted in `sessions.db` so `SubmissionUnknown` increments the counter and is never blindly replayed.
3. **Loopback SSE wake-up nudges, never self-authenticating state:** When `tachi-server` emits MCP Streamable HTTP SSE task notifications (`kckylechen1/Tachi#2003`), a lightweight loopback listener calls `DelegationNotificationTask::notify()` to trigger an immediate sweep pass while keeping the 60-second interval timer as fallback. SSE payloads act solely as wake-up hints; `reconcile_with_deps` always calls `TachiStaffClient::status` against the route-verified endpoint before acting or notifying.

### 4. Explicit owner-promoted candidate dispatch (`Approve` vs. `Approve and Dispatch`)

To close the loop from idea capture through the review inbox into long-horizon Tachi execution without violating [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) §5 (background extraction never auto-executes):

1. **Status-only resolution stays pure:** `POST /api/review/action-candidates/{agent}/{day_key}/resolve` with `approved` or `dismissed` continues to mutate only `DailyDigest.action_decisions` in `memorycore.db`. Approving a note for the record never implicitly launches a subprocess or external job.
2. **Explicit operator-authenticated promotion endpoint:** A dedicated operator-gated action, `POST /api/review/action-candidates/{agent}/{day_key}/dispatch`, promotes a pending or approved candidate in `DailyDigest` into a bounded L2 Tachi run using existing owners:
   - **Required caller inputs:** canonical `candidate` text matching `DailyDigest.action_candidates`, caller-supplied idempotent `request_id` (`valid_request_id`), target `harness` alias admitted in `[tachi.harnesses]`, optional `issue_ref` / `pr_ref` / `flow_id`, and optional redacted `context_brief` (`<= 4,096` bytes).
   - **Single delegation pipeline:** The handler routes through `TachiDelegation` (`crates/reiclaw-tools/src/tachi_delegation.rs`) with `StaffingReason::ExplicitUserRequest`, enforcing the target agent's live `SecurityPolicy`, `tachi_admission::scan_text`, `prepare_outbound_task_with_brief` (blocking Soul/User Model markers and scrubbing secret patterns), and `SqliteSessionBackend` request claiming (`claim_delegation_request` / `bind_delegation_request`).
   - **Idempotent linkage on `DailyDigest`:** `DailyDigest` records an optional `action_dispatches: BTreeMap<String, ActionCandidateDispatch>` entry (`request_id`, `harness`, `promoted_at`, and last known binding state `bound` or `unresolved`, defaulting to empty via `#[serde(default)]` for full backward compatibility). Re-clicking dispatch with the same `(candidate, request_id)` returns the existing claim/receipt; supplying a different `request_id` for an already promoted candidate returns HTTP 409 (`candidate_already_dispatched`) unless the owner explicitly creates a separate chat/delegation request, preventing accidental double-starts after an unresolved submission.

### 5. Quiet-by-default background sentinels over `cron` and `BridgeOutbox`

Continuous 24-hour external monitoring (such as third-party dependency advisories, upstream repository releases, or breaking API changes) is realized as a **Sentinel Cron Pattern** with zero new kernel tables, daemons, or embedded web dashboards:

1. **Read-only sentinel risk profile:** Sentinel schedules run under a dedicated agent or cron `risk_profile` whose `allowed_tools` contains only bounded read tools (`web_fetch`, `web_search_tool`, L1 read-only subagent scouting, or explicit read-only `<server>__<tool>` MCP entries), with `mcp_discovered_tool_policy = "explicit_only"`, `always_ask = []`, and zero shell, file-write, git, or `tachi_start` tools.
2. **Quiet on healthy/no-impact checks:** Following [ADR-020](./ADR-020-durable-turns-replay-and-attention.md) §7, a sentinel pass that finds no actionable impact on the owner's pinned versions or active repositories records only normal cron run metadata and emits **zero** `BridgeOutbox` messages.
3. **Impact confirmation before remediation:** When a sentinel pass verifies a relevant advisory or breaking upstream change (citing concrete source URLs and affected local targets), it records a durable `EpisodeRecord` / pending action candidate and enqueues one deduplicated `BridgeOutbox` notification (subject to `#63` quiet-hours and attention rules). Code changes, dependency bumps, and sandbox test runs execute only after the owner promotes that candidate to Tachi (§4), and Tachi workers stop at PR creation (`AGENTS.md` landing rules) so human merge authority remains intact.

### 6. Overnight feedback-driven attention and candidate self-learning (`04:00` Auto Dream)

To eliminate repetitive sentinel noise and low-signal `ActionCandidate` clutter without model retraining or a new telemetry database, ReiClaw adapts the implicit/explicit proactive feedback mechanism demonstrated by [Linger](https://github.com/simplexai-labs/linger) as a first-party Rust loop inside the existing `04:00` local-time Auto Dream worker (`crates/reiclaw-runtime/src/agent/sleep_compactor/`), importing zero external code:

1. **Read-only distillation over existing outcome ledgers:** During the `04:00` Auto Dream pass, the confined sleep worker inspects three first-party evidence sources across the trailing 14-day window:
   - **Outbox attention receipts** in `BridgeOutbox` (`bridge_outbox.db`): operator-recorded `dismiss` resolutions (`delivery_state = 'dismissed'` with `owner_action_by`), repeated `snoozed_until` deferrals, and active `bridge_attention_mutes` vs. acknowledged items, grouped by `(bridge, recipient, source_kind, source_id)`;
   - **Candidate review decisions** in `DailyDigest` (`memorycore.db`): `action_decisions` (`ActionCandidateStatus::Dismissed` vs. `Approved`) and `action_dispatches` (§4) across recent `day_key` digests;
   - **Explicit owner chat directives** in authenticated `user`-role messages in `sessions.db` (for example, instructing the body to silence patch-level advisories on a repository, hold non-urgent notices until a specific hour, or surface a topic immediately), enforcing [ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md) §4 (`user`-role provenance only; assistant prose and external tool outputs never self-authorize rules).
2. **Two-speed candidate synthesis (implicit reinforcement vs. explicit instruction):**
   - **Implicit reinforcement (`>= 2` consistent owner pushbacks):** When the owner dismisses or snoozes the same non-critical `(source_kind, source_id)` or recurring `ActionCandidate` theme at least twice within 14 days without an intervening approval or engagement, Auto Dream synthesizes a concise **Attention / Triage Rule Candidate** citing the triggering outbox receipt or `day_key` IDs.
   - **Explicit instruction (`1` pass):** An explicit `user`-role timing or relevance directive stages the corresponding rule candidate in a single consolidation pass (or immediately during an L0 turn) without waiting for repeated dismissals.
3. **Owner-governed activation (`Propose` then `Confirm`, zero silent authority drift):** Preserving [ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md), [ADR-019](./ADR-019-one-body-many-hands.md) §1, and [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) §5:
   - Background sleep passes **never** rewrite `config.toml` (`[gateway.attention]`), mutate `soul.db`, promote active `user_model.db` heads without review, or widen tool/MCP grants.
   - Proposed rules surface in `GET /api/review/inbox` (`crates/reiclaw-gateway/src/api_review.rs`) via existing `UserModelStore` pending candidates (for durable candidate-triage preferences) and structured source-mute recommendations. Operator confirmation applies exact source suppressions through `BridgeOutbox::owner_action` (`bridge_attention_mutes`, inspectable and reversible via `POST /api/attention/{bridge}/{id}` with `unmute`) and activates bounded triage rules (`<= 16` active rules under a fixed byte cap) injected into future `sleep_compactor` candidate extraction and sentinel evaluation.
4. **Non-suppressible safety and governance floor:** Learned rules and mutes govern only discretionary proactive streams (`cron` sentinels, `daily_digest` summaries, and low-priority notices). Safety-critical and authority-bearing events remain structurally exempt from learned suppression:
   - interactive tool approval prompts (`always_ask`),
   - L2 `TASK_STATE_AUTH_REQUIRED` escalations and `SubmissionUnknown` / `delivery_state = 'unknown'` uncertain-effect reconciliations,
   - `estop`, security or sandbox boundary alerts, and [ADR-018](./ADR-018-soul-character-model.md) Soul drift threshold alerts.

### 7. Scoped shared services via deferred MCP and strict Read/Act separation

Shared external services (GitHub, Google Drive, Gmail, Google Calendar, Notion) attach outside the Rust kernel binary:

| Capability class | Execution path | Credential and policy boundary |
| --- | --- | --- |
| **Personal productivity reads** (calendar lookup, inbox search, Drive doc read) | Body L0 turn or L1 read-only subagent via deferred MCP (`mcp_deferred.rs`) | Credentials stored in Tachi Vault (`vault:<NAME>`), `mcp_discovered_tool_policy = "explicit_only"`, scoped per agent alias; never exposed to public/unverified voice device bridges ([ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md) A2). |
| **Personal productivity mutations** (send email, create/modify calendar invite, share/overwrite cloud file) | Body L0 turn only via deferred MCP behind `always_ask` | Requires explicit authenticated owner approval ([ADR-019](./ADR-019-one-body-many-hands.md) §5); never exposed to L1 subagents, background cron/sleep turns, or unverified voice. |
| **Long-horizon coding, sandbox verification, and PR creation** (GitHub branch/PR, Cargo/Docker test gates) | L2 Tachi delegation (`tachi_staff`) | Executed inside Tachi-managed worktrees and OS/Docker sandboxes under Tachi profile grants. Workers never receive personal Gmail/Calendar/Drive MCP credentials or Soul/User Model stores; workers never merge to `master`. |

In-process kernel tools that duplicate these external or Tachi-owned responsibilities (`git_operations`, `backup_tool`, `data_management`, `pipeline`, `llm_task`, `tunnel/`, and the in-process Skill IDE) are not extended to cover FounderOS workflows and continue retiring in bounded slices under [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) §4.

## Implementation Slices (`#374`-compatible)

Each slice lands as one independent or stacked PR under `AGENTS.md`:

1. **Slice 1: Live gateway wiring for `GroundedContinuationResolver`.** Construct a config-snapshot-backed `ContinuationResolver` in `crates/reiclaw-gateway/src/lib.rs`, pass it to `delegation_notifications::start_with_resolver`, remove `#[allow(dead_code)]` on `GroundedContinuationResolver::new`, and add gateway integration tests proving grounded auto-continuation, `INSUFFICIENT_FACTS` escalation, `AUTH_REQUIRED` refusal, and zero Soul/User Model leakage.
2. **Slice 2: Bounded L1 read-only subagent fan-out over shared confined `ToolLoop`.** Extract the shared confined `ToolLoop` runner from `crates/reiclaw-runtime/src/agent/sleep_compactor/mod.rs` and extend `subagent_v1` (and `docs/book/src/agents/delegation.md`) to support parallel (`1..=4`), turn-scoped `ToolOperation::Read` scout/recall runs alongside zero-tool Advisor mode, with discrimination tests enforcing `Read`-only filtering, zero approval prompts, `depth == 0` non-recursion, foreground `CancellationToken` propagation, device-bridge `exclude_memory` inheritance, and zero durable task rows.
3. **Slice 3: Operator-gated `ActionCandidate` dispatch promotion.** Add backward-compatible `ActionCandidateDispatch` metadata to `DailyDigest` in `crates/reiclaw-api/src/sleep_records.rs` and implement `POST /api/review/action-candidates/{agent}/{day_key}/dispatch` in `crates/reiclaw-gateway/src/api_review.rs` over `TachiDelegation`, with tests for operator-only auth, idempotent `request_id` replay, conflict refusal (`409`) on duplicate dispatch with a new ID, `SubmissionUnknown` preservation, and protected-marker rejection.
4. **Slice 4: Loopback SSE wake-up listener for `DelegationNotificationTask`.** Attach the MCP Streamable HTTP SSE task-status listener to `DelegationNotificationTask::wake_handle()` once pinned `tachi-server` notifications (`kckylechen1/Tachi#2003`) are available, removing the remaining `#[allow(dead_code)]` annotations while retaining 60-second fallback polling and canonical `status` verification.
5. **Slice 5: Overnight attention and candidate feedback distillation in `04:00` Auto Dream.** Extend `crates/reiclaw-runtime/src/agent/sleep_compactor/` with a read-only outcome summary over `BridgeOutbox` receipts and `DailyDigest.action_decisions`, staging bounded attention/triage rule candidates into `UserModelStore` / `GET /api/review/inbox` with operator confirmation into `bridge_attention_mutes` and hard non-suppression tests for `always_ask`, `AUTH_REQUIRED`, `SubmissionUnknown`, and `estop`.
6. **Slice 6: Quiet-by-default sentinel cron contract and [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) S7-S9 builtin retirement.** Validate the read-only sentinel `risk_profile` + `cron` + deduplicated `BridgeOutbox` pattern, and complete bounded caller-audited removals of `git_operations`, `backup_tool`, `data_management`, `pipeline`, `llm_task`, `tunnel/`, and in-process Skill authoring endpoints in favor of deferred MCP integrations and Tachi harnesses.

Rollback per slice: Slice 1 reverts `start_with_resolver` to `None`; Slice 2 gates read-only subagent profiles behind explicit tool/profile admission while preserving existing zero-tool `reasoning_subagent` behavior; Slice 3 uses `#[serde(default)]` additive metadata in `DailyDigest` and reuses existing `sessions.db` delegation bindings; Slice 4 falls back to the 60-second poll timer; Slice 5 disables outcome distillation in `sleep_compactor` while leaving existing `bridge_attention_mutes` and `UserModelStore` entries intact.

## Consequences

Positive:

- Gives the main agent natural, fast, parallel L1 hands for everyday research, multi-document reading, personal memory/MCP synthesis, and option comparison without bloating the main conversation window or paying cross-process CLI startup overhead.
- Keeps Tachi razor-focused on what it does best: long-horizon software engineering, worktree isolation, `Claude Code` + `Codex` cross-review pipelines, and restart-durable run truth.
- Preserves `#197` Spawn-Wall safety: L1 children cannot mutate state, prompt for approvals, message channels, recurse, or open a second task ledger.
- Activates already-tested [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) continuation code in `crates/reiclaw-gateway` and closes the inbox-to-Tachi promotion gap.
- Closes the proactive attention feedback loop during `04:00` Auto Dream over existing `BridgeOutbox` and `DailyDigest` receipts, steadily reducing sentinel and digest noise without model retraining, silent `config.toml` edits, or new SQLite stores.

Negative:

- Parallel L1 read-only subagent fan-out can multiply per-turn token consumption if unbounded; strict per-turn concurrency (`<= 4`), step ceilings (`6-8`), and per-child token/time meters are mandatory.
- Amending [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) §3 and updating `subagent_v1` requires updating `docs/book/src/agents/delegation.md` in Slice 2 so documentation and admission checks stay in lockstep.
- Promoting an `ActionCandidate` from the review UI requires the client to supply a target `harness` and stable `request_id`.
- Feedback-learned triage rules require explicit owner confirmation in `/api/review/inbox` and a hard active-rule cap (`<= 16`) so stale preferences neither silently suppress desired notices nor bloat sleep prompts.

## Acceptance

This ADR is implemented when:

1. the main agent can fan out `2-4` parallel turn-scoped L1 read-only subagents (over `session_search`, `memory_recall`, `web_fetch`, or admitted read-only MCPs) with `tachi-server` stopped, receiving structured digests while raw tool outputs stay out of the main transcript, and tests prove that write/approval-gated tools, recursion (`depth > 0`), public-voice private memory access, and detached execution are refused;
2. cancelling an active foreground turn immediately aborts all in-flight L1 subagents and leaves zero background tasks or database rows behind;
3. a live `rei gateway` with `[tachi]` enabled automatically answers a delegated L2 worker's non-partial `TASK_STATE_INPUT_REQUIRED` technical question using grounded `sessions.db` / `memorycore.db` facts via `GroundedContinuationResolver`, refuses `AUTH_REQUIRED` and Soul/User Model content, wakes immediately on loopback SSE notifications when available, and escalates `INSUFFICIENT_FACTS` to `BridgeOutbox`;
4. an operator promotes a `DailyDigest` `action_candidate` via `/api/review/action-candidates/{agent}/{day_key}/dispatch` into a bound L2 `TachiDelegation` claim (`StaffingReason::ExplicitUserRequest`), replaying identically on retry and refusing duplicate dispatch under a new `request_id` with HTTP 409;
5. repeated owner `dismiss` / `Dismissed` outcomes (`>= 2` within 14 days) on a discretionary sentinel or `DailyDigest` candidate theme, or a single explicit `user`-role chat directive, produce an inspectable attention/triage rule candidate in `04:00` Auto Dream without mutating `config.toml`, `soul.db`, or active `user_model.db` heads until operator approval, while `always_ask`, `AUTH_REQUIRED`, `SubmissionUnknown`, and `estop` notices remain structurally exempt from suppression;
6. a read-only sentinel `cron` job completes a no-impact check with zero `BridgeOutbox` rows, personal external services run under `explicit_only` deferred MCP with `always_ask` on L0 mutations, and retired overlapping builtins in [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) S7-S9 are removed with green CI gates.

## References

- [ADR-013: Channels as gateway clients](./ADR-013-channels-as-gateway-clients.md)
- [ADR-016: Growth proposed by the agent, approved by the owner](./ADR-016-growth-proposed-by-agent-approved-by-owner.md)
- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-018: Soul character model](./ADR-018-soul-character-model.md)
- [ADR-019: One body, many hands](./ADR-019-one-body-many-hands.md)
- [ADR-020: Durable turns, replay classes, background work, and attention](./ADR-020-durable-turns-replay-and-attention.md)
- [ADR-023: Realtime voice carrier with delegation to the body](./ADR-023-realtime-voice-carrier-with-body-delegation.md)
- [ADR-024: Bounded Tachi provider broker for inference](./ADR-024-bounded-tachi-provider-broker.md)
- [ADR-025: Long-context stack, sleep-time memory subagent, and delegation continuity](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md)
- [ADR-026: Headless pure-Rust kernel, external pluggable workbenches, and complexity budget](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)
- [Delegation and SubAgents](../../agents/delegation.md)
- [MCP loading and argument recovery](../../tools/mcp.md)
- [Fork Notes](../../contributing/fork-notes.md)
- External mechanism reference: [simplexai-labs/linger](https://github.com/simplexai-labs/linger) (overnight implicit/explicit proactive notification feedback loop)
- Issues `#63`, `#197`, `#374`, `#381`, `#388`; `kckylechen1/Tachi#2003`
