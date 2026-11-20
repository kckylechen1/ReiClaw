---
id: ADR-017
title: One personal agent body, edge devices, and delegation through Tachi
date: 2026-09-23
status: accepted
relates-to:
  - ADR-011
  - ADR-013
  - ADR-015
  - ADR-016
  - ADR-024
  - ADR-027
  - crates/reiclaw-gateway/src/nodes.rs
  - crates/reiclaw-runtime/src/subagent_v1/
  - crates/reiclaw-tools/src/tachi_delegation.rs
  - crates/reiclaw-api/src/tachi_staff.rs
  - crates/reiclaw-config/src/tachi.rs
---

# ADR-017: One Personal Agent Body, Edge Devices, and Delegation Through Tachi

## Status

Accepted by the owner on 2026-09-23. It restores the product direction of issues #55 (device fabric) and #198 (ZeroClaw and Tachi convergence), which the 2026-09-23 issue reset had closed, and it fits that direction to ADR-013. It amends one ADR-013 point: the Node role is no longer deferred, and `nodes.rs` is no longer deleted (§2). Where this record and those issues differ, this record wins.

Amended on 2026-09-24 to follow the direction baseline in #388: upstream reuse comes first, a replacement is proven before code is deleted, and hardware leaves the body for a Node host instead of being dropped (§2, §5, §7). The reading order for implementation work is #388, then the execution index #374, then the child issue.

Amended on 2026-11-16 by [ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md). The robot's realtime voice carrier may answer public, non-personal questions (such as the weather) itself while it speaks with the body's projected persona. Everything personal or consequential is still delegated to the body. This narrows the §3 L0 placement of spoken everyday questions and the §4 rule "the owner talks only to the body" for that carrier only.

Amended on 2026-09-26 for #381: §8 records how ZeroClaw maps onto Tachi's `tachi_staff` tool, and the Context now describes the launch path Tachi's Staff route really takes (CLI and opencode processes, not `acpx` or native ACP).

Amended on 2026-11-10, after the owner confirmed the positioning in review: the product is a framework for agents whose actions have real consequences, and the personal body is its first deployment, not its whole scope (Context, §2). The decisions in §1 to §8 are unchanged. #388 still words the direction as "not a general agent framework"; until the owner reconciles that issue, this record states the confirmed positioning and #388 keeps governing reuse, module boundaries and replace-before-delete. The contract work that follows from it is tracked in #630.

Amended on 2026-11-20 by [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md). Section 3 retains the zero-tool Advisor and adds bounded, turn-scoped read-only Scout/Recall children over the shared tool loop, including structured intermediate communication and parent steering. Peer identity and communication remain separate from this work classification. Tachi remains the general coordination and execution owner for external, cross-host, sandboxed, and durable work, including work outside software development. Sections 4 and Acceptance also distinguish delegation independence from the inference dependency accepted in [ADR-024](./ADR-024-bounded-tachi-provider-broker.md). These are implementation contracts, not a claim that the new L1 pool or peer transport has shipped.

## Context

This fork cuts upstream ZeroClaw down to a small kernel. The owner chose ZeroClaw over heavier agents such as OpenClaw because it is Rust and small: one static binary can run always-on and its device-side hosts can run on constrained hardware.

The kernel is meant to carry more than one deployment. The owner names three: a robot, a trading system, and an always-on desktop agent. They share one shape, which is what this record and its successors protect:

- the kernel sends slow intents and holds identity, memory, approvals and policy;
- any loop faster than a turn runs inside the edge, not through the kernel;
- the deterministic safety floor sits at the edge and holds without the kernel;
- a model is never in the hot path of a motor or an order.

The differentiator is this handling of consequential actions (approval, grants, unknown outcomes never replayed, edge safety floors), not breadth of channels or providers. The personal body below is the first deployment of that kernel.

The owner wants three things from that first deployment:

1. **A companion that grows** (ADR-015, ADR-016).
2. **A body that can reach devices.** It first runs on a computer. A smart speaker, a phone, or a watch then connects to it as an edge device.
3. **A manager and collaborator of other agents.** It uses bounded in-body children and authorized peers, and hands external execution to Tachi. Codex and Claude Code are examples of workers, not the limit of that collaboration.

The 2026-09-23 reset went too far on the second and third points:

- It closed #55 and #198.
- It planned to delete the Node code (`nodes.rs`) and `voice_duplex` (#375).
- It parked Tachi with a plan to delete it after M1 (#381, #382 S8).

At adoption, the code also showed a duplication:

- Tachi already drives CLI agents. Its `tachi_staff` facade (`start` / `status` / `cancel`) resolves a dispatch profile to a worker and backend, issues an execution grant, and launches the worker (`claude`, `codex`, `grok`, `kimi`, and others) as a CLI process, or through `opencode` for opencode profiles. It persists the run receipt and reaps the environment. Tachi also has `acpx` and native ACP backends, but the Staff route does not reach them: its profile resolution yields only the `cli`, `opencode_cli`, and `opencode_serve` transports.
- At adoption, ZeroClaw also had its own ACP driver for Codex (`execution_subagent`, about 10.8k lines).
- ZeroClaw also had a `tachi_bridge` port (about 4.6k lines) whose only implementations were in-memory and test ones. Section 5 records the later replacement and retirement; this historical inventory is not a list of components to restore.

## Decision

### 1. One body

ZeroClaw runs as **one always-on process on a computer the owner controls**, such as a Mac mini, a desktop, or a home server. That process holds:

- the gateway;
- the agent loop;
- the Soul and companion memory;
- the approvals;
- the scheduler.

For a workflow addressed to this body, it owns the owner's conversation, intent, approvals, and final acceptance. An independently configured peer retains its own identity and scope; it does not inherit this body's Soul or authority. Moving the body onto a constrained device is a later goal. It does not shape v1.

### 2. Edge devices are Nodes

A speaker, a phone, a watch, or another computer connects to the body **through the gateway** (ADR-013). #55 gives it two possible roles, and one device may hold both:

- **Client:** a surface that carries conversation. For example, a speaker captures voice and plays replies, and a phone shows chat and approvals.
- **Node:** a device that offers bounded, typed capabilities, such as "play audio", "turn on the light", or "read a sensor".

A Node capability call is an ordinary tool call inside a turn. It is not delegation. The Node enforces its own local permissions, and high-risk capabilities go through the approval chain.

The Node path carries intents, their results, and (once #630 adds them) device events. It is not a control loop. Anything that must react faster than a turn, such as a robot's moment-to-moment expression or an order's execution and risk checks, runs inside the edge process, bounded by that edge's own allowlist and limits (ADR-019 §4).

A small robot is a Node too. It runs a Node host on board (for example a Raspberry Pi, or a microcontroller behind a bridge) and advertises capabilities such as `drive`, `look`, `sense`, `speak`, and `emote`. Safety stays on the robot: emergency stop, speed limits, and collision checks run locally and still hold when the link to the body drops. The body never drives serial ports or GPIO in its own process. The in-process hardware crates leave the body (#382 S2). Before they are removed, the slice records their Node-side home or a pinned source reference (`robot-kit` at `e42142161`) with its tests, and it keeps the capability schemas, the admission rules, and the `node_capabilities.rs` approval table. The `robot-kit` capability set and `safety.rs` are the reference for the future robot Node.

Nodes follow the #55 design:

- signed device identity and pairing (#60);
- a headless Node host first (#61);
- an invocation lifecycle with heartbeat, pending-before-send, cancellation, and stale-capability refusal (#62).

At adoption, `nodes.rs` carried the v2 handshake and signed device identity, while turn-driven invocation was missing. The retained production v2 invocation path subsequently gained a real-process loopback host in #514, as recorded in [ADR-019](./ADR-019-one-body-many-hands.md). That delivery does not prove physical-device acceptance. The former `voice_duplex` implementation is not a reason to restore an in-process voice stack; retained voice-carrier behavior follows [ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md).

### 3. Three levels of work; peer relationships are separate

Choose a level by the authority and lifecycle the work requires. Duration and tool-call count alone do not choose it. In this table, "Tachi" means delegation through its coordination and execution surfaces; inference brokerage is a separate dependency under ADR-024.

| Level | Who does the work | Examples | Tachi |
|---|---|---|---|
| **L0 Direct** | The body's own turn: tools, MCP servers, and Node capabilities | weather, chat, reminders, calendar, memory, lights, a quick search | no delegation |
| **L1 Subagent Pool** | Bounded foreground children inside the body: zero-tool Advisor or read-only Scout/Recall, using the shared tool loop | parallel research, scoped personal recall, document comparison | no delegation |
| **L2 Delegate** | External execution managed by **Tachi**, using an admitted worker/profile and the supported execution contract | sandboxed coding, durable research or operations, cross-host work, cross-session runs | always |

Most requests end at L0. A spoken "what is the weather" needs neither a delegated job nor a subagent; its inference dependency follows §4 and ADR-024.

L1 children inherit the parent's actual admitted scope and receive only explicitly classified read tools. They cannot mutate state, prompt the owner, approve actions, recurse, or outlive the foreground turn. Public-device and session/privacy restrictions remain attached to the turn; rebuilding a child from an agent alias must not widen them. Per-child limits and the shared parent budget bound parallel work. The initial pool does not grant background cron or sleep workers a new spawn path.

Structured intermediate requests, progress, and partial findings are allowed. The parent may answer a context request, narrow the task, or stop a branch through the run's existing control path. Messages are bounded and attributed to the parent and child run; they carry evidence or instructions within the current grant, not new authority. Steering cannot reset budgets or revive a terminal run. A change requiring more authority goes through new admission. Raw intermediate tool output stays in the child, and the parent owns user-facing questions and final acceptance. None of this requires restoring the retired generic supervisor or a second task ledger.

External execution has one owner in Tachi. Short versus long and ephemeral versus durable are **options of the admitted Tachi request**, not separate local launch paths:

- A **short job** is watched live by the body, which reports the result in the same conversation.
- A **long job** lives on Tachi's board. It survives restarts, and it reaches the owner through proactive delivery (#63) when it finishes.

A request may name a registered, authorized worker or profile as a typed preference, for example "Codex implements, Claude Code reviews". Task text that mentions a harness is ordinary content, not a reason to refuse it. The command, working directory, credentials, transport, sandbox, and allowed tools still come from controlled configuration and policy, never from task text (#381).

An admitted harness may use its own native subagents inside its grant. Those substeps do not route back through the body or Tachi.

An independent peer is an identity and communication relationship under [ADR-011](./ADR-011-multi-agent-runtime-boundaries.md), not necessarily an L1 child or CLI worker. Delivering a peer message authorizes neither execution of its contents nor access to the sender's private context. Steering or continuing an existing run remains bound to that run's grant, state, and budget; starting new work requires separate admission. Cross-host coordination and external execution remain Tachi responsibilities. The current body-to-Tachi connection remains loopback-only (§8); broader collaboration does not silently relax that transport boundary.

### 4. Rules that do not change

- **The body owns its conversation.** Child and delegated-worker output is evidence, not the owner's approval. The body answers in its own voice and does not give workers its Soul. Independent peers keep their own identities; communicating with them never merges identity, memory, or authorization.
- **Fail closed, never fall back.** If Tachi delegation is unavailable, L2 returns a typed unavailable result and is never launched locally instead. L0/L1 do not require a delegated run. Before the ADR-024 inference cutover, their retained direct inference route remains available with Tachi stopped. After cutover, model-dependent L0/L1 return typed `inference-unavailable` when the broker is unavailable, while estop, local state, and authorized non-model operations remain available. No direct-provider fallback or unknown-outcome replay is introduced. The delegation implementation lives in `crates/reiclaw-tools/src/tachi_delegation.rs` and `tachi_staff.rs`, with contract tests in `crates/reiclaw-tools/tests/tachi_staff_client.rs`.
- **Tachi owns delegated run truth.** The body keeps references to Tachi runs and projections of their state, not a second execution ledger. Upstream `control_plane` is not restored beside Tachi.
- **Approvals.** Approvals inside the body are local (ADR-013). Delegated work runs under Tachi's execution grant and permission profile, and its approval requests surface in the same owner inbox. Needing an approval is no longer, by itself, a reason to choose durable execution.
- **The voice path does not wait for delegated completion.** Before a canonical accepted dispatch receipt, the body describes its intent to delegate. Only that receipt supports "I've asked Codex to do it". It may add "I'll tell you when it's done" only when background delivery and an authorized destination are configured; otherwise it explains the available status/result path. Acknowledgement, dispatch acceptance, and delivery remain separate facts ([ADR-020](./ADR-020-durable-turns-replay-and-attention.md)).

### 5. What happens to the code

The 2026-11-20 inventory below supersedes the adoption-time deletion plan. Completed retirements are not reopened to implement ADR-027.

| Code | Decision |
|---|---|
| `reiclaw-tools/src/tachi_staff.rs`, `tachi_delegation.rs` | **Keep.** The production MCP transport, request claims, and canonical Tachi receipt binding implement §8. The former runtime `tachi_bridge/` wrapper has retired. |
| `execution_subagent/`, `session_exec/`, `supervisor_v1/`, `procedure_v1/` | **Retired.** Do not restore the duplicate launcher or generic orchestration stack for L1 communication or peer collaboration. |
| `reiclaw-runtime/src/subagent_v1/`, `reiclaw-api/src/subagent_v1.rs` | **Replace in bounded slices.** Keep the live zero-tool Advisor, scoped admission, privacy boundary, lineage, budgets, and reports. Make zero-tool single-shot execution an Advisor policy while adding Scout/Recall over the shared loop. Review unused message/profile shapes against the required communication contract before deleting or replacing them. |
| `reiclaw-runtime/src/agent/sleep_compactor/` | **Reuse the common runner, preserve sleep ownership.** Extract shared loop/model assembly with its real consumers; sleep mutation tools and background lifecycle do not enter the L1 read-only registry. |
| `gateway/src/nodes.rs` | **Keep** the production v2 handshake, identity, capability admission, and invocation path. Physical-node acceptance remains device-specific. |
| Channels, extra providers, config surface, hardware crates, vendor webhooks, admin surfaces | Follow the recorded #374 and ADR-026 slice status. Remaining removals require absent consumers, explicit retirement, or a proven replacement (§7). |

Implementation status (2026-11-20): the old launchers and orchestration modules above are absent, and the bounded Advisor is live. ADR-027's L1 pool and intermediate communication remain implementation work. Source retirement does not establish real-harness, recovery, delivery, or physical-device acceptance; those outcomes still require their own recorded evidence.

### 6. Build and licensing note

As of 2026-11-20, the root `Cargo.toml` declares `MIT OR Apache-2.0` and includes `memory-core` in its default features. The memory integration is therefore part of the default build, not an opt-in-only configuration. Tachi is reached over MCP as a separate runtime.

Release decisions must use the actual dependency graph and the project's license materials. This ADR does not determine licensing obligations from either a process boundary or a feature flag.

### 7. Reuse before rewrite, replace before delete

The body keeps what makes it the owner's agent: identity, Soul and memory, conversation, everyday tools, bounded reasoning, the delegation decision, and the final report. Beneath that, it reuses existing work instead of rebuilding it.

- **Upstream is a reference, not a gate.** Consult upstream ZeroClaw or the reference projects named in #388 when they already solve the problem at hand. There is no required upstream search for every change. When adopting or adapting work, cite the source actually used and explain the local gap it addresses. Port bounded changes; never merge upstream wholesale or restore retired apps and platforms through a port.
- **Replace before delete.** A deletion slice names the real consumers, the replacement or explicit retirement, the tests that keep shared guarantees, and the rollback. Line, package, and crate counts are budgets, not goals, and they never outrank working behavior or tests.
- **Security settings never vanish silently.** Retired UI or channel settings may warn. Authentication, allowlist, credential, privacy, sandbox, and approval settings migrate or block the affected capability.
- **One conversation owner.** Each logical conversation or run has one runtime owner. WebSocket, CLI, and cron adapters call it (the RPC socket was retired in [#408](https://github.com/kckylechen1/zeroclaw/pull/408)) instead of each building their own agent, history, and approvals (#376). Disconnecting a client, cancelling the current turn, and cancelling a delegated job are separate actions.
- **Modules before processes.** A module boundary is not automatically a process boundary. The default install stays one manageable package; Tachi and any optional inference gateway or relay are installed, started, checked, and upgraded as part of it.

### 8. ZeroClaw ↔ `tachi_staff` mapping

This section records the body-to-Tachi Staff contract for L2 work (#381). The client is `TachiStaffClient` in `crates/reiclaw-tools/src/tachi_staff.rs`, with shared receipts in `reiclaw-api`; the former runtime compatibility wrapper has retired. Its start/terminal rules are checked against Tachi's golden fixture `external-staffing-contract-v1.fixture.json`; result reads and the remaining transport behavior use a scripted MCP server against the pinned source contract below. This adapter inventory does not limit Tachi's general coordination role. The bounded continuation path added under ADR-025 and its remaining production wiring are specified in ADR-027.

**Transport.** ZeroClaw calls the Tachi daemon over MCP streamable HTTP, using its own MCP transport. Each tool invocation resolves current policy and constructs a client; bounded watch reuses that client's session within its polling loop. Sessions are not cached across tool invocations. Reads may reopen a stale session once. Start and cancel are never automatically replayed after transmission. Each session sends these headers:

| Header | Value |
|---|---|
| `x-tachi-profile` | `standard` |
| `x-tachi-agent-identity` | `[tachi].agent_identity`, or `reiclaw:<agent alias>` |
| `x-tachi-client` | `reiclaw` |
| `x-tachi-project` | `[tachi].project`, only when it is set |

**Operations.**

| ZeroClaw | Tachi call | Answer |
|---|---|---|
| `start(harness, task, reason, refs)` | `tachi_staff` `action=start`, with `task`, `staffing_reason`, `profile`, and the optional `project`, `issue_ref`, `pr_ref`, and `flow_id` | a receipt: `dispatch_id`, `state` (`TASK_STATE_WORKING`), `run_dir` |
| `status(dispatch_id)` | `tachi_staff` `action=status` | the canonical `status.json`: `state`, `status_revision`, `closure_kind`, and more |
| `result(dispatch_id)` | `tachi_staff` `action=result` | canonical receipt and full UTF-8 `result.md`, bounded at 64 KiB of report bytes |
| `cancel(dispatch_id, revision)` | `tachi_staff` `action=cancel`, `expected_status_revision` | a cancellation receipt, or a typed "unavailable" |

Staff result requires Tachi containing [#2039](https://github.com/kckylechen1/Tachi/pull/2039),
merge `ae659148a5d77455a418517ee5e51a8752315fb6`; a `2.0.0` version string alone
is insufficient. Older peers return the existing typed refusal, with no Task
fallback. The public `RunResult.task_state` changes from `RunState` to
`Option<RunState>`; its JSON key, `state_basis` and `managed_run` remain present
as `null` on this path. They are absent Task projections, never aliases for
canonical state. The tool's `result.status.read_projection` preserves Staff
recovery facts from the same response as `result.state` and `result.status`.
An explicit null report body means no report is available; an empty text body
remains an empty report. Neither implies a different execution outcome. Missing
required response fields or a body above 64 KiB fails as a protocol error.

A run is finished when its state is `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, or `TASK_STATE_CANCELED`, or when it is `TASK_STATE_INPUT_REQUIRED` with `closure_kind = "partial"`.

The client reads only the fields it uses and ignores the rest. Tachi refuses unknown request fields, so the client sends only fields that `TachiStaffParams` declares. It never sends `worker`, a command, a working directory, credentials, or a sandbox. Tachi resolves all of those from the profile.

**Harness to profile.** The body asks for a harness by name, for example `codex`. `[tachi.harnesses]` maps each name to a Tachi dispatch profile, for example `codex = "codex_55_review"`. A name that is not listed is refused before Tachi is contacted. The list is empty by default, so no harness is available until the owner adds one.

**Staffing reason.** Tachi refuses a start without a reason. When the owner asked for the delegation, the reason is `explicit_user_request`. Otherwise the model picks the true reason from Tachi's list: `durable_cross_session`, `cross_device_remote`, or `native_subagent_unavailable`. The client never picks a default reason.

**Polling.** The pinned Staff transport uses `status` polling. `[tachi].poll_secs` controls bounded `tachi_watch` polling, 15 seconds by default. The gateway also runs the configured delegation-notification observer on a 60-second sweep, feeding the existing `BridgeOutbox`; local request-to-dispatch bindings remain in the existing session store, never a second run ledger. Automatic grounded continuation and event-driven wake-ups are separate ADR-027 wiring work. An event hint never substitutes for reading canonical status.

**Cancel.** Tachi can cancel only runs that it manages itself on the same daemon (`managed_custom`). For a CLI run, the answer is `CancelUnsupported`, and the body tells the owner plainly that the run cannot be stopped from here. A cancel quotes the `status_revision` from the last status read. A stale revision is reported as `StaleRevision`, and the body reads the status again.

**Same machine only.** `[tachi].endpoint` must use `http://` with a loopback host (`127.0.0.0/8`, `::1`, or `localhost`). Otherwise validation fails. Tachi's HTTP MCP endpoint does not authenticate callers, so v1 has no setting to allow a remote Tachi. A later change can allow one after Tachi adds caller authentication.

**Fail closed.** `[tachi]` is disabled by default. If it is disabled or invalid, if the daemon is down, or if the transport fails, the answer is `Unavailable`, and nothing runs locally instead (§4). Other errors are typed too:

- `Refused`: Tachi rejected the request (a tool error or a JSON-RPC error).
- `UnknownHarness`: the harness name is not in `[tachi.harnesses]`.
- `CancelUnsupported`: Tachi cannot cancel the run.
- `StaleRevision`: the cancel quoted an old `status_revision`.
- `Protocol`: the response could not be read.
- `SubmissionUnknown`: a start may have been accepted, but no trustworthy receipt was received; never retry it automatically.

**Task text.** A task that mentions a harness, vendor, or model is sent as written (§3). The TaskIntent composer no longer refuses vendor names. It still refuses text that tries to choose where or how the work runs, such as a working directory, a worktree, tmux, SSH, a sandbox, or CLI flags.

**Worker context.** Task text, staffing reason, and references leave the body with protocol caller identity and configured profile/project routing metadata. The body never copies Soul or User Model bytes into a start (ADR-014, ADR-015, ADR-016). A worker gets the task without the owner's persona identity or personal profile. When a task needs a fact about the owner, the body states that one fact in the task text.

**Gaps tracked in Tachi** ([kckylechen1/Tachi#2003](https://github.com/kckylechen1/Tachi/issues/2003)):

- Tachi cannot cancel CLI-backed Staff runs.
- There is no event stream, so the body has to poll.
- The HTTP endpoint does not authenticate callers.
- A start has no idempotency key. If the connection drops after the request is sent, the body cannot tell whether the run started. A transmitted start with no trustworthy receipt reports `SubmissionUnknown` and is never replayed, including after a stale-session response. The production tools persist the local request claim before sending it; an unresolved claim blocks automatic resubmission after reconnect or restart. This prevents replay but does not recover the missing dispatch identity.

## Consequences

Positive consequences:

- One CLI-driver implementation, in Tachi. Supporting a new agent is a Tachi change only.
- ZeroClaw gets smaller: about 10.8k lines of ACP driver are replaced by a thin MCP-backed bridge.
- Everyday use (L0 and L1) starts no Tachi delegation; its model availability follows the ADR-024 inference-cutover contract.
- The speaker, phone, and future devices share one gateway and one identity.

Negative consequences:

- Delegation requires the configured Tachi daemon; the current body-to-Tachi transport is loopback-only even when Tachi coordinates work elsewhere.
- Two repositories must agree on the `tachi_staff` wire contract. A change there is a cross-repository change.
- The Node work (#60–#62) returns to the plan, and it is not small.

## Acceptance

This ADR is implemented when:

1. before ADR-024 broker cutover, a spoken or typed L0 request (for example, weather) completes with Tachi stopped; after cutover, stopping Tachi yields typed `inference-unavailable` for model-dependent L0/L1 while estop, local state, and authorized non-model operations remain available, with no direct-provider fallback;
2. a delegation request with Tachi stopped returns a typed "unavailable" answer and runs nothing locally (test);
3. `TachiStaffClient` provides the production MCP transport, and one real Codex or Claude Code job runs end to end: started by the body, tracked, and reported in the body's voice;
4. `execution_subagent` is deleted;
5. one headless Node pairs, advertises a capability, is invoked by a turn, disconnects, reconnects, and is revoked (#61, #62).

The whole-package acceptance (two harnesses, one Node, shared conversation across clients) is tracked in #374. The 2026-11-20 L1 and peer amendments additionally use ADR-027's scoped-read, intermediate-communication, cancellation, and message-versus-execution acceptance; they are not satisfied by the existing zero-tool Advisor alone.

## References

- Issue #55: device fabric (Channel / Client / Node / Gateway)
- Issue #198: ZeroClaw and Tachi convergence (Reason / Ephemeral / Durable)
- Issues #60, #61, #62: Node identity, headless host, invocation lifecycle
- Issue #63: attention and proactive delivery
- Issue #381: Tachi integration
- Issue #382: reuse and replacement-first deletion
- Issue #388: direction baseline (reuse, module boundaries, replace before delete)
- [ADR-013: Channels as gateway clients](./ADR-013-channels-as-gateway-clients.md)
- [ADR-011: Configured agent identities and peer boundaries](./ADR-011-multi-agent-runtime-boundaries.md)
- [ADR-024: Inference brokerage and availability after cutover](./ADR-024-bounded-tachi-provider-broker.md)
- [ADR-027: Owner workflows and scoped collaboration](./ADR-027-owner-workflow-closure-and-scoped-services.md)
- Tachi `crates/tachi-server/src/tools/workflow_facade.rs` (`tachi_staff` handler), `crates/tachi-server/src/staffing_ops/`, and `crates/tachi-params/src/facade/orchestration.rs` (`TachiStaffParams`)
- Tachi `docs/engineering/architecture/external-staffing-contract-v1.fixture.json` (golden Staff contract)
- [kckylechen1/Tachi#2003](https://github.com/kckylechen1/Tachi/issues/2003): cancel of CLI runs, events, results on the staff facade, and caller authentication

### Production capability pin and reuse (#381 B1)

The tool mapping above was checked against Tachi `7873b720993cc10519db6232863f4db965c657f2`:
`TachiStaffParams` in `tachi-params/src/facade/orchestration.rs`, Staff handlers in
`tachi-server/src/staffing_ops/` and `tools/workflow_facade.rs`, result reads in
`tools/task_facade.rs`, and the profile catalog in `tachi-dispatch/src/profiles.rs`.
Start, canonical status, and revision-bound managed-custom cancel are real
operations. Staff has no watch stream or durable caller request key. At that pin,
result text was read through Task and its inferred state was kept separate from
canonical `run_status`. Status and bounded watch retain the same-response
`read_projection`, including orphaned execution, unavailable control, and unknown
outcome after a daemon
restart. Those facts never rewrite canonical state into a terminal outcome.
CLI cancellation and lost-submit identity reconciliation remain unsupported.
The newer `preflight` action is advisory and is not execution/authentication proof.

DSH has no admitted worker/profile in that Tachi pin. The DeepSeek explorer
profile runs OpenCode and does not prove DSH support. The
[official DSH CLI reference](https://github.com/deepseek-ai/deepseek-harness/blob/master/apps/cli/README.md)
provides headless and ACP profile carriers; selecting/admitting one belongs to
Tachi, not a second launcher in this body.

The result adapter was subsequently checked against Tachi
`b3d82a0287b52b793876ab5bc883741feea66adf`, including #2039:
[`workflow_facade.rs`](https://github.com/kckylechen1/Tachi/blob/b3d82a0287b52b793876ab5bc883741feea66adf/crates/tachi-server/src/tools/workflow_facade.rs#L90)
reads one canonical receipt and its `read_projection`, then attaches the report;
[`task_facade.rs`](https://github.com/kckylechen1/Tachi/blob/b3d82a0287b52b793876ab5bc883741feea66adf/crates/tachi-server/src/tools/task_facade.rs#L104)
owns the shared bounded report reader and Unicode/missing/size assertions.
The exact Staff action inventory is frozen in
[`schema.rs`](https://github.com/kckylechen1/Tachi/blob/b3d82a0287b52b793876ab5bc883741feea66adf/crates/tachi-contract-tests/src/tests/facade_tests/schema.rs#L342).
The existing external-staffing fixture is unchanged; it does not freeze the full
result JSON schema. The adapter reads the top-level receipt, not a nested Task
`run_status`; report text remains separate from receipt-owned `receipt_result`
metadata, which is outside the fields this client projects.

Reuse decision: reuse this fork's existing Staff MCP transport, its pinned
golden contract and the existing session SQLite owner. Upstream ZeroClaw native
CLI adapter/process code (checked at
`329e66f76394b00af9d00b767e00ad298133961e`) and Cindy maker-core adapters are not
imported into this slice: launch ownership stays on Tachi and no local adapter is
needed for the body's protocol surface. The original local multi-harness launch proposal
(#433) was closed. No external source code or dependency is added here. Real
harness execution, result delivery and backend recovery still need their own
end-to-end evidence before #381 can close; the source retirements recorded in §5
do not stand in for that evidence.

See [Delegation and SubAgents](../../agents/delegation.md#external-work-through-tachi)
for production request IDs, polling limits and rollback behavior. To roll back
this slice, disable `[tachi]`; retain request bindings to avoid forgetting an
unresolved submission. Disabling delegation does not switch inference routes;
L0/L1 availability follows the installed ADR-024 phase in §4.
