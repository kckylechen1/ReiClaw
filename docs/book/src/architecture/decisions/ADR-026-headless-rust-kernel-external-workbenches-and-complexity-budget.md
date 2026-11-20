---
id: ADR-026
title: Headless pure-Rust kernel, external pluggable workbenches, and complexity budget
date: 2026-11-19
status: accepted
relates-to:
  - ADR-001
  - ADR-002
  - ADR-005
  - ADR-010
  - ADR-011
  - ADR-012
  - ADR-013
  - ADR-017
  - ADR-019
  - ADR-024
  - ADR-025
  - ADR-027
  - Cargo.toml
  - crates/reiclaw-gateway/
  - crates/reiclaw-config/
  - crates/reiclaw-macros/
  - crates/reiclaw-runtime/
  - web/
---

# ADR-026: Headless Pure-Rust Kernel, External Pluggable Workbenches, and Complexity Budget

## Status

Proposed and accepted by the owner on 2026-11-19, alongside owner acceptance of [ADR-019](./ADR-019-one-body-many-hands.md). Acceptance records the architectural boundaries, ADR reconciliations, and pruning rules; it does not by itself delete code, weaken a protected contract, or reorder `#374`. Every implementation, retirement, or consolidation slice remains subject to `AGENTS.md` (one issue slice = one PR, replace before delete, never silently weaken security or approval boundaries).

### Amendment 1 (2026-11-20): retain useful capabilities and consolidate their implementation

Following the owner's review of [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md), this amendment replaces the original blanket retirement of in-process peer messaging and narrows the overlapping-tools retirement list. Authorized named-peer collaboration and mid-task communication remain product capabilities. The old coupling between sending a message and implicitly starting an uncorrelated detached turn is a replacement target; it does not make peer identity, communication, or an explicitly owned independent peer lifecycle obsolete. `peer_groups` also carries external-peer and channel administration permissions, which must be preserved or explicitly migrated.

Personal commitments and reminders, bounded L1 work, and Tachi runs keep their distinct owners. Existing `task_list` and scheduler paths are retained while their real consumers need them; a filename containing `task` or `schedule` is not evidence of a duplicate execution ledger. The amended decisions in §4 and S9 govern remaining work. This amendment does not restore the retired Skill authoring IDE, tunnel managers, backup engine, or other already-retired duplicate tools.

S10 and S11 retain their replacement acceptance gates for deleting the embedded UI and local providers. They are not prerequisites for ADR-027's L1, continuation, peer communication, or owner-workflow improvements through the retained runtime and gateway. Line and crate counts remain complexity budgets, never reasons to remove retained behavior or its enforcement.

This record establishes the following relationships with prior ADRs without rewriting accepted history:

- **Extends** [ADR-013](./ADR-013-channels-as-gateway-clients.md) and [ADR-019](./ADR-019-one-body-many-hands.md): user-facing UIs (desktop workbench, mobile app) are out-of-process Gateway Clients in their own repositories, not an embedded web application inside the Rust kernel repository.
- **Amends** [ADR-002](./ADR-002-trait-driven-extensibility.md): retires obsolete in-process extension contracts (`Peripheral`, in-process `Channel`, redundant `Observer`, and single-impl `MemoryStrategy`) and enforces the `ADR-019 §3` threshold for traits and workspace crates.
- **Reconciles** [ADR-005](./ADR-005-pluggable-memory-backends.md) (default memory backend is now `memorycore` under [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) and `#745`; SQLite, PostgreSQL, Qdrant, and Lucid are retired), [ADR-010](./ADR-010-memory-authority-boundaries.md) (superseded by ADR-025), [ADR-011](./ADR-011-multi-agent-runtime-boundaries.md) (per-alias isolation and authorized peer communication stay; Amendment 1 replaces implicit detached peer starts with explicit execution ownership and correlated receipts), and [ADR-012](./ADR-012-generation-scoped-live-config-apply.md) (configuration is file-first; live apply does not justify an embedded dynamic web-form reflection stack).
- **Preserves** Soul and User Model governance ([ADR-014](./ADR-014-reviewed-agentsoul-projection.md) through [ADR-018](./ADR-018-soul-character-model.md)), body/Node/Tachi authority ([ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md), with the work split amended by ADR-027), realtime voice carrier ([ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md)), Tachi provider broker end state ([ADR-024](./ADR-024-bounded-tachi-provider-broker.md) Amendment 1), and foreground/sleep memory ownership ([ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md)).

## Context

At the 2026-11-19 audit, after retiring `zeroclaw-channels`, 54 compat-preset model providers (`#734`), browser automation tools, knowledge graphs, and legacy config migrations, the ReiClaw repository carried ~500k LOC of Rust across 24 workspace crates (~218k non-test LOC) plus ~49.7k LOC of embedded React/TypeScript (`web/`). These are the historical campaign baseline, not a current inventory or a deletion quota.

The 2026-11-19 full-repository architectural audit identified four structural sources of avoidable complexity that `rustc` cannot prevent:

1. **An embedded Web SPA distorting the Rust kernel.** Maintaining `web/` (~49.7k LOC, including a 12.3k-line 18-locale `i18n.ts`) and its generic visual TOML editor (`FieldForm.tsx` + `Config.tsx`) forces the Rust kernel to maintain `api_config.rs` (5.3k LOC), `api_sections.rs` (1.9k LOC), a 1,315-LOC hand-rolled `toml_edit` dirty-path patcher in `save.rs`, and a single 3,029-LOC procedural macro function (`derive_configurable` in `reiclaw-macros`) generating 24 inherent methods across 123 structs.
2. **Multi-domain UI workflows belong in a modular external client, not in the kernel.** The owner wants one general-purpose desktop workbench that adapts per scenario through pluggable sidebar modules (for example, a Hyperion stock/portfolio/risk panel talking directly to `hapi-edge`, a scoped document and file preview panel, and a Node/Tachi status panel), plus a lightweight mobile client later. Both connect to the same headless Gateway. Placing domain panels inside ReiClaw would violate Hyperion's isolation boundary and keep dragging frontend tooling into the daemon repository.
3. **Inherited platform features and overlapping entry points.** The audit found an in-process Skill authoring and self-evolution IDE (~17k LOC across `skills/` and `skill_*` tools), 6 OS-level tunnel process managers (`tunnel/`), enterprise backup and retention engines (~4.7k LOC), overlapping coding/orchestration tools (`git_operations`, `pipeline`, `llm_task`), and a duplicate Prometheus `Observer` stack alongside `reiclaw-log`. It also grouped `schedule` and `send_message_to_peer` into retirement candidates; Amendment 1 separates their retained scheduling and collaboration behavior from duplicate or insufficiently owned execution paths.
4. **Dead seams, unwired safety surfaces, and matryoshka wrappers.** The audit found 41 uncompiled orphan files in `src/`, ~6.5k+ LOC of zero-caller modules, 4 over-split single-consumer or single-file glue crates, a 5-layer `Memory` decorator stack whose outermost `RetrievalPipeline` is a default no-op, ceremonial scaffolding around single-shot `ReasoningSubagentTool` (including a write-only `review_queue`), and two unwired runtime seams (`EstopManager` written by CLI with zero runtime enforcement, and Gateway constructing an empty `HookRunner::new()`).

## Decision

### 1. ReiClaw is a headless pure-Rust kernel

The ReiClaw repository owns **only** the always-on personal controller daemon (`rei`) and its protocol-level gateway bridges and clients. It owns **zero frontend code, zero Node/npm build chains, and zero domain-specific UI panels** (such as stock charts, portfolio views, or document editors).

`web/` is frozen immediately (no new feature pages). Once an external client passes replacement acceptance (§2), `web/`, the `embedded-web` Cargo feature, `static_files.rs`, `xtask` web bundling, and CI's Node build step are retired from this repository.

The kernel continues to own authorized conversation, collaboration, personal commitments, and scheduling. Headless delivery changes where those capabilities are presented; it does not remove them from the product.

### 2. UI surfaces are external Gateway Clients (Desktop Workbench and Mobile Client)

Following ADR-013 and ADR-019 ("one door, one contract, any language"), human interfaces attach to the kernel strictly over `/ws/chat` and the versioned Gateway REST surface:

- **Desktop Workbench (separate repository, e.g. Tauri + React/TS):**
  - Owns OS shell integration (menu bar tray, daemon health/lifecycle supervision, native notifications, global shortcut) and rich visual rendering where web tooling excels (financial charts, Markdown/KaTeX/PDF/code preview).
  - Fixed core spine: conversation (`/ws/chat`), approval inbox (`approval_request` / `approval_response`), Soul and User Model review (`/api/soul`, `/api/user-model/candidates`), and pairing/status (`/api/pair`, `/api/node-identity`).
  - **Pluggable Sidebar Panel Registry (frontend-owned):** scenario modules mount as independent panels. A **Hyperion panel** talks to `hapi-edge` directly, preserving the rule in [Fork Notes](../../contributing/fork-notes.md) that ReiClaw never touches trading databases or broker APIs. A **Document panel** reads scoped workspace files via ReiClaw's existing `browse` surface or local read-only access. A **Node/Tachi panel** shows device and delegation status.
  - **Zero domain leakage into the kernel:** the kernel never models stocks, K-lines, or portfolios. If future turns want to suggest opening a sidebar panel, the kernel emits at most a generic, optional presentation hint (`panel_id` and opaque `payload`) that any client may ignore.
- **Mobile Client (future separate repository, e.g. Flutter or SwiftUI):**
  - Connects to the exact same `/ws/chat` and REST contract.
  - Scoped to high-frequency mobile workflows (chat, voice, push notifications, approvals, Soul/User Model review, compact status cards) and never replicates the desktop multi-panel workspace.

### 3. File-first configuration; retire the dynamic web-form reflection stack

`config.toml` (edited directly or via narrow `rei config` commands) is the single owner's configuration surface. External UI clients **must not** rebuild a generic full-schema TOML form editor (`FieldForm`). UI settings stay limited to explicit, high-frequency operational endpoints (such as active model alias, quiet hours, voice consent, and estop state).

After `web/` is retired:

- Retire dynamic UI-form endpoints (`api_sections.rs` and generic `prop_fields`/form-metadata routes in `api_config.rs`), keeping only explicit operational endpoints required by CLI and external clients.
- Shrink `reiclaw-macros` (`derive_configurable`) and `reiclaw-config/src/save.rs` by removing display-only form reflection metadata (`prop_fields` UI hints) and replacing or drastically simplifying the 1,315-LOC hand-rolled `toml_edit` dirty-path patcher.
- [ADR-012](./ADR-012-generation-scoped-live-config-apply.md), if later adopted, applies only to bounded runtime reloads and must not re-introduce dynamic schema-form reflection.

Form-only metadata may retire with its last consumer. Runtime validation, credential and permission scope, safe persistence, and operational configuration contracts remain until their own consumers are replaced; removing a form generator is not permission to remove those guarantees.

### 4. Kernel scope pruning (aligning body capabilities with ADR-017)

Move external execution and host administration to their named owners, and remove proven duplicate entry points in bounded replace-before-delete slices. Retain body-owned capabilities and their policy enforcement as specified below:

| Area | Decision | Replacement / Owner |
| --- | --- | --- |
| Dead / uncompiled code and 0-caller seams | **Delete immediately** (Wave 1) | None (0 production consumers: 41 orphan files in `src/`, `a2a-v1.proto`, `reiclaw-api/{elicitation,jsonrpc,peripherals_traits}`, `reiclaw-tools/node_capabilities`, `reiclaw-infra/{debounce,stall_watchdog}`, `reiclaw-log/{observer_bridge,chain}`, `reiclaw-runtime/{nodes,subagent,security/{audit,prompt_guard,auth_provider}}`, dead scripts) |
| Retained safety and hook seams (`estop`, `otp`, gateway `HookRunner`) | **Retain and wire** (Wave 2) | Owner decision: keep `estop` and `otp`, wire `EstopManager` into `SecurityPolicy` / tool execution enforcement with integration tests, and fix Gateway `HookRunner::from_config` (while pruning 11 dead `HookRunner` dispatch methods) |
| Skill authoring IDE (`creator`, `improver`, `suggestions`, `review`, `testing`, `skill_manage`, `open-skills` git sync) | **Retire authoring machinery** (Wave 3) | Automated authoring or modification runs via Tachi. Retain the minimal `SKILL.md` loader, `read_skill`, configured bundle resolution, and the admission and execution policy required by retained skill consumers. Existing operator CLI/Gateway bundle lifecycle commands, including `scaffold`, require their own consumer and replacement audit. A skill never grants tools or permissions on its own. |
| In-process tunnel managers (`tunnel/*`) | **Retire** (Wave 3) | Host OS manages Tailscale, `cloudflared`, or VPN daemons |
| Enterprise backup and retention engine (`backup_tool`, `data_management`, `api_backup_retention`) | **Retire** (Wave 3) | Host OS backups (`Time Machine`, `restic`, `rsync`) on `~/.rei` |
| Proven duplicate builtin tools (`git_operations`, `pipeline`, `llm_task`, `file_upload*`/`file_download`, `image_gen`/`image_info`, and the retired `todo_write` board) | **Retire per caller audit** (Wave 3) | Name the retained consumer and proven replacement, or explicit behavior retirement, per tool. Existing Tachi, bounded L1, attachment, and MCP paths replace their respective functions; a similar tool name alone is not replacement evidence. Completed retirements stay retired. |
| Named-peer collaboration and mid-task communication (`send_message_to_peer`, `peer_groups`) | **Retain capability; replace implicit detached starts** | Messages target an authorized peer identity with conversation/run context where applicable, correlated delivery/reply evidence, and receipts for any resulting work. New work has an explicit execution owner; independent peer work need not die with the sender's turn. Preserve per-peer policy, cost attribution, identity binding, and external-peer/channel administration scopes. External harness runs remain Tachi-owned. Replace the old launch path only after retained communication and authority behavior pass acceptance. |
| Personal commitments and reminders (`task_context`, `task_list`, `task_section`, `agent_tasks`, reminder occurrences) | **Retain existing owners** | The body's existing session store owns accepted multi-day commitments and reminder intent; the existing reminder worker and outbox own delivery. `DailyDigest` candidates remain proposals and Tachi owns delegated execution state. These facts are distinct; do not create another workflow ledger or delete commitments because their implementation is named `task`. |
| Scheduling (`cron`, `cron_*`, `schedule`) | **Retain scheduling; consolidate only duplicate frontends** | `schedule` is a live shell-job frontend over the same cron store. Retire that frontend only after a named retained API covers required create/read/update/cancel behavior, existing jobs, execution limits, and authorization checks. Keep owner reminders, bounded background work, and cron sentinels; the L1 no-durable-task rule does not retire scheduler-owned work. |
| Duplicate telemetry (`observability/prometheus` and `Observer` polymorphism) | **Consolidate** (Wave 3) | Single structured observability stream via `reiclaw-log` |
| Legacy stores and one-time importers (`acp_session_store`, JSONL `session_store`, `migration.rs`, `reiclaw-log/migrate.rs`) | **Retire after window** (Wave 2) | SQLite `sessions.db` and `memorycore.db` |
| Model providers (`reiclaw-providers`, 45.7k LOC) | **Freeze; retire via ADR-024** (Wave 4) | Do not invest in local provider refactors; remove wholesale once Tachi broker slices pass acceptance |

### 5. Anti-complexity rules (amending ADR-002, enforcing ADR-019 §3)

To prevent compiler-clean but unmaintainable complexity from regrowing:

1. **No crate without a boundary.** A workspace crate must have at least two distinct consumers or enforce a hard dependency isolation boundary (such as `reiclaw-gateway-client`, `reiclaw-http`, or `reiclaw-provider-dispatch`). Fold over-split single-consumer or single-file glue crates (`reiclaw-spawn` into `reiclaw-log`, `reiclaw-cost` and `reiclaw-platform` into `reiclaw-runtime`, `reiclaw-schema-markdown` into `reiclaw-config`), and move `tachi_bridge/staff_tests.rs` into `reiclaw-tools`.
2. **No trait for one implementation.** Amending [ADR-002](./ADR-002-trait-driven-extensibility.md): do not add or retain a trait with 0 or 1 production implementations unless it breaks a crate dependency cycle (such as `NodeTools`). Collapse ceremonial wrappers (`subagent_v1` unused hooks, queues, and 10ms abort polling; default no-op `RetrievalPipeline`; test-only `McpTransportConn` and owned `ProviderDispatch`).
3. **One pipeline per concern.** Collapse parallel implementations of the same runtime path: one system prompt builder, one loop/repetition guard, one shared turn bootstrap across `Agent::turn`, `turn_streamed`, and `loop_`, one Gateway auth middleware/extractor replacing ~80 manual handler checks, and one Gateway client crate shared by all retained Rust bridges.
4. **No hidden dead code.** Remove crate-wide `#![allow(dead_code, unused_variables, unused_imports)]` from `src/main.rs` and stop compiling `src/` twice as independent `lib` and `bin` module trees.
5. **No placebo safety controls.** Every retained security or safety primitive exposed in CLI or configuration (including `estop` and `otp`) must have an integration test proving runtime enforcement on the execution path.
6. **No capability removal by label or count.** Before deleting an overlapping module, identify its live consumers, the owner of its state and execution, and the behavior being replaced or explicitly retired. Preserve authorization, identity, cancellation, recovery, and user-data contracts at the replacement boundary. A smaller file, trait, or crate count does not establish equivalent behavior.

## Execution Waves (`#374`-compatible slices)

Each slice lands as its own PR under `AGENTS.md`. The waves below retain the historical campaign identifiers; completed slices are not reopened by Amendment 1. Remaining work follows actual dependencies. S10 and S11 gate deletion of their respective implementations, not delivery of unrelated ADR-027 capabilities:

- **Wave 1 (P0 dead-code sweeps, zero behavior change):**
  - `S1`: remove 41 uncompiled orphan files and 9 zero-caller facades in `src/`, remove dead Homebrew onboard helpers, drop unused root `reiclaw-tool-call-parser` dep, and eliminate dual lib/bin compilation and crate-wide `#![allow(dead_code)]`.
  - `S2`: delete verified zero-caller modules (`a2a-v1.proto`, `reiclaw-api/{elicitation,jsonrpc,peripherals_traits}`, `reiclaw-tools/node_capabilities`, `reiclaw-infra/{debounce,stall_watchdog}`, `reiclaw-memory/chunker`, `reiclaw-log/{observer_bridge,chain}`, `reiclaw-runtime/{nodes,security/{audit,prompt_guard,auth_provider}}`, dead scripts).
  - `S3`: collapse zero-production/single-impl glue (`McpTransportConn`/`create_transport`, `subagent/`, dead `agent/dispatcher` & `agent/eval` functions, unused owned `ProviderDispatch`).
- **Wave 2 (P1 wiring, ceremony collapse, and crate folding):**
  - `S4`: wire retained `estop` / `otp` into runtime enforcement with tests; fix Gateway `HookRunner::from_config` and prune 11 dead hook dispatchers.
  - `S5`: collapse `subagent_v1/mod.rs` ceremonial layers while keeping `ReasoningSubagentTool`, `admit()` privacy boundary, and `#405` advisor budget; remove leaking write-only `review_queue`. ADR-027's bounded read-only child modes and mid-run communication reuse the shared runtime. Reuse or reshape useful message types; remove only superseded scaffolding without restoring the write-only review queue.
  - `S6`: fold 4 over-split crates (`reiclaw-spawn`, `reiclaw-cost`, `reiclaw-platform`, `reiclaw-schema-markdown`), repatriate 6 single-crate modules from `reiclaw-api`, and move `tachi_bridge/staff_tests.rs` into `reiclaw-tools`.
  - `S7`: retire verified dead config sections (`Nevis`, `WebAuthn`, `Acp`, `Trust`, `MediaPipeline`, `LinkEnricher`, `NodeTransport`), legacy stores (`acp_session_store`, JSONL `session_store`, `migration.rs`), empty Cargo channel features, and retired CLI error stubs.
- **Wave 3 (P2 deduplication and scope pruning):**
  - `S8`: unify `agent/` dual prompt/loop/turn pipelines, remove default no-op `RetrievalPipeline`, and convert Gateway manual auth checks into Axum extractors/middleware.
  - `S9`: retire the Skill IDE factory, in-process `tunnel/`, enterprise backup tools, proven duplicate tools, and duplicate Prometheus `Observer`, one bounded slice per area. Apply amended §4 to remaining work: preserve peer collaboration and permissions, personal commitments/reminders, and scheduling; replace the implicit detached peer launch path and consolidate scheduler frontends only with retained-behavior acceptance.
- **Wave 4 (P3 external workbench and ADR-024 cutover):**
  - `S10`: after the external desktop workbench proves `/ws/chat`, approvals, Soul/User Model review, and pairing end to end, delete `web/`, `embedded-web`, `api_config`/`api_sections` dynamic form surfaces, and simplify `reiclaw-macros` and `save.rs`.
  - `S11`: complete [ADR-024](./ADR-024-bounded-tachi-provider-broker.md) broker cutover and delete `reiclaw-providers`.

## Acceptance

- **Wave 1–2 hygiene:** CI green on every slice; `src/` has zero unmounted `.rs` files; `src/main.rs` compiles without crate-wide `#![allow(dead_code)]`; `estop` enforcement and Gateway `HookRunner` wiring are covered by automated tests. Record workspace crate count against the original 24-to-20 consolidation target; preserving a required boundary takes precedence over reaching a count.
- **Retained capability parity:** a peer-path replacement proves authorized mid-task messages and correlated replies, explicit ownership of new or independent work, per-peer policy and cost attribution, and unchanged external-peer/channel administration scope. Any commitment or scheduling consolidation preserves existing commitments, due reminders, cancellation, bounded execution, and delivery receipts. Data migration or explicit behavior retirement is named before removing a live consumer.
- **External workbench cutover (prerequisite to deleting `web/`):** the external desktop workbench passes live end-to-end `/ws/chat` streaming, approval handling, Soul/User Model review, and pairing against `rei gateway` with zero kernel frontend assets.
- **Provider cutover (prerequisite to deleting `reiclaw-providers`):** ADR-024's broker replacement passes its scoped inference, credential, failure, and rollback acceptance before local provider removal. Existing inference continues to serve retained and ADR-027 capabilities until that cutover.
- **Directional complexity budget:** combined with ADR-024, reduce non-test Rust LOC from ~218k toward ~120k to 145k, workspace crates from 24 toward 18 or fewer, and repository frontend/Node footprint to 0. Measure these as outcomes of consolidation and accepted retirement; they do not override retained behavior, replacement gates, or protected boundaries.

## Consequences

Positive:

- ReiClaw returns to a human-comprehendible, pure-Rust personal controller kernel.
- Desktop (Tauri with pluggable Hyperion, Document, and Node panels) and mobile clients iterate independently without coupling to kernel CI or binary size.
- Retires the largest metaprogramming complexity sink (`web/` + `api_config` + `save_dirty` + `reiclaw-macros` form reflection).
- Closes real operational hazards (unwired `estop`, empty Gateway `HookRunner`, write-only `review_queue`).
- Retains useful peer collaboration, personal commitments, and scheduling while removing duplicate execution paths and preserving their authority owners.

Negative:

- Full personal setup installs the headless `rei` daemon plus a separate desktop/mobile client rather than serving a bundled localhost web app.
- Advanced configuration edits move to `config.toml` and `rei config` instead of a generic browser form.
- Each feature retirement requires disciplined caller verification and explicit fail-closed notices where security settings are affected.

## References

- [ADR-002](./ADR-002-trait-driven-extensibility.md): trait-driven extensibility (amended by this record).
- [ADR-005](./ADR-005-pluggable-memory-backends.md): memory backend contract (default backend superseded by ADR-025 / `#745`).
- [ADR-013](./ADR-013-channels-as-gateway-clients.md): out-of-process Gateway Clients.
- [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md): one personal agent body, edge devices, and Tachi delegation.
- [ADR-019](./ADR-019-one-body-many-hands.md): crate, process, and trait splitting rules.
- [ADR-024](./ADR-024-bounded-tachi-provider-broker.md): Tachi provider broker and zero-provider end state.
- [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md): long-context stack, sleep memory, and `memorycore.db`.
- [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md): bounded L1 work, peer communication, Tachi execution, and owner-workflow closure.
- [Fork Notes](../../contributing/fork-notes.md): Hyperion isolation rules and fork-specific invariants.
