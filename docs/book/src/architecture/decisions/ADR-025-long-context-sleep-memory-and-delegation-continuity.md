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
  - crates/zeroclaw-infra/src/session_sqlite.rs
  - crates/zeroclaw-memory/src/tachi.rs
  - crates/zeroclaw-memory/src/consolidation.rs
  - crates/zeroclaw-runtime/src/agent/turn/mod.rs
  - crates/zeroclaw-runtime/src/agent/memory_inject.rs
  - crates/zeroclaw-runtime/src/agent/soul_reflection.rs
  - crates/zeroclaw-tools/src/tachi_staff.rs
  - crates/zeroclaw-tools/src/tachi_delegation.rs
  - crates/zeroclaw-tools/src/mcp_era.rs
  - crates/zeroclaw-tools/src/mcp_task.rs
  - crates/zeroclaw-gateway/src/ws.rs
---

# ADR-025: Long-Context Stack, Sleep-Time Memory Subagent, and Delegation Continuity

## Status

Proposed and accepted by the owner on 2026-11-18. Acceptance records the architecture and its ownership boundaries; it does not by itself change runtime behavior, modify a protected contract, delete code, or reorder the `#374` execution queue. Each implementation slice below requires its own bounded issue, PR review, and validation evidence.

This record resolves the open observation-storage question in [ADR-020](./ADR-020-durable-turns-replay-and-attention.md) ("Where do observations live: the existing session store, or memory with a session scope"): rolling session checkpoints and message-row watermarks belong to `sessions.db` (`SessionBackend`), while cross-session `Episode` records, durable facts, entity anchors, the pinned memory index, and daily digests belong to `tachi.db` (`portable-kernel`).

When Slices 2 and 5 land, this record amends [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) §8 in two bounded respects:

1. `TachiStaffClient` adds the `continue` and `finish` operations already defined on Tachi's `tachi_staff` facade;
2. `tachi-server` task status notifications over Streamable HTTP SSE (`rmcp` / MCP `2026-07-28`) wake the body on `TASK_STATE_INPUT_REQUIRED` and terminal transitions, retaining bounded `status` polling as fallback and requiring canonical `status` verification before acting.

All protected boundaries in `AGENTS.md` remain unchanged: Soul and User Model authority ([ADR-014](./ADR-014-reviewed-agentsoul-projection.md), [ADR-015](./ADR-015-one-governed-soul.md), [ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md), [ADR-018](./ADR-018-soul-character-model.md)), local and delegated approvals, `/ws/chat`, the Node path, and L0/L1 availability when `tachi-server` is stopped.

## Context

1. **Hard-drop trimming loses working continuity.** Current history management (`crates/zeroclaw-runtime/src/agent/turn/mod.rs` and [History management](../../agents/history-management.md)) enforces `max_history_messages = 50` and token budgets by dropping oldest whole turns and inserting a static breadcrumb. `SqliteSessionBackend` (`crates/zeroclaw-infra/src/session_sqlite.rs`) already retains append-only `sessions` rows with `message_source` provenance and `sessions_fts` FTS5 indexing, so trimmed messages survive on disk, but the active model loses the reasoning thread and has no scoped tool to inspect earlier turns or episodes.
2. **Single-turn consolidation is fragmented and bypassed on `/ws/chat`.** `crates/zeroclaw-memory/src/consolidation.rs` runs an isolated LLM call after every turn on a 4,000-character truncated snippet without coordinating with `history_trim`. It spends one model call per turn while lacking multi-turn perspective to distinguish tentative exploration from settled decisions. In addition, `crates/zeroclaw-gateway/src/ws.rs` calls `consolidation::consolidate_turn` directly rather than `DefaultMemoryStrategy`, skipping the 12-hour `tachi_enrichment` and `light_sleep_governance` passes.
3. **Turn-start recall is too narrow for terse follow-ups.** `crates/zeroclaw-runtime/src/agent/memory_inject.rs` queries at most 5 entries and renders at most 4 entries (4,000 characters) keyed solely on the current user utterance. Short instructions ("use the usual rule", "go with that direction") carry too little lexical or semantic signal to retrieve broader project state, while mutating prefix blocks every turn invalidates provider prompt caches.
4. **Controller sessions carry thoughts, not harness tool noise.** Unlike coding CLIs whose context windows fill with command retries, compiler logs, and file diffs, ZeroClaw delegates heavy execution to external harnesses through Tachi ([ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md)). ZeroClaw's own sessions consist predominantly of high-signal reasoning, trade-offs, rejected alternatives, and owner decisions ("thoughts"), punctuated by bounded L0 tool outputs and up-to-64 KiB `result.md` Staff reports. Compaction must therefore preserve reasoning trajectories (`why`) while dehydrating bulky reports, rather than scanning workspace source trees like a coding harness.
5. **Delegation lacks an idle wake-up wire and proxy continuation.** Tachi's `tachi_staff` contract already defines `continue` (`dispatch_id`, `expected_status_revision`, `task`) and `finish` at `awaiting_instruction` / `TASK_STATE_INPUT_REQUIRED` boundaries, and ZeroClaw already implements MCP `2026-07-28` `io.modelcontextprotocol/tasks` and in-turn `resultType = "input_required"` handles (`mcp_era.rs`, `mcp_task.rs`) within a 30-second poll wall. However, `crates/zeroclaw-tools/src/tachi_staff.rs` exposes only `start`, `status`, `result`, and `cancel`, and `ADR-017` §8 relies on 15-second polling (`kckylechen1/Tachi#2003`). When a delegated worker pauses on a question already settled in the owner's conversation or memory, the body cannot yet wake up, answer on the owner's behalf, and let the worker finish uninterrupted.
6. **Upstream and external comparison.** Upstream `zeroclaw-labs/zeroclaw` at `1c1cd118ac536a1dd1bc79b9cce69cbdc04e4f29` has no `portable-kernel` adapter, no double-buffered `session_checkpoints`, no background sleep-time memory subagent, and no `tachi_staff` continuation path. External industry mechanisms, specifically OpenAI Codex CLI `async_compaction` (background snapshot compaction with delta-turn swap), Anthropic Claude Code `Idle Compaction` and background memory consolidation ("Auto Dream"), and MCP `2026-07-28` server-to-client SSE task notifications, serve as architectural references only; no external runtime or harness code is imported.

## Decision

### 1. Shared `portable-kernel` primitives, separate process-owned databases

Shared memory record schemas, supersession transactions, and sleep-governance operators are implemented in Tachi's `portable-kernel` Rust library so ZeroClaw and Tachi share one storage engine implementation. Runtime database files remain strictly process-owned ([ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md), [ADR-024](./ADR-024-bounded-tachi-provider-broker.md)):

- ZeroClaw exclusively opens and mutates `<install>/data/memory/tachi.db` and `<install>/data/sessions/sessions.db`.
- `tachi-server` owns its separate execution and project databases; the two daemons never open or write the same SQLite file concurrently.
- Stopping `tachi-server` leaves L0/L1 turns, local four-layer context assembly, session checkpoints, and local memory recall/consolidation working.

### 2. Single source of truth across `sessions.db` and `tachi.db`

No new SQLite database file is created. Compaction outputs split strictly by lifecycle and authority ([ADR-010](./ADR-010-memory-authority-boundaries.md)):

| Artifact | Canonical owner | Lifecycle and deletion semantics |
| --- | --- | --- |
| Raw transcript rows, `sessions_fts`, and double-buffered `session_checkpoints` (`session_key`, `up_to_message_id`, `staged` / `active` state, digest text) | `SqliteSessionBackend` (`data/sessions/sessions.db`) | Bound atomically to monotonic `sessions.id` row watermarks. Rehydrating a session reads the active checkpoint plus `sessions.id > up_to_message_id`. Deleting or clearing a session cascades to its checkpoints without deleting curated long-term memory. |
| Cross-session `Episode` records, durable facts/decisions (`Core`), entity anchors (`anchor:*`), pinned memory index (`PinnedDigest`), and daily digests (`DailyDigest`) | `TachiMemory` over `portable-kernel` (`data/memory/tachi.db`) | Agent- and namespace-scoped durable memory. Survives session cleanup; governed by explicit retention, privacy erasure, and `supersedes` revision chains. |
| Owner values, goals, preferences, habits, constraints (`user_model.db`) and agent persona (`soul.db`) | Existing Companion stores (`UserModelStore`, `SoulProfileStore`) | Append-only, owner-governed stores; never merged into `tachi.db`. |

### 3. Cache-preserving four-layer context stack and asymmetric dehydration

Foreground prompt assembly replaces drop-on-overflow history trimming with four ordered layers designed to preserve provider KV caches and thought continuity:

1. **L0 Static Anchor Prefix (cache-pinned):** System prompt, governed Soul projection, and a compact **Pinned Memory Index** (`1,000-2,000` tokens summarizing active projects, settled decisions, and key entity anchors). This prefix remains byte-stable across an active prompt-cache warm window; background index updates activate only on a cold cache boundary or new session.
2. **L1 Rolling Session Checkpoint:** Structured working summary covering `sessions.id <= up_to_message_id`, preserving goals, settled decisions, **reasoning trade-offs (`why`), and rejected alternatives** rather than reducing discussion to bare action bullets.
3. **L2 Active Window with Asymmetric Payload Dehydration:** Complete recent turns (`sessions.id > up_to_message_id`) plus high-confidence turn-tail `[Memory context]` supplements attached at the tail so prefix caching is undisturbed. Because conversational thoughts have high signal density, pure user/assistant dialogue is retained across long spans, whereas stale bulky payloads (delegated `result.md` reports and large `web_fetch` / tool outputs older than `keep_tool_context_turns`) are dehydrated at cold-cache boundaries into compact (~300-word) outcome digests carrying an exact transcript/episode reference handle.
4. **L3 Lossless Drill-Down Tools:** Canonical messages and full tool/delegation reports remain intact in `sessions.db` and `tachi.db` (until explicit owner deletion or retention expiry). The foreground model receives agent- and session-admissible read-only tools (`session_search` over `sessions_fts` and `episode_read`) to inspect exact historical passages or full reports on demand.

### 4. Bounded background compaction and sleep-time subagent

Per-turn `consolidation::consolidate_turn` and `ws_consolidation_model` are replaced (after end-to-end parity) by an asynchronous, isolated background subagent built on `zeroclaw-runtime`'s tool-call loop (`run_tool_call_loop` with a dedicated `ScopedToolSet`):

- **Capability confinement:** The background subagent holds only read-only session/memory search tools (`session_search`, `memory_recall`) and structured commit primitives (`checkpoint_stage`, `episode_commit`, `memory_supersede`, `digest_publish`, `action_propose`). It has no shell, network, delegation, or filesystem write tools, and it does not scan workspace source repositories (codebase state belongs to Tachi harnesses). Tool outputs and third-party fetched text inside transcripts remain fenced as untrusted data.
- **Trigger 1: Double-buffered watermark pre-compaction (~65% context budget).** When active context crosses the configured soft watermark, the subagent snapshots the oldest stable turn prefix up to `target_msg_id` while foreground turns continue appending unobstructed (`id > target_msg_id`, the delta log). The subagent stages a new L1 checkpoint in `sessions.db` and commits a provenance-tagged `Episode` to `tachi.db`. The staged checkpoint swaps in atomically on the next expired 300-second prompt-cache window, or immediately if context hits the hard ceiling (~90%).
- **Trigger 2: Idle session flush and startup reconciliation.** When a session with unflushed turns (`id > last_episode_msg_id`) sits idle past the quiet window, or on daemon startup recovery, the subagent packages those turns into an `Episode` and runs `portable-kernel` light-sleep governance (enrichment and scoped near-duplicate collapse), closing the current `/ws/chat` governance gap without per-turn LLM calls.

### 5. Three-tier authority split for memory, protected stores, and actions

Background extraction enforces three distinct authority tiers:

1. **Automatic durable knowledge (`tachi.db`):** `Episode` records, objective project/technical facts, explicit owner decisions, and entity anchors are committed automatically by the background subagent. Contradicted or outdated entries are linked via atomic `supersede_entry(old_id, new_entry)` chains rather than silently overwritten or left conflicting.
2. **Owner-approved identity and profile (`user_model.db`, `soul.db`):** Inferred owner values, goals, preferences, habits, or constraints, and any Soul Tone/Story changes, never auto-activate. `crates/zeroclaw-runtime/src/agent/soul_reflection.rs` remains physically separate on its `user`-role-only input rule ([ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md) §4) so tool or delegation text cannot steer persona or User Model proposals.
3. **Inbox-gated action candidates:** Follow-up todos, reminders, or proposed Tachi delegations extracted from conversations or digests are recorded only as non-executing review candidates. No reminder is scheduled and no `tachi_staff(start)` runs until the owner explicitly confirms the candidate on an authenticated surface.

### 6. Daily `04:00` deep-sleep compilation and quiet Daily Digest

Once per local day at the configured natural day boundary (`04:00` local time by default, with automatic catch-up on wake from host sleep if overdue), the daemon runs a cross-session deep-sleep pass ("Auto Dream"):

- Reads the prior day's `Episode` stream and recent `Core` / entity records from `tachi.db`;
- Reconciles cross-session contradictions via `supersedes`, archives stale transient notes, and publishes the refreshed **Pinned Memory Index** (`PinnedDigest`);
- Writes a structured **Daily Digest** (`DailyDigest`) covering settled decisions, key thread progression, cross-day latent patterns or contradictions between stated goals and recent work, and pending action candidates;
- Exposes the Daily Digest quietly in the authenticated Web and phone views by default, creating a proactive notification candidate only when high-priority items qualify under `#63` quiet-hours and attention policy.

### 7. Split fast and flagship model routing

Background memory work separates high-frequency mechanical passes from daily synthesis:

- **Fast route (watermark compaction, report dehydration, idle flush, light-sleep enrichment):** Supports an optional dedicated fast or local model-provider route in configuration; falls back to the agent's configured primary model when unset.
- **Primary flagship route (daily `04:00` deep-sleep compilation):** Defaults to the agent's primary reasoning model so cross-day contradiction detection and latent insight extraction retain full reasoning depth.

### 8. Delegation continuity: redacted context, `rmcp` SSE wake-up, and proxy `continue`

ZeroClaw and Tachi close the L2 loop without breaking `TachiStaffParams` wire compatibility or weakening authority boundaries:

1. **Outbound redacted brief:** When starting a Staff run, ZeroClaw may prepend a compact, technical `[Task Context]` derived from the current session checkpoint and admitted project memories. Raw Soul bytes, User Model records, and unrelated private conversations are never included ([ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) §8).
2. **`rmcp` SSE wake-up doorbell:** Once `tachi-server` emits MCP `2026-07-28` server-to-client task status notifications over Streamable HTTP SSE (`kckylechen1/Tachi#2003`), ZeroClaw maintains a lightweight loopback SSE listener alongside bounded fallback polling. A notification carries only run identity and revision hints, never self-authenticating instructions: ZeroClaw always calls `tachi_staff(status)` to verify canonical state and treats worker text strictly as untrusted subordinate evidence, never as an `owner` turn on `/ws/chat`.
3. **Chief-of-Staff proxy continuation (`action = "continue"`):** When verified canonical status is `TASK_STATE_INPUT_REQUIRED` (non-partial `awaiting_instruction`):
   - **Auto-answer when grounded:** If the answer is already established by the current session's owner thoughts, active `tachi.db` decisions, or approved `user_model.db` rules, and requests no new scope or privilege, the body calls `tachi_staff(action = "continue", dispatch_id, expected_status_revision, task)` directly and records a transparent audit note for the final report.
   - **Escalate novel trade-offs:** If evidence is absent, ambiguous, or conflicting, the body translates the worker's blocker into concise owner-facing options and waits for the owner's decision before calling `continue`.
   - **Hard safety and loop floors:** Permission grants, destructive actions, external communication, spend, or `AUTH_REQUIRED` states are **never** auto-approved by the body. Automatic proxy continuations per `dispatch_id` enforce a strict small cap (default 2-3 rounds); exceeding the cap halts auto-reply and escalates to the owner.
4. **Inbound report digestion:** On terminal `TASK_STATE_COMPLETED`, the body fetches `result.md` (and calls `finish` when applicable for supervised sessions), reports concisely to the owner (including any proxy-answered questions), and wakes the background subagent to distil durable findings/pitfalls into `tachi.db` while queuing the 64 KiB raw report for cold-window dehydration.

## Consequences

Positive:

- Multi-day conversations keep coherent reasoning context (`why` and rejected options) without hitting 50-message amnesia or blocking foreground turns on synchronous compaction.
- Eliminating per-turn `consolidate_turn` removes one LLM call per chat message while closing the `/ws/chat` governance bypass.
- Prefix-pinned memory indexing and cold-window checkpoint swaps preserve provider prompt-cache hit rates.
- Delegated Tachi runs pause less often on questions the owner already answered in chat, while retaining strict fail-closed approval and anti-loop bounds.

Negative:

- Two repositories (`Tachi` `portable-kernel` / `tachi-server` and `ZeroClaw`) must coordinate Slice 2 primitives before Slices 3-5 consume them.
- Double-buffered checkpoints, watermark tracking, and SSE wake-up handling add state transitions in `sessions.db` and `zeroclaw-runtime` that require crash, session-delete, and stale-revision tests.
- Poorly tuned fast models could over-compress subtle reasoning if checkpoint prompts are not verified against multi-turn decision fixtures.

## Implementation slices

Each slice is one bounded issue/PR under `#374` following `AGENTS.md` replace-before-delete rules:

1. **Slice 1: Architecture baseline (`ADR-025`).** Record this ADR in `docs/book` and link the follow-up slice sequence to `#374`.
2. **Slice 2: Tachi `portable-kernel` primitives and `tachi-server` SSE task notifications.** Add typed `Episode`, `PinnedDigest`, and `DailyDigest` contracts, atomic `supersede_entry`, and day-window queries in `portable-kernel`; emit MCP `2026-07-28` SSE task status notifications from `tachi-server` (`kckylechen1/Tachi#2003`); bump the pinned `portable-kernel` revision in `zeroclaw-memory`.
3. **Slice 3: `sessions.db` checkpoints, four-layer context stack, asymmetric dehydration, and drill-down tools.** Add `session_checkpoints` to `SqliteSessionBackend` with cascade-on-delete; wire L0 pinned index, L1 checkpoint, cold-window report dehydration, tail-attached high-confidence recall, and scoped `session_search` / `episode_read` tools.
4. **Slice 4: Background compaction and `04:00` deep-sleep subagent, Daily Digest UI, and `consolidate_turn` retirement.** Implement the confined sleep-time subagent with fast/flagship routing, 65% watermark snapshot-plus-delta swap, idle/startup Episode flush, `04:00` deep sleep, and Web/phone Daily Digest + candidate confirmation views; retire per-turn `consolidation.rs` and `ws_consolidation_model` once end-to-end parity tests pass.
5. **Slice 5: `tachi_staff` `continue`/`finish`, SSE wake-up listener, and bounded proxy continuation.** Wire `continue` and `finish` in `TachiStaffClient`, attach the loopback SSE wake-up listener with canonical `status` verification, enforce proxy-answer grounding, `AUTH_REQUIRED` refusal, and per-dispatch turn caps, and route completed `result.md` reports into background Episode digestion.

Rollback per slice: Slice 1 is docs-only (`git revert`). Slices 3-5 gate new runtime paths behind config/feature defaults until verified, retain append-only `sessions` rows so disabling checkpoints falls back to raw recent turns, and disable `[tachi]` SSE/proxy continuation cleanly back to explicit status reads.

## Acceptance

This ADR is implemented when:

1. a session exceeding 100 turns retains L0 prefix cache stability across warm turns, stages a background L1 checkpoint past the 65% watermark without blocking foreground replies, swaps atomically on a cold window, and retrieves pre-checkpoint verbatim text via `session_search` while denying cross-session/cross-agent reads;
2. deleting a gateway session removes its `sessions`, `sessions_fts`, and `session_checkpoints` rows while leaving curated `tachi.db` memories intact;
3. short idle sessions flush unflushed turns into `tachi.db` `Episode` rows and run light-sleep governance on `/ws/chat` with zero per-turn LLM calls, and legacy per-turn `consolidate_turn` is deleted with its exclusive dead code;
4. the `04:00` deep-sleep pass (including overdue wake catch-up) supersedes contradicted facts, refreshes the `PinnedDigest`, writes a `DailyDigest` visible in the owner UI, keeps `soul_reflection` isolated on `user`-role messages, and never auto-executes extracted action candidates or mutates `user_model.db` / `soul.db` without owner approval;
5. a delegated Tachi run entering `TASK_STATE_INPUT_REQUIRED` wakes ZeroClaw via SSE, verifies canonical `status`, auto-answers via `continue` when grounded in session/memory facts (with audit trail), escalates novel or `AUTH_REQUIRED` prompts to the owner, enforces the per-dispatch auto-continue cap, and digests terminal `result.md` into `tachi.db` while dehydrating stale report bytes in subsequent turns.

## References

- [ADR-005: Pluggable memory backends](./ADR-005-pluggable-memory-backends.md)
- [ADR-010: Separate conversation history, curated memory, and enrichment authority](./ADR-010-memory-authority-boundaries.md)
- [ADR-016: Growth proposed by the agent and approved by the owner](./ADR-016-growth-proposed-by-agent-approved-by-owner.md)
- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-019: One body, many hands](./ADR-019-one-body-many-hands.md)
- [ADR-020: Durable turns, replay classes, background work, and attention](./ADR-020-durable-turns-replay-and-attention.md)
- [ADR-024: Bounded Tachi provider broker for inference](./ADR-024-bounded-tachi-provider-broker.md)
- [History management](../../agents/history-management.md)
- [Memory and payload lifecycle](../memory-payload-lifecycle.md)
- [Runtime state and persistence](../runtime-state-and-persistence.md)
- Issues `#63`, `#374`, `#380`, `#381`, `#382`, `#388`; `kckylechen1/Tachi#2003`
