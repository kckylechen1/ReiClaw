---
id: ADR-011
title: Configured agents have explicit runtime boundaries under one daemon
date: 2026-07-19
status: accepted
relates-to:
  - https://github.com/zeroclaw-labs/zeroclaw/issues/5890
  - https://github.com/zeroclaw-labs/zeroclaw/pull/6398
  - ADR-005
  - ADR-010
  - ADR-017
  - ADR-026
  - ADR-027
  - docs/book/src/agents/overview.md
  - docs/book/src/agents/internals.md
  - docs/book/src/channels/peer-groups.md
---

# ADR-011: Configured Agents Have Explicit Runtime Boundaries Under One Daemon

This is a retroactive record of the multi-agent architecture shipped through [#6398](https://github.com/zeroclaw-labs/zeroclaw/pull/6398) in v0.8.0-beta-1. The date above is the date this ADR was added, not the original implementation date.

Accepted RFC [#5890](https://github.com/zeroclaw-labs/zeroclaw/issues/5890) proposed a broader multi-agent UX and configuration model. This record captures five durable boundaries that shipped: named identities, one supervising daemon, explicit channel relationships, agent-scoped memory, and per-agent runtime-policy and workspace scope. It does not ratify the RFC's unshipped alias-only join requirement, `memory_namespaces` schema, bundle taxonomy, swarm shape, or deferred dashboard and observability contracts.

Amended on 2026-11-20 by [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md): peer collaboration and its current peer-group/channel authorization contracts remain product capabilities. A peer is an independently addressed identity with an explicit communication relationship; it need not be an L1 child or a CLI worker. Message delivery, continuation of an existing run, and admission of new work have separate authority checks. The current internal peer path's implicit detached-launch coupling awaits replacement; `send_message_to_peer` and `peer_groups` have not retired. The old channel orchestrator retired separately under [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md). This amendment does not restore it or claim that the replacement peer execution path has shipped.

## Context

ZeroClaw originally centered configuration and runtime behavior on one implicit agent. Multi-agent operation requires identities that can coexist without sharing workspace, memory, permissions, or channel reachability by accident.

The V3 implementation replaced the implicit singleton with configured agents keyed by alias. Each agent resolves its own runtime inputs and scoped boundaries while one daemon coordinates the enabled set. Explicit grants connect agents where collaboration is intended.

## Decision

### Agent identity and supervision

Each `[agents.<alias>]` entry defines an addressable agent identity. The alias selects the configured agent at CLI, channel, daemon, and other runtime entry points. A single-agent install uses the same model with one configured entry; there is no separate privileged singleton architecture.

Each agent has its own workspace boundary and identity source. Identity files are resolved for that agent rather than from one install-wide personality.

One `zeroclaw daemon` process supervises the enabled configured agents and their channel bindings. This does not mean each agent owns an always-running process or loop. It means daemon operation starts and coordinates the configured set rather than requiring one daemon per identity.

### Explicit communication

The durable rule is that authorized communication relationships are explicit. Co-residence in one daemon, use of the same provider, and access to the same MCP server do not make agents peers or grant access to each other's sessions, memory, or tools.

A peer relationship names the sender, recipient, permitted conversation or run, and accepted message scope. Each peer retains its own identity and policy. The L0/L1/L2 split in [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) classifies how work executes; it does not classify every communicating identity. A bounded L1 child shares its parent turn's lifetime, while an independent peer can outlive that turn.

| Operation | Required boundary |
| --- | --- |
| Deliver a question, finding, or progress update | Authenticate the sender and authorize the recipient and message scope. Delivery is evidence, not a grant to execute its contents. |
| Steer or continue an existing run | Bind the message to that run and its current grant, state, and budget. A stale or terminal run cannot be revived by message delivery. |
| Start new work | Use the target runtime's admission path. External execution and cross-host coordination belong to Tachi; a message alone cannot launch a worker or widen a grant. |

Current peer groups and channel authorization retain routing, inbound acceptance, external-peer, and administration consumers. Preserve those permission contracts and their configuration while replacing the internal message path's implicit detached-launch coupling. Retirement of the old in-process channel orchestrator does not retire `peer_groups` or the retained peer paths. Any replacement must cover those consumers and enforce the boundaries above before activation. Tachi provides general cross-host coordination and execution; its use does not turn every peer into a newly spawned CLI worker.

For L1 children, ADR-027 allows bounded intermediate requests, progress or partial findings, and parent steering within the original turn grant. These messages do not grant recursion, direct user contact, approvals, or durable background execution. They use the parent run's existing ownership rather than a second supervisor or task ledger.

### Workspace and runtime-policy scope

Each enabled agent resolves a risk profile and a workspace boundary. The default filesystem boundary is the agent's own workspace. An access entry on the addressed agent explicitly grants that agent access to the named sibling workspace.

The workspace-only jail can be disabled by the resolved risk profile, including full autonomy, or by the agent's unrestricted-filesystem setting. Remaining forbidden-path policy and host permissions still apply.

Sharing a provider, channel adapter, bundle, or other referenced configuration does not merge the agents' workspace or resolved runtime-policy scope. It may intentionally share the referenced resource or credentials: secrets remain install-wide rather than per-agent.

### Memory isolation

The configured agent identity is the scope consumed by the memory contract. Backends enforce that scope through stored agent metadata or an agent-owned storage boundary and scoped adapter.

Cross-agent recall is additive and explicit. It requires compatible same-backend access; the runtime does not silently bridge different backend kinds.

At adoption, [ADR-005](./ADR-005-pluggable-memory-backends.md) owned storage and cross-agent recall constraints, and [ADR-010](./ADR-010-memory-authority-boundaries.md) assigned memory authority. [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) now governs the retained session and memory stores. A peer message or L1 relationship never grants cross-agent recall by itself; any shared content is a scoped projection admitted by its owner.

### Schema evolution boundary

The durable decision is the named identity and explicit boundary model above. Exact config references, bundle aliases, and filesystem fields may evolve without superseding this ADR as long as those boundaries remain intact.

## Consequences

Positive consequences:

- Single-agent and multi-agent installs share one runtime model.
- Operators can reason about workspace, memory, runtime policy, and channel reachability per named agent.
- Sharing is visible through configuration rather than inferred from process co-residence.
- Cross-agent collaboration can be added selectively without weakening the default scoped boundaries.

Negative consequences:

- Every agent-aware entry point must carry or resolve an agent alias correctly.
- Configuration validation must reject dangling aliases and incompatible cross-agent grants before runtime use.
- Shared install-wide backends and adapters need agent attribution and filtering rather than relying on separate processes for scope.
- Secrets remain installation-wide, so per-agent workspace and runtime-policy scope does not imply per-agent credential isolation.

Follow-up decisions:

- New cross-agent capabilities must define their own authorization and attribution boundary rather than assuming peer-group or daemon membership is sufficient.
- Changes that weaken default workspace, risk-profile, or memory scope require an explicit architecture and security decision.

## References

- [RFC #5890: Multi-agent UX flow](https://github.com/zeroclaw-labs/zeroclaw/issues/5890)
- [PR #6398: Multi-agent V3 runtime and schema](https://github.com/zeroclaw-labs/zeroclaw/pull/6398)
- [Agents](../../agents/overview.md)
- [Running agents](../../agents/operating.md)
- [Runtime internals](../../agents/internals.md)
- [Peer groups](../../channels/peer-groups.md)
- [ADR-017: Body, edges, and work levels](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-027: Owner workflows and scoped collaboration](./ADR-027-owner-workflow-closure-and-scoped-services.md)
- `crates/zeroclaw-config/src/schema.rs`
- `crates/zeroclaw-config/src/multi_agent.rs`
- `crates/zeroclaw-policy/src/lib.rs`
- `crates/zeroclaw-channels/src/orchestrator/mod.rs`
- `crates/zeroclaw-memory/src/agent_scoped.rs`
