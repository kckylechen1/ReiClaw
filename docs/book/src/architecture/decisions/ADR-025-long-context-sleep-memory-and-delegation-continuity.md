---
id: ADR-025
title: Long-context stack, sleep-time memory subagent, and delegation continuity
date: 2026-11-18
status: accepted
relates-to:
  - ADR-005
  - ADR-010
  - ADR-014
  - ADR-015
  - ADR-016
  - ADR-017
  - ADR-019
  - ADR-020
  - ADR-024
  - ADR-027
  - crates/reiclaw-infra/src/session_sqlite.rs
  - crates/reiclaw-memory/src/memorycore.rs
  - crates/reiclaw-runtime/src/agent/turn/mod.rs
  - crates/reiclaw-runtime/src/agent/memory_inject.rs
  - crates/reiclaw-runtime/src/agent/soul_reflection.rs
  - crates/reiclaw-runtime/src/agent/sleep_compactor/
  - crates/reiclaw-tools/src/tachi_staff.rs
  - crates/reiclaw-tools/src/tachi_delegation.rs
  - crates/reiclaw-tools/src/mcp_era.rs
  - crates/reiclaw-tools/src/mcp_task.rs
  - crates/reiclaw-gateway/src/ws.rs
---

# ADR-025: Long-Context Stack, Sleep-Time Memory Subagent, and Delegation Continuity

## Status

Proposed and accepted by the owner on 2026-11-18. Acceptance records the architecture and its ownership boundaries; it does not by itself change runtime behavior, modify a protected contract, delete code, or reorder the `#374` execution queue. Each implementation slice below requires its own bounded issue, PR review, and validation evidence.

Amended on 2026-11-20 by [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md): the body owns the authorized workflow through acceptance and delivery; Tachi retains delegated execution truth. Sections 5 and 8 distinguish review from execution intent, confine continuation evidence to the delegated task, and require durable admission before an automatic continuation is sent. ADR-027 also extends the scheduled sleep pass with review-gated attention feedback. These decisions do not establish that the gateway resolver, SSE listener, candidate dispatch, or feedback pass is active in production.

This record resolves the open observation-storage question in [ADR-020](./ADR-020-durable-turns-replay-and-attention.md) ("Where do observations live: the existing session store, or memory with a session scope"): rolling session checkpoints and message-row watermarks belong to `sessions.db` (`SessionBackend`), while cross-session `Episode` records, durable facts, entity anchors, the pinned memory index, and daily digests belong to `memorycore.db` (`portable-kernel`).

The Slice 2/5 contract amends [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) §8 in two bounded respects:

1. `TachiStaffClient` supports the facade's `continue` and `finish` operations; those transport helpers now exist, while production continuation wiring and recovery acceptance remain under ADR-027.
2. Admitted Streamable HTTP SSE notifications wake the body for relevant Staff transitions, with canonical `status` verification and bounded polling fallback. Verify the installed facade's actual event contract; an upstream Tasks notification, protocol-version label, or closed issue alone does not establish Staff integration.

All protected boundaries in `AGENTS.md` remain unchanged: Soul and User Model authority ([ADR-014](./ADR-014-reviewed-agentsoul-projection.md), [ADR-015](./ADR-015-one-governed-soul.md), [ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md), [ADR-018](./ADR-018-soul-character-model.md)), local and delegated approvals, `/ws/chat`, and the Node path. Delegation availability and inference availability are separate capabilities; the staged availability contract in §1 applies.

## Context

This section records the adoption baseline on 2026-11-18, including historical source paths and then-open implementation gaps. It is not a current code inventory.

1. **Hard-drop trimming loses working continuity.** Current history management (`crates/zeroclaw-runtime/src/agent/turn/mod.rs` and [History management](../../agents/history-management.md)) enforces `max_history_messages = 50` and token budgets by dropping oldest whole turns and inserting a static breadcrumb. `SqliteSessionBackend` (`crates/zeroclaw-infra/src/session_sqlite.rs`) already retains append-only `sessions` rows with `message_source` provenance and `sessions_fts` FTS5 indexing, so trimmed messages survive on disk, but the active model loses the reasoning thread and has no scoped tool to inspect earlier turns or episodes.
2. **Single-turn consolidation is fragmented and bypassed on `/ws/chat`.** `crates/reiclaw-memory/src/consolidation.rs` runs an isolated LLM call after every turn on a 4,000-character truncated snippet without coordinating with `history_trim`. It spends one model call per turn while lacking multi-turn perspective to distinguish tentative exploration from settled decisions. In addition, `crates/zeroclaw-gateway/src/ws.rs` calls `consolidation::consolidate_turn` directly rather than `DefaultMemoryStrategy`, skipping the 12-hour `memorycore_enrichment` and `light_sleep_governance` passes.
3. **Turn-start recall is too narrow for terse follow-ups.** `crates/zeroclaw-runtime/src/agent/memory_inject.rs` queries at most 5 entries and renders at most 4 entries (4,000 characters) keyed solely on the current user utterance. Short instructions ("use the usual rule", "go with that direction") carry too little lexical or semantic signal to retrieve broader project state, while mutating prefix blocks every turn invalidates provider prompt caches.
4. **Controller sessions carry thoughts, not harness tool noise.** Unlike coding CLIs whose context windows fill with command retries, compiler logs, and file diffs, ZeroClaw delegates heavy execution to external harnesses through Tachi ([ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md)). ZeroClaw's own sessions consist predominantly of high-signal reasoning, trade-offs, rejected alternatives, and owner decisions ("thoughts"), punctuated by bounded L0 tool outputs and up-to-64 KiB `result.md` Staff reports. Compaction must therefore preserve reasoning trajectories (`why`) while dehydrating bulky reports, rather than scanning workspace source trees like a coding harness.
5. **Delegation lacks an idle wake-up wire and proxy continuation.** Tachi's `tachi_staff` contract already defines `continue` (`dispatch_id`, `expected_status_revision`, `task`) and `finish` at `awaiting_instruction` / `TASK_STATE_INPUT_REQUIRED` boundaries, and ZeroClaw already implements MCP `2026-07-28` `io.modelcontextprotocol/tasks` and in-turn `resultType = "input_required"` handles (`mcp_era.rs`, `mcp_task.rs`) within a 30-second poll wall. However, `crates/zeroclaw-tools/src/tachi_staff.rs` exposes only `start`, `status`, `result`, and `cancel`, and `ADR-017` §8 relies on 15-second polling (`kckylechen1/Tachi#2003`). When a delegated worker pauses on a question already settled in the owner's conversation or memory, the body cannot yet wake up, answer on the owner's behalf, and let the worker finish uninterrupted.
6. **Upstream and external comparison.** Upstream `zeroclaw-labs/zeroclaw` at `1c1cd118ac536a1dd1bc79b9cce69cbdc04e4f29` has no `portable-kernel` adapter, no double-buffered `session_checkpoints`, no background sleep-time memory subagent, and no `tachi_staff` continuation path. External industry mechanisms, specifically OpenAI Codex CLI `async_compaction` (background snapshot compaction with delta-turn swap), Anthropic Claude Code `Idle Compaction` and background memory consolidation ("Auto Dream"), and MCP `2026-07-28` server-to-client SSE task notifications, serve as architectural references only; no external runtime or harness code is imported.

## Decision

The context-stack layers called L0/L1/L2/L3 below are prompt layers, not the execution tiers in ADR-017/027. The scheduled sleep worker has its own narrowly admitted storage/governance operations; it is not a foreground read-only L1 child. Sharing the loop implementation does not transfer sleep write tools, background lifetime, or governance authority to that child pool.

### 1. Shared `portable-kernel` primitives, separate process-owned databases

Tachi's `portable-kernel` Rust library owns reusable storage primitives, including generic record, supersession, and time-window operations. ReiClaw owns its host-specific session, Episode, and digest projections and their source/authorization context. Adopt existing kernel primitives before adding shared types or requiring a pin bump; any missing primitive needs a demonstrated consumer gap. Runtime database files remain strictly process-owned ([ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md), [ADR-024](./ADR-024-bounded-tachi-provider-broker.md)):

- ZeroClaw exclusively opens and mutates `<install>/data/memory/memorycore.db` and `<install>/data/sessions/sessions.db`.
- `tachi-server` owns its separate execution and project databases; the two daemons never open or write the same SQLite file concurrently.
- Local context assembly, session checkpoints, and memory storage do not require Tachi's delegation service. Model-dependent L0/L1 turns and consolidation additionally require their admitted inference route: a retained direct route remains independent of delegation; after the sole-broker cutover in [ADR-024](./ADR-024-bounded-tachi-provider-broker.md) §12, broker loss returns typed `inference-unavailable`. Local state and authorized non-model operations remain available, without a hidden direct-provider fallback or replay of unknown inference outcomes.

### 2. Single source of truth across `sessions.db` and `memorycore.db`

No new SQLite database file is created. Compaction outputs split strictly by lifecycle and authority ([ADR-010](./ADR-010-memory-authority-boundaries.md)):

| Artifact | Canonical owner | Lifecycle and deletion semantics |
| --- | --- | --- |
| Raw transcript rows, `sessions_fts`, and double-buffered `session_checkpoints` (`session_key`, `up_to_message_id`, `staged` / `active` state, digest text) | `SqliteSessionBackend` (`data/sessions/sessions.db`) | Bound atomically to monotonic `sessions.id` row watermarks. Rehydrating a session reads the active checkpoint plus `sessions.id > up_to_message_id`. Deleting or clearing a session cascades to its checkpoints without deleting curated long-term memory. |
| Cross-session `Episode` records, durable facts/decisions (`Core`), entity anchors (`anchor:*`), pinned memory index (`PinnedDigest`), and daily digests (`DailyDigest`) | `MemoryCore` over `portable-kernel` (`data/memory/memorycore.db`) | Agent- and namespace-scoped durable memory. Survives session cleanup; governed by explicit retention, privacy erasure, and `supersedes` revision chains. |
| Owner values, goals, preferences, habits, constraints (`user_model.db`) and agent persona (`soul.db`) | Existing Companion stores (`UserModelStore`, `SoulProfileStore`) | Append-only, owner-governed stores; never merged into `memorycore.db`. |

### 3. Cache-preserving four-layer context stack and asymmetric dehydration

Foreground prompt assembly replaces drop-on-overflow history trimming with four ordered layers designed to preserve provider KV caches and thought continuity:

1. **L0 Static Anchor Prefix (cache-pinned):** System prompt, governed Soul projection, and a compact **Pinned Memory Index** (`1,000-2,000` tokens summarizing active projects, settled decisions, and key entity anchors). This prefix remains byte-stable across an active prompt-cache warm window; background index updates activate only on a cold cache boundary or new session.
2. **L1 Rolling Session Checkpoint:** Structured working summary covering `sessions.id <= up_to_message_id`, preserving goals, settled decisions, **reasoning trade-offs (`why`), and rejected alternatives** rather than reducing discussion to bare action bullets.
3. **L2 Active Window with Asymmetric Payload Dehydration:** Complete recent turns (`sessions.id > up_to_message_id`) plus high-confidence turn-tail `[Memory context]` supplements attached at the tail so prefix caching is undisturbed. Because conversational thoughts have high signal density, pure user/assistant dialogue is retained across long spans, whereas stale bulky payloads (delegated `result.md` reports and large `web_fetch` / tool outputs older than `keep_tool_context_turns`) are dehydrated at cold-cache boundaries into compact (~300-word) outcome digests carrying an exact transcript/episode reference handle.
4. **L3 Lossless Drill-Down Tools:** Canonical messages and full tool/delegation reports remain intact in `sessions.db` and `memorycore.db` (until explicit owner deletion or retention expiry). The foreground model receives agent- and session-admissible read-only tools (`session_search` over `sessions_fts` and `episode_read`) to inspect exact historical passages or full reports on demand.

### 4. Bounded background compaction and sleep-time subagent

Per-turn `consolidation::consolidate_turn` and `ws_consolidation_model` are replaced (after end-to-end parity) by an asynchronous, isolated background subagent built on `zeroclaw-runtime`'s tool-call loop (`run_tool_call_loop` with a dedicated `ScopedToolSet`):

- **Capability confinement:** The background subagent holds only read-only session/memory search tools (`session_search`, `memory_recall`) and structured commit primitives (`checkpoint_stage`, `episode_commit`, `memory_supersede`, `digest_publish`, `action_propose`). It has no shell, network, delegation, or filesystem write tools, and it does not scan workspace source repositories (codebase state belongs to Tachi harnesses). Tool outputs and third-party fetched text inside transcripts remain fenced as untrusted data.
- **Trigger 1: Double-buffered watermark pre-compaction (~65% context budget).** When active context crosses the configured soft watermark, the subagent snapshots the oldest stable turn prefix up to `target_msg_id` while foreground turns continue appending unobstructed (`id > target_msg_id`, the delta log). The subagent stages a new L1 checkpoint in `sessions.db` and commits a provenance-tagged `Episode` to `memorycore.db`. The staged checkpoint swaps in atomically on the next expired 300-second prompt-cache window, or immediately if context hits the hard ceiling (~90%).
- **Trigger 2: Idle session flush and startup reconciliation.** When a session with unflushed turns (`id > last_episode_msg_id`) sits idle past the quiet window, or on daemon startup recovery, the subagent packages those turns into an `Episode` and runs `portable-kernel` light-sleep governance (enrichment and scoped near-duplicate collapse), closing the current `/ws/chat` governance gap without per-turn LLM calls.

### 5. Three-tier authority split for memory, protected stores, and actions

Background extraction enforces three distinct authority tiers:

1. **Automatic durable knowledge (`memorycore.db`):** `Episode` records, objective project/technical facts, explicit owner decisions, and entity anchors are committed automatically by the background subagent. Contradicted or outdated entries are linked via atomic `supersede_entry(old_id, new_entry)` chains rather than silently overwritten or left conflicting.
2. **Owner-approved identity and profile (`user_model.db`, `soul.db`):** Inferred owner values, goals, preferences, habits, or constraints, and any Soul Tone/Story changes, never auto-activate. `crates/reiclaw-runtime/src/agent/soul_reflection.rs` remains separate and retains authenticated original-owner source checks ([ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md) §4). A `user` role label alone does not establish owner authorship, and tool or delegation text cannot become an owner directive by being quoted or summarized.
3. **Inbox-gated action candidates:** Follow-up todos, reminders, or proposed Tachi delegations extracted from conversations or digests are recorded only as non-executing review candidates. Marking a candidate approved or dismissed records a review decision; it does not schedule a reminder or start a delegated run. Execution requires a distinct authenticated execution action through the existing authorization and dispatch path, as specified in [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md) §4. An active owner-authorized workflow can continue within its established scope without asking again; background extraction or candidate status cannot create that authority.

### 6. Daily sleep compilation and quiet Daily Digest

The design calls for one cross-session deep-sleep pass ("Auto Dream") per owner-local day, with `04:00` as the initial day-boundary default and catch-up after host sleep. The schedule must state its timezone, day-window identity, and fallback explicitly; a host-local clock does not establish the owner's timezone. The `04:00` label is not evidence that a deployment has a configurable cutoff, fired on time, or completed the pass. DST transitions and catch-up must reuse the same logical day window rather than duplicate a completed pass.

The pass:

- Reads the prior day's `Episode` stream and recent `Core` / entity records from `memorycore.db`;
- Reconciles cross-session contradictions via `supersedes`, archives stale transient notes, and publishes the refreshed **Pinned Memory Index** (`PinnedDigest`);
- Writes a structured **Daily Digest** (`DailyDigest`) covering settled decisions, key thread progression, cross-day latent patterns or contradictions between stated goals and recent work, and pending action candidates;
- Exposes the Daily Digest quietly in the authenticated Web and phone views by default, creating a proactive notification candidate only when high-priority items qualify under `#63` quiet-hours and attention policy.

### 7. Split fast and flagship model routing

Background memory work separates high-frequency mechanical passes from daily synthesis:

- **Fast route (watermark compaction, report dehydration, idle flush, light-sleep enrichment):** Supports an optional dedicated fast or local model-provider route in configuration; falls back to the agent's configured primary model when unset.
- **Primary flagship route (scheduled daily sleep compilation):** Defaults to the agent's primary reasoning model so cross-day contradiction detection and latent insight extraction retain full reasoning depth.

### 8. Delegation continuity: redacted context, MCP SSE wake-up, and proxy `continue`

ReiClaw owns the owner's goal, authorized next steps, acceptance, and delivery. Tachi owns the execution and collaboration of admitted external workers, including durable, sandboxed, and cross-host work; this division is not limited to software engineering. Existing transport and authentication limits remain in force until their replacements pass acceptance. The two systems close the L2 loop without breaking `TachiStaffParams` wire compatibility or weakening authority boundaries:

1. **Outbound redacted brief:** When starting or continuing a Staff run, the body may supply a compact `[Task Context]` derived only from the delegation-bound originating session and explicitly admitted project facts, with source references. Sharing an agent alias does not authorize reading or exporting every conversation of that agent. Raw Soul bytes, User Model records, and unrelated private conversations are never included; the continuation resolver does not open protected stores ([ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) §8). An owner constraint already admitted to the task may guide its execution without exporting the underlying profile record.
2. **MCP SSE wake-up doorbell:** Once the installed `tachi-server` facade is verified to emit the relevant Staff status notifications over Streamable HTTP SSE, ZeroClaw connects a lightweight loopback listener to the existing observer alongside bounded fallback polling. Adoption of `kckylechen1/Tachi#2003` must identify the actual notification surface and paired acceptance; use the admitted transport rather than adding a parallel delegation client. A notification carries only run identity and revision hints, never self-authenticating instructions: ZeroClaw always calls `tachi_staff(status)` to verify canonical state and treats worker text strictly as untrusted subordinate evidence, never as an `owner` turn on `/ws/chat`.
3. **Chief-of-Staff proxy continuation (`action = "continue"`):** When verified canonical status is `TASK_STATE_INPUT_REQUIRED` (non-partial `awaiting_instruction`):
   - **Continue within established authority:** Using task-scoped evidence and existing authorization, the body can answer settled questions, correct a worker's approach, arrange CI repair or independent acceptance checks, and advance agreed delivery steps through the same Tachi execution path. It need not return ordinary implementation choices to the owner. A `continue` quotes the canonical `expected_status_revision` and produces an audit record for the final report.
   - **Escalate unresolved decisions:** The body first gathers available evidence within the admitted scope. If a necessary owner decision remains absent, ambiguous, or conflicting, it presents concise options and waits; it does not invent an owner preference or widen the task to resolve the ambiguity.
   - **Hard safety and loop floors:** New scope or privilege, unresolved approval requirements, new spend or budget increases, and `AUTH_REQUIRED` states require the owner's decision. Destructive actions and external communications remain subject to their existing grants and approval policy; a technical proxy answer cannot supply a missing grant. Existing task authorization never bypasses `always_ask` or renews an expired or consumed grant. Automatic proxy continuations per `dispatch_id` have a strict bounded budget; exceeding it stops automatic replies and surfaces the blocker.
   - **Persist before sending:** The existing `sessions.db` delegation owner atomically reserves the continuation budget and records the pending `(dispatch_id, expected_status_revision)` attempt before transmission. A failed durable claim prevents sending. A crash or uncertain response retains the consumed reservation and an unresolved outcome for reconciliation; it never silently permits replay or a new revision to bypass the cap. Only proof that no request was sent can release that reservation. Tachi's revision check remains an additional guard, not a substitute for local recovery state.
4. **Report acceptance and digestion:** A supervised run may be closed with `finish` only at a supported `INPUT_REQUIRED` boundary, under the caller's existing authorization and with `expected_status_revision` from canonical `status`. This state distinction grants no new automatic-finish authority; approval and unknown-outcome constraints remain in force. After canonical status becomes terminal `TASK_STATE_COMPLETED`, the body fetches `result.md`. A worker's completion receipt is execution evidence, not proof that the owner's acceptance criteria are met. The body checks those criteria, arranges bounded correction or independent verification within existing authority when needed, and reports the actual result and delivery state. Background digestion retains provenance and the distinction between execution and acceptance while extracting durable findings into `memorycore.db` and dehydrating bulky report bytes. No second worker lifecycle or execution ledger is introduced.

## Consequences

Positive:

- Multi-day conversations keep coherent reasoning context (`why` and rejected options) without hitting 50-message amnesia or blocking foreground turns on synchronous compaction.
- Eliminating per-turn `consolidate_turn` removes one LLM call per chat message while closing the `/ws/chat` governance bypass.
- Prefix-pinned memory indexing and cold-window checkpoint swaps preserve provider prompt-cache hit rates.
- Delegated Tachi runs pause less often on questions the owner already answered in chat, while retaining strict fail-closed approval and anti-loop bounds.

Negative:

- Cross-repository work is required only for a demonstrated missing primitive or notification contract. Existing kernel adoption, host DTO ownership, and paired SSE acceptance must be reconciled separately; do not make every host slice wait for an unnecessary kernel pin bump.
- Double-buffered checkpoints, watermark tracking, and SSE wake-up handling add state transitions in `sessions.db` and `zeroclaw-runtime` that require crash, session-delete, and stale-revision tests.
- Poorly tuned fast models could over-compress subtle reasoning if checkpoint prompts are not verified against multi-turn decision fixtures.

## Implementation slices

Each slice is one bounded issue/PR under `#374` following `AGENTS.md` replace-before-delete rules:

1. **Slice 1: Architecture baseline (`ADR-025`).** Record this ADR in `docs/book` and link the follow-up slice sequence to `#374`.
2. **Slice 2: Kernel contract adoption and Tachi notification compatibility.** Map host-owned Episode/digest projections to the pinned kernel's generic records, immutable supersession, and time-window APIs. Add a primitive or bump the pin only for a proven gap. Separately verify the installed Staff notification surface and live wake-up contract; a Tasks/Staff upstream delivery does not close ReiClaw integration acceptance.
3. **Slice 3: `sessions.db` checkpoints, four-layer context stack, asymmetric dehydration, and drill-down tools.** Add `session_checkpoints` to `SqliteSessionBackend` with cascade-on-delete; wire L0 pinned index, L1 checkpoint, cold-window report dehydration, tail-attached high-confidence recall, and scoped `session_search` / `episode_read` tools.
4. **Slice 4: Background compaction, scheduled daily sleep, Daily Digest UI, and `consolidate_turn` retirement.** Implement the confined sleep-time subagent with fast/flagship routing, 65% watermark snapshot-plus-delta swap, idle/startup Episode flush, the timezone-explicit daily window in §6, and Web/phone Daily Digest + candidate review views; retire per-turn `consolidation.rs` and `ws_consolidation_model` once end-to-end parity tests pass.
5. **Slice 5: `tachi_staff` `continue`/`finish`, SSE wake-up listener, and bounded proxy continuation.** Wire `continue` and `finish` in `TachiStaffClient`, attach the loopback SSE wake-up listener with canonical `status` verification, enforce task-scoped evidence, `AUTH_REQUIRED` refusal, and durable before-send continuation admission, and route completed `result.md` reports into background Episode digestion. The remaining production activation and recovery work follows ADR-027; transport helpers and unit tests alone do not establish live acceptance.

Rollback per slice: Slice 1 is docs-only (`git revert`). Slices 3-5 gate new runtime paths behind config/feature defaults until verified, retain append-only `sessions` rows so disabling checkpoints falls back to raw recent turns, and disable `[tachi]` SSE/proxy continuation cleanly back to explicit status reads.

## Acceptance

This ADR is implemented when:

1. a session exceeding 100 turns retains L0 prefix cache stability across warm turns, stages a background L1 checkpoint past the 65% watermark without blocking foreground replies, swaps atomically on a cold window, and retrieves pre-checkpoint verbatim text via `session_search` while denying cross-session/cross-agent reads;
2. deleting a gateway session removes its `sessions`, `sessions_fts`, and `session_checkpoints` rows while leaving curated `memorycore.db` memories intact;
3. short idle sessions flush unflushed turns into `memorycore.db` `Episode` rows and run light-sleep governance on `/ws/chat` with zero per-turn LLM calls, and legacy per-turn `consolidate_turn` is deleted with its exclusive dead code;
4. the scheduled sleep pass identifies its timezone and logical day window, catches up after host sleep without duplicating a completed window, supersedes contradicted facts, refreshes the `PinnedDigest`, and writes a `DailyDigest` visible in the owner UI; `soul_reflection` retains authenticated owner-source provenance, candidate review alone causes no execution, and protected stores never activate unreviewed changes;
5. a delegated Tachi run entering `TASK_STATE_INPUT_REQUIRED` wakes the body through the admitted notification path, verifies canonical `status`, and continues within existing authority using task-scoped facts; tests reject unrelated same-agent sessions, new authority, and `AUTH_REQUIRED`, preserve the continuation cap and unknown outcomes across crash/write-failure boundaries, and keep worker completion distinct from body acceptance and result delivery.

## References

- [ADR-005: Pluggable memory backends](./ADR-005-pluggable-memory-backends.md)
- [ADR-010: Separate conversation history, curated memory, and enrichment authority](./ADR-010-memory-authority-boundaries.md)
- [ADR-016: Growth proposed by the agent and approved by the owner](./ADR-016-growth-proposed-by-agent-approved-by-owner.md)
- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-019: One body, many hands](./ADR-019-one-body-many-hands.md)
- [ADR-020: Durable turns, replay classes, background work, and attention](./ADR-020-durable-turns-replay-and-attention.md)
- [ADR-024: Bounded Tachi provider broker for inference](./ADR-024-bounded-tachi-provider-broker.md)
- [ADR-027: Owner workflow closure and scoped services](./ADR-027-owner-workflow-closure-and-scoped-services.md)
- [History management](../../agents/history-management.md)
- [Memory and payload lifecycle](../memory-payload-lifecycle.md)
- [Runtime state and persistence](../runtime-state-and-persistence.md)
- Issues `#63`, `#374`, `#380`, `#381`, `#382`, `#388`; `kckylechen1/Tachi#2003`
