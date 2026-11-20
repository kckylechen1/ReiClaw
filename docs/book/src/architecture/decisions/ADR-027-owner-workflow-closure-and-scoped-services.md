---
id: ADR-027
title: Owner workflow closure, bounded collaboration, and scoped services
date: 2026-11-20
status: accepted
relates-to:
  - ADR-011
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

# ADR-027: Owner Workflow Closure, Bounded Collaboration, and Scoped Services

## Status

Accepted by the owner on 2026-11-20 and clarified on the same date to preserve mid-run communication and peer collaboration while reducing duplicate implementation. Acceptance establishes a target contract; it does not claim runtime delivery, grant new permissions, delete code, or reorder `#374`. Each implementation slice needs a bounded issue, PR, and validation under `AGENTS.md`.

Relationships with existing decisions:

- **Amends [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) §3:** L1 includes a bounded read-only pool and zero-tool Advisor. L2 uses Tachi for general external execution and collaboration, including engineering, research, independent review, and end-to-end or computer-use evaluation.
- **Amends [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) §4 and its S5/S9 retirement scope, recorded there as Amendment 1:** retain mid-run communication, authorized named-peer collaboration, personal commitments/reminders, and scheduling. Replace redundant or incomplete implementations after retained behavior is covered. Preserve the isolation and explicit communication boundaries in [ADR-011](./ADR-011-multi-agent-runtime-boundaries.md).
- **Extends [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md):** complete scoped continuation, SSE wake-up, candidate dispatch, and evidence-based attention learning through existing owners. Wiring an existing prototype requires validating its scope and recovery semantics first.
- **Preserves** Soul/User Model governance, approvals, Client/Node boundaries, the `#197` Spawn Wall, and the complexity budget. [ADR-024](./ADR-024-bounded-tachi-provider-broker.md) continues to govern inference-broker cutover and availability independently of delegation.

## Context and delivery baseline

The product needs one body to carry an authorized goal from capture through investigation, delegation, clarification, verification, and a useful result. Routine questions from workers should reach the body first. Mid-run correction and collaboration are part of completing that goal.

The original audit used `7a322a49f9`. Reconciliation against `cd6ca4f5cb8a672dda0195dad957a54f45e0961f` distinguishes current behavior from this ADR's target:

| Area | Current evidence | Required change |
| --- | --- | --- |
| L1 | `ReasoningSubagentTool` uses a zero-tool completion. Mid-run message types exist without a production sender/receiver. | Retain Advisor; add confined Scout/Recall runs and bounded communication over the shared loop. |
| Peer messaging | `send_message_to_peer` checks peer/channel membership and recipient cost attribution, but an internal target starts a detached full turn and returns acceptance without an automatically correlated completion receipt. | Preserve peer relationships; separate message delivery, continuation of existing work, and authorized new work. |
| L2 continuation | Resolver and observer seams exist; gateway startup does not wire the production resolver/SSE path. | Scope evidence to the originating delegation, persist continuation reservations before sending, then wire and verify the installed pair. |
| Candidate review | Review changes a `DailyDigest` status; no candidate dispatch endpoint exists. | Add explicit promotion with candidate-level atomic claims and existing Tachi request bindings. |
| Attention feedback | Current outbox/mute and digest records include state snapshots; repeated snoozes and a complete ordered owner-action history cannot be reconstructed from them. | Preserve authenticated event evidence before claiming repeated-feedback learning. |
| Slimming | `#782` through `#785` already retired overlapping builtins, backup/retention, tunnel managers, and skill authoring/automatic registry sync. | Consume those deliveries. Retained skills APIs still have consumers and their own cutover gate; do not recount old removals or remove retained capabilities by name. |

The remaining work should reuse the existing loop, session store, memory store, scheduler, review inbox, outbox, and Tachi delegation path. A new generic workflow platform or durable local subagent ledger is outside this decision.

## Decision

### 1. The body owns authorized workflow closure

Voice, quick capture, messaging bridges, and workbenches submit personal-agent requests through the gateway and the existing authenticated or device-scoped intake. Sessions and memory retain their current canonical owners. External notes remain scoped resources, not a replacement personal memory or cross-agent session store.

Clients may use the body's admitted delegation facade; they cannot turn client input into an unmediated harness launch or owner approval. This does not prohibit an external domain workbench from accessing its own services within the boundary already established by ADR-026.

Once the owner's goal and scope are authorized, the body carries the workflow forward: answer a worker from admitted facts, correct scope within that grant, investigate a failed check, request an authorized repair, arrange independent validation, and report the remaining acceptance gap. A worker's terminal success is evidence, not proof that the owner's goal is complete. Ask the owner when meaning, authority, budget, or a protected decision is actually missing; do not ask again merely because work is delegated or takes another step.

Existing action approvals remain binding. Ordinary workers default to bounded implementation/review and PR preparation. An explicitly authorized landing actor may merge through the repository's required CI and risk-review rules. Neither a worker completion nor this ADR grants merge authority, and no universal human-only merge rule is introduced.

### 2. Bounded L1 work and general L2 delegation

| Level | Work and lifecycle | Authority | Tachi delegation |
| --- | --- | --- | --- |
| **L0 Direct** | The main body's turn: conversation, direct investigation, Node intents, scoped actions, and workflow decisions | Configured `Read` and gated `Act`; owns identity, owner interaction, and interpretation of results | No external run for direct work; the body may initiate L2 |
| **L1 Subagent Pool** | Ephemeral Advisor or parallel Scout/Recall runs within the active parent turn | Zero tools for Advisor; an explicitly admitted `ToolOperation::Read` subset for Scout/Recall; no independent durable task rows | Not required |
| **L2 External Delegate** | Tachi-managed external work: engineering, research, independent review, computer-use/end-to-end evaluation, and work requiring an external environment or restart-durable execution | The task's admitted Tachi profile, project scope, tools, and budget; no ambient body credentials or personal stores | Required for delegated external execution |

Codex, Claude Code, and OpenCode are examples of harnesses, not a restriction on Tachi's product scope. Tachi remains the general cross-host dispatch and collaboration control plane. Its Staff, Tasks, A2A, and evaluation capabilities must be adopted through their actual supported contracts; naming them here does not claim all ReiClaw adapters are delivered.

The table describes **delegation**, not inference transport. Before ADR-024 broker cutover, L0/L1 may use the retained direct provider path while Tachi is stopped. After an admitted path cuts over to the sole broker, unavailable Tachi inference produces typed `inference-unavailable`; it does not authorize direct fallback or a local harness launcher.

#### L1 admission and lifecycle

1. **Trusted read classification:** Assemble a child registry from the intersection of the parent's admitted tools, caller/device privacy scope, and a trusted `Read` classification. Tool names, model claims, `needs_approval == false`, and a server's unverified read-only hint are insufficient. Unknown or mixed-effect operations are denied. Shell/process, file edits, git, cron mutation, memory writes, external messaging, and `tachi_*` are excluded.
2. **Confinement survives deferred discovery:** If tool search or MCP activation is available, every activated tool is checked against the same child policy before use. Discovery cannot recover the parent's broader registry or credentials. An approval-requiring tool is excluded even if otherwise read-only; a reached approval gate returns a typed refusal, never a child approval card.
3. **Bounded concurrency and shared budgets:** Initial proposed defaults are up to four concurrent children, six to eight tool rounds, and a 30-second child deadline, under per-child and aggregate parent token/action budgets. These are bounded admission parameters, not claims about current runtime defaults. Retries, steering, and context requests do not reset budgets or extend the parent's lifetime.
4. **Fate-sharing and no recursion:** Only a depth-zero parent may spawn. Children cannot spawn, detach, or own a durable queue. The parent's cancellation token reaches every child and its in-flight local work. The first delivery covers foreground turns; a scheduled root may use the pool only after demonstrating the same admission, budget, cancellation, and completion contract. A background schedule does not make its children independently durable.
5. **Privacy and concise results:** Carry the parent's effective device/session/agent restrictions, including public-device exclusion of private memory and session search. Raw Soul/User Model stores and raw parent transcripts are not handed to children. Scoped references and bounded context are admitted explicitly. Raw tool intermediates remain ephemeral; only bounded structured events, findings, and a terminal report enter the parent context.
6. **One loop implementation:** Scout/Recall uses `run_tool_call_loop_with_security` and the confined-loop seam shared with sleep. Preserve `OpaqueModelBinding`, route selection, cost attribution, and estop behavior while consolidating duplicate construction. Advisor remains a zero-tool mode, not a second general-purpose engine.

#### Mid-run communication is a retained capability

The parent owns the goal, permissions, user wording, and final result. A small turn-local protocol supports:

| Direction | Message | Effect |
| --- | --- | --- |
| Child to parent | Need context, ambiguity, blocker, partial finding, or bounded progress | Parent supplies admitted facts, changes emphasis, stops a branch, or decides whether owner input is necessary. |
| Parent to child | Scoped context, correction, graceful stop, or cancellation | Adjust existing work within the admitted objective, tools, privacy scope, budget, and deadline. A materially expanded scope requires fresh admission. |
| Child to parent | One terminal report | Consolidate findings, evidence, uncertainty, usage, and requested parent actions. |

Messages carry run/request correlation and are bounded in size, count, and rate. They do not expose raw tool streams or create a cross-session mailbox. Parent-mediated sharing between sibling children uses the same admitted context boundary. Children cannot use this control channel to send external messages, obtain new authority, or launch more work. A request requiring long owner interaction ends the bounded child; the body can start a freshly admitted child later.

`SubAgentMidRunRequest` and `ReportChannelMessage` are candidate implementation material, not evidence of a working channel and not an unconditional deletion target. Reuse or reshape them in the L1 slice and remove only superseded scaffolding.

#### Peer collaboration has explicit identity and execution ownership

A peer is an authorized collaborator, potentially with its own identity, session, and lifetime. It is not automatically an L1 child. Co-residence in a daemon neither grants trust nor requires all peers to become external CLI harnesses.

Preserve named-peer messaging and channel-scoped peer relationships while separating three operations:

1. **Deliver information:** route to an admitted peer/session and return a delivery receipt. Informational messages need not create a job or require a terminal work result.
2. **Continue existing work:** correlate the message with the existing conversation or run. For a Tachi run, use its admitted dispatch and supported continuation/control operation; do not start a second run to deliver a clarification.
3. **Request new work:** name the initiating authority, receiving execution owner, scope, budget, request identity, and result destination. The receiving body re-admits under its own policy; external delegated work uses Tachi. Detached execution cannot arise merely as an undocumented side effect of message delivery.

Replies and results retain their initiating request reference. Limit automatic reply hops, concurrent work, and message volume to prevent cycles and uncontrolled fan-out. A peer message does not become authenticated owner input, share the sender's private memory, or grant the recipient new tools. A sender's turn cancellation does not silently cancel an independent peer's unrelated work; supported cancellation and unknown outcomes are reported explicitly.

Replace the current implicit detached launcher only after the retained messaging and collaboration cases have a working path. `peer_groups` also carries external-peer and channel administration boundaries and cannot be removed as exclusive launcher configuration.

### 3. Scoped, recoverable continuation and canonical wake-up

For admitted L2 runs, complete the existing ADR-025 observer before production activation:

1. **Resolve the owning agent's model per decision:** use the validated live config snapshot and its admitted fast/primary route. If unavailable, preserve the blocked state and send an appropriate owner notification through `BridgeOutbox`; never borrow another agent's model authority.
2. **Ground in the originating delegation:** persist or resolve its admitted source session, project, task references, and redacted context. Do not collect arbitrary recent sessions or episodes merely because their agent alias matches. Revalidate access when facts are read; deleted source sessions, revoked grants, and stale cached context cannot silently remain eligible. Filter both retrieval and outbound text; `soul.db`, `user_model.db`, protected identity markers, secrets, and unrelated personal context remain excluded.
3. **Keep the existing gate:** automatic continuation is limited to non-partial `INPUT_REQUIRED` for which admitted facts suffice. `AUTH_REQUIRED`, new permissions, spend decisions, and `closure_kind = "partial"` are not auto-answered. Insufficient evidence goes to the owner with the missing decision identified. An existing workflow authorization does not bypass `always_ask` or impersonate an approval.
4. **Reserve before sending:** `SqliteSessionBackend` owns the continuation reservation, dispatch/revision association, and the three-attempt cap in existing `sessions.db`. Atomically reserve an attempt before transmission; concurrent sweeps cannot reserve the same revision twice. Persistence failure prevents sending. After any awaited model/fact resolution, recheck the live grant, route binding, and estop immediately before transmission; a stale snapshot cannot authorize the send. A possible transmission consumes the reservation; `SubmissionUnknown` survives restart and cannot be retried under a new ID or revision without canonical reconciliation. Release only with evidence that nothing was transmitted.
5. **Treat SSE as a wake-up hint:** route-verified MCP Streamable HTTP notifications wake the existing observer; canonical `TachiStaffClient::status` remains authoritative. Retain the 60-second polling fallback. Verify that the installed Tachi facade emits the relevant Staff event; a closed upstream issue or a notification from another task surface is not an integration receipt.

Implement these semantics in the existing observer/resolver, replacing the broad fact collector and send-then-best-effort counter path. Do not activate the old prototype and add a parallel repair coordinator beside it.

### 4. Candidate review, personal commitments, and explicit dispatch

These records serve different purposes and retain their existing owners:

| Fact | Owner | Meaning |
| --- | --- | --- |
| `DailyDigest.action_candidates` and review decisions | Memory/review store | Suggestions and owner review; approval alone does not start work. |
| Personal commitments and reminder occurrences | Existing session/task owner and scheduler/outbox | Multi-day promises to the owner and their due reminders. They are not L1 execution rows. |
| Candidate/request-to-dispatch binding | `SqliteSessionBackend` in `sessions.db` | Local claim, scope, idempotency, and a reference to the delegated run. |
| External execution status and result | Tachi | Canonical runtime truth; local views are projections. |

Retain `task_list`, task context/section, and reminders until an explicit replacement or product retirement is accepted. Do not merge these facts into a new body workflow database.

`POST /api/review/action-candidates/{agent}/{day_key}/resolve` stays status-only. A separate operator-gated `/dispatch` action may promote a pending or approved candidate through the existing `TachiDelegation` path with `StaffingReason::ExplicitUserRequest`. It requires:

- a stable candidate identity and expected source revision, plus caller-supplied `request_id` and an admitted `harness`; optional issue/PR/flow references and a redacted context brief remain bounded (`<= 4,096` bytes);
- an explicit compatibility rule for today's string candidates: deterministic identity, no ambiguous text match, and no new identity merely from editing display text;
- live authorization, admission scanning, outbound redaction, and an atomic candidate-level claim in the same existing session owner as delegation request binding, **before** any start is sent;
- replay matching the admitted candidate revision, harness, route, and payload fingerprint, returning the existing claim only for unchanged arguments; a different request ID for an already claimed candidate returning `409 candidate_already_dispatched`, including unresolved submissions;
- recoverable claim/receipt state across concurrent requests, timeouts, crashes, and review edits. A later explicit new task is a separate authorized delegation, not an automatic retry of the candidate.

An additive `DailyDigest.action_dispatches` field, if needed for presentation, is a rebuildable projection of those claims. It cannot enforce uniqueness or become a second execution ledger. Review changes use narrow, revision-checked updates; an old whole-digest snapshot must not erase review changes or dispatch links. Pin the source revision being promoted and specify cross-store recovery without claiming an atomic transaction across `memorycore.db` and `sessions.db`.

Background extraction and an `approved` label never manufacture execution permission. An existing explicit workflow grant may independently authorize the body to carry out its next step through the normal delegation path. Preserve and check that grant rather than requiring the owner to restate an already authorized task.

### 5. Quiet sentinels over existing scheduling and outbox

Use the existing scheduler, cron run records, memory/review path, and `BridgeOutbox` for monitoring. No new daemon, task database, or embedded monitoring application is needed.

- A sentinel has an explicit read-only policy with bounded approved reads and `mcp_discovered_tool_policy = "explicit_only"`. An empty `always_ask` list is not evidence that its tools are read-only. Shell, writes, external messaging, and Tachi start are excluded from its model tool registry. First delivery uses admitted direct read tools; scheduled L1 scouting requires the lifecycle acceptance in §2.
- A healthy/no-impact check records normal run metadata and emits zero outbox messages. Verified impact names evidence and affected targets; the host persists a bounded finding/candidate and sends one deduplicated notice under existing attention rules.
- Sentinel evidence does not itself authorize remediation. The body may act under a separately established, bounded owner workflow grant; otherwise it requests the missing approval through the existing surface. Workers receive scoped work and default to PR preparation; authorized landing follows §1.

Preserve personal scheduling/reminders and the existing `schedule` behavior until a named replacement covers permissions, old jobs, and cancellation semantics. Unifying duplicate scheduler frontends is separate from retiring scheduling as a capability.

### 6. Evidence-based attention learning within existing sleep

Run attention distillation as part of the configured local-time sleep schedule. `04:00` is an initial example/default, not proof of a currently deployed timer or a hard-coded universal timezone. Use the configured agent-local day/window and existing restart/catch-up rules. No additional learning daemon or model retraining is introduced.

The proposed input window is 14 days. Distillation reads a bounded projection from existing owners:

- **Authenticated attention actions:** actor, event ID, occurrence time, agent/recipient/source identity, and action (`dismiss`, `snooze`, `mute`, `unmute`) retained by the outbox owner. Current `snoozed_until` and active mute rows are snapshots; they do not prove multiple actions or event order. Delivery acknowledgement is transport evidence, not owner engagement.
- **Candidate decisions and dispatch receipts:** provenance and occurrence time for review actions, plus the authoritative dispatch linkage from §4. A digest's `day_key` is not the time an owner approved or dismissed it.
- **Explicit owner directives:** authenticated owner provenance in the existing session record. A `user` role label alone is insufficient; peer messages, unverified device speech, assistant prose, and external content cannot authorize a rule.

Persist bounded action evidence through the owning outbox/review mutation, with a named retention window and replay deduplication. Do not build a general telemetry database. If adequate event history is absent, skip the inference or use a provably narrower signal, such as distinct authenticated dismiss receipts. Never infer repeated snoozes or the absence of intervening approval from overwritten snapshots.

Two independent consistent pushbacks within the window may propose a concise attention/triage rule, provided complete ordered evidence shows no intervening approval or engagement. Repeated delivery of one event counts once. An explicit owner directive can stage a candidate in one pass. Both paths cite their evidence and keep the recipient/agent scope; an unsupported theme generalization is not a suppression rule. Deduplicate rule proposals by evidence set and scope so the same events do not create a new candidate every night. Retention truncation makes the window incomplete; missing events and silence never prove absence of engagement.

Inferred durable preferences stay in the existing review-governed `UserModelStore` candidate flow, with at most 16 active rules under a fixed byte cap. Source mutes use `BridgeOutbox::owner_action` and remain inspectable and reversible. A direct authenticated mute/unmute command may use that existing action path; it does not require inventing a new inferred preference first. Sleep never silently edits configuration, Soul, active User Model heads, or tool grants.

Learned rules only affect discretionary proactive content. Approval prompts, `AUTH_REQUIRED`, uncertain effects (`SubmissionUnknown` or unknown delivery), estop, security/sandbox alerts, and Soul drift alerts remain outside learned suppression. Enforce this before learned rules or source-mute evaluation using event classes supplied by canonical producers, not model text or a source name alone. One delegation source can carry both routine completion and an authority-bearing event; muting the former cannot hide the latter.

### 7. Scoped services and capability-preserving slimming

| Capability | Execution owner | Boundary |
| --- | --- | --- |
| Personal productivity reads | L0 or admitted read-only L1 through deferred MCP | Agent/resource-scoped authorization and Vault references; public device privacy rules remain. |
| Personal productivity mutations | L0 through existing approval-gated MCP | Preserve `always_ask` and authenticated owner approval for sending, sharing, invitations, and overwrites. No child, peer text, cron, or sleep pass substitutes for approval. |
| External work and independent acceptance | Tachi-managed run under a scoped profile | Only task-authorized project data and credential references. No raw personal stores or ambient personal service access. A future task-specific service grant requires its own admitted contract. |
| Named-peer collaboration | Existing bodies/sessions, or the admitted Tachi run when work is delegated | Delivery, ongoing work, and new execution have distinct authorization and receipts; peer identity is preserved. |

Keep the minimal skill loader/read path and its script-execution policy. Authoring skills and managing host tunnels/backups do not return to the kernel merely to support collaboration. Shared services use deferred MCP rather than new in-process SaaS adapters.

ADR-026's external-workbench and sole-broker retirements retain their own acceptance gates. They are not prerequisites for shipping bounded L1, peer contract improvements, or continuation. Reuse completed slimming PRs and delete replaced paths in their replacement slice; do not reopen a repository-wide deletion campaign to implement this ADR.

## Implementation slices (`#374`-compatible)

These are bounded work items, not delivered features or an automatic reorder of `#374`. Related slices may stack; each implementation PR updates the current-behavior guides and records its own acceptance.

| Slice | Scope | Required evidence before delivery |
| --- | --- | --- |
| **1. Continuation correctness, then wiring** | Replace broad context collection and post-send counting in the existing resolver; wire live model resolution and gateway startup. | Origin-scope isolation, persistence failure before send, concurrent/restart attempt limits, insufficient-fact escalation, and refusal of auth/partial/protected content. |
| **2. Shared L1 loop and communication** | Keep Advisor; admit bounded Scout/Recall, deferred-tool filtering, mid-run requests/results, steering, and parent cancellation over one runner. | Confinement before/after activation, aggregate budgets, privacy inheritance, no recursion/approval cards, early findings and correction, one terminal report, zero detached child rows. |
| **3. Candidate promotion** | Stable candidate identity and versioned review writes; atomic candidate/request claims in the existing session owner; operator dispatch and rebuildable display links. | Two different request IDs racing one candidate yield one outbound start; unknown/crash/review-edit recovery; same-ID replay; marker and scope rejection. |
| **4. SSE integration** | Connect the existing observer's wake handle to the installed Tachi notification contract; retain polling. | Paired live wake-up, canonical status verification, stale/wrong-route event refusal, missed-event recovery, and terminal result digestion. |
| **5. Attention evidence and learning** | Bounded authenticated action history under existing owners, then sleep distillation and existing review activation. | Duplicate-event refusal, snapshot/ACK non-evidence, complete event order, scoped reversible rules, and non-suppressible safety events. |
| **6. Quiet sentinel recipe** | Validate read-only monitoring, host-owned finding persistence, deduplicated delivery, and separate authorized remediation. | Healthy pass creates no outbox row; no model-side writes/start; granted work can progress through the body without manufacturing approval. No repeat credit for `#782`-`#785`. |
| **7. Peer routing and lifecycle** | Preserve named peers and permission configuration; separate delivery, run continuation, and newly admitted work; replace the implicit detached launcher. | Authorized request/reply correlation, recipient isolation, bounded cycles/volume, live-scope checks, known/unknown result handling, and retained external-peer/admin behavior. |

Rollback disables the new admission or wiring while preserving existing Advisor, polling, sessions, peer authorization, and personal reminders. Persisted candidate/continuation claims, unknown outcomes, review evidence, and accepted rules survive code rollback. Older writers must not erase new fields, reset budgets, or replay unresolved work; a schema rollback requires an explicit compatible data plan. No production cutover is established by this documentation PR.

## Consequences

- The body can investigate in parallel, exchange useful interim information, collaborate with independent peers, and complete authorized work with fewer unnecessary owner interruptions.
- Tachi keeps general external execution, collaboration, and canonical run truth. The body retains personal identity, context, approvals, and outcome interpretation.
- Communication adds bounded protocol work; retained capabilities still need implementation and acceptance. Unused types are neither proof of delivery nor a reason to retire the product behavior.
- Candidate dispatch and reliable feedback learning require narrow durable metadata within existing owners. A display map or an overwritten state snapshot cannot substitute for correct claims or event evidence.
- Runtime simplification is measured by fewer duplicate paths with retained behavior and clear ownership. File/line reductions alone do not satisfy this ADR.

## Acceptance

This ADR is delivered only when the relevant implementation slices demonstrate:

1. **L1 usefulness and confinement:** two to four admitted parallel read-only workers produce concise evidence, request missing context, and accept scoped correction. Write/unknown/approval-gated tools, late activation escapes, recursion, private public-device access, and detached children are refused. Parent cancellation terminates local child work without new execution rows.
2. **Honest inference availability:** with delegation unavailable, L1 uses the currently admitted inference path without creating a Tachi task. Before broker cutover that can be the direct provider; after sole-broker cutover unavailable inference fails explicitly without fallback.
3. **Peer collaboration:** information reaches the intended peer/session; a clarification reaches an existing run; authorized new work has a correlated result. A peer cannot impersonate the owner or inherit private context. Independent peer lifetimes and supported cancellation are reported correctly.
4. **Owner workflow closure:** an authorized multi-step task can move from investigation to delegated work, technical clarification, repair, and independent verification without repeated intent questions. Missing authority, `always_ask`, auth/spend decisions, and partial/unknown outcomes still stop the affected operation. Landing requires a separately authorized actor and the repository's actual gates.
5. **Continuation and wake-up:** a live installed pair scopes facts to the delegation, reserves attempts before transmission, survives restart without duplicate continuation, verifies canonical status after SSE, and falls back to polling. Grounding failures and protected decisions reach the owner appropriately.
6. **Candidate dispatch:** approval alone starts nothing. Concurrent promotion and retries yield one canonical candidate claim and at most one transmitted start; unresolved claims cannot be bypassed with another request ID. Review edits and projection failures preserve recoverable bindings.
7. **Attention learning:** distinct authenticated events or an explicit authenticated directive produce an evidence-linked review candidate. ACKs and snapshots cannot masquerade as engagement or repeated feedback. Activation is bounded, reversible, and cannot suppress protected events.
8. **Preserved everyday capabilities:** personal commitments/reminders, scheduling, minimal skill reading, peer authorization, and direct owner mute/unmute still work. Healthy sentinel checks are quiet; external-workbench/provider retirements wait for their own replacement acceptance.

## References

- [ADR-011: Multi-agent runtime boundaries](./ADR-011-multi-agent-runtime-boundaries.md)
- [ADR-013: Channels as gateway clients](./ADR-013-channels-as-gateway-clients.md)
- [ADR-016: Growth proposed by the agent, approved by the owner](./ADR-016-growth-proposed-by-agent-approved-by-owner.md)
- [ADR-017: Personal agent body, edges, and delegation](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-018: Soul character model](./ADR-018-soul-character-model.md)
- [ADR-019: One body, many hands](./ADR-019-one-body-many-hands.md)
- [ADR-020: Durable turns, replay classes, background work, and attention](./ADR-020-durable-turns-replay-and-attention.md)
- [ADR-023: Realtime voice carrier](./ADR-023-realtime-voice-carrier-with-body-delegation.md)
- [ADR-024: Tachi inference broker](./ADR-024-bounded-tachi-provider-broker.md)
- [ADR-025: Long-context memory and delegation continuity](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md)
- [ADR-026: Headless kernel and complexity budget](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)
- [Delegation and SubAgents](../../agents/delegation.md)
- [Peer groups](../../channels/peer-groups.md)
- [MCP loading and argument recovery](../../tools/mcp.md)
- [Fork notes](../../contributing/fork-notes.md)
- Issues `#63`, `#197`, `#374`, `#381`, `#388`, `#725`, `#772`; `kckylechen1/Tachi#2003`
