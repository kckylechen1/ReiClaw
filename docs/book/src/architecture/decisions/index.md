---
type: architecture
status: accepted
last-reviewed: 2026-11-19
relates-to:
  - docs/book/src/foundations/fnd-002-documentation-standards.md
---

# Architecture Decision Records

Architecture Decision Records (ADRs) are the durable record of architecture choices that constrain future ReiClaw (ZeroClaw fork) work. They are shorter than RFCs: each one records the context, decision, consequences, and the sources that justify the decision.

Accepted ADRs are immutable. If the architecture changes, write a new ADR and mark the old one as amended or superseded rather than rewriting history.

## Core Kernel and Boundaries

| ADR | Status | Decision |
| --- | --- | --- |
| [ADR-001](./ADR-001-rust-first.md) | accepted (reinforced by [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)) | The runtime and first-party crates are implemented in Rust; under ADR-026 the kernel repository retires its embedded frontend and stays pure Rust. |
| [ADR-004](./ADR-004-tool-shared-state-ownership.md) | accepted | Tool-held shared state follows daemon-owned identity, handle ownership, isolation, and reload rules. |
| [ADR-011](./ADR-011-multi-agent-runtime-boundaries.md) | accepted (peer messaging retired by [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) and [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)) | Configured agent aliases have explicit runtime boundaries under one daemon; work delegation goes through Tachi rather than in-process peer messaging. |
| [ADR-013](./ADR-013-channels-as-gateway-clients.md) | accepted (Node deferral amended by [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md); extended to UIs by [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)) | Channels and user-facing UIs are out-of-process gateway clients; the gateway is the only external door and the runtime owns the turn lifecycle. |
| [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) | accepted (voice carrier amended by [ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md)) | One always-on body on the owner's computer; speakers, phones, and other devices are gateway Clients and Nodes; the body answers directly or reasons itself, and hands work to CLI agents only through Tachi. |
| [ADR-019](./ADR-019-one-body-many-hands.md) | accepted | One body retains Soul, approval, and safety authority; processes, crates, and traits split only for named cost-justified reasons (`>=2` real consumers/impls or hard dependency boundaries). Clients use scoped `/ws/chat`, Nodes use v2, and body-initiated MCP stays distinct. |
| [ADR-024](./ADR-024-bounded-tachi-provider-broker.md) | proposed | Tachi brokers inference through its provider/vault control plane and a narrow local data plane. Amendment 1 fixes the end state: zero model-provider families or upstream credentials remain in the body. |
| [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) | accepted | Foreground turns use a cache-pinned four-layer context stack with `sessions.db` checkpoints and drill-down tools; a confined sleep-time subagent compiles episodes into `memorycore.db` (`portable-kernel`) and a quiet Daily Digest; `rmcp` SSE wake-ups drive bounded `tachi_staff(continue)` proxy answers. |
| [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) | accepted | The ReiClaw repository is a headless pure-Rust kernel; desktop workbenches (with pluggable scenario sidebars such as Hyperion and Documents) and mobile apps live in external client repositories over `/ws/chat` and REST; configuration is file-first, retiring embedded `web/` and dynamic web-form reflection after cutover; misplaced platform features and zero-caller seams are pruned in bounded `#374` waves. |

## Soul and Character Governance

| ADR | Status | Decision |
| --- | --- | --- |
| [ADR-014](./ADR-014-reviewed-agentsoul-projection.md) | accepted (Voice precedence amended by [ADR-015](./ADR-015-one-governed-soul.md); Voice renamed Tone by [ADR-018](./ADR-018-soul-character-model.md)) | Reviewed AgentSoul uses a closed presentation-only review and projection contract. |
| [ADR-015](./ADR-015-one-governed-soul.md) | accepted (§1 and §3 amended by [ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md); layers renamed by [ADR-018](./ADR-018-soul-character-model.md)) | Soul has owner-governed layers; the model can only propose changes, and legacy personality files become seeds. |
| [ADR-016](./ADR-016-growth-proposed-by-agent-approved-by-owner.md) | accepted (layers renamed and `challenge` floor tightened by [ADR-018](./ADR-018-soul-character-model.md)) | The agent grows through bounded Story and Tone changes that it proposes and the owner approves; it reflects weekly from the owner's own messages only. |
| [ADR-018](./ADR-018-soul-character-model.md) | accepted | Each Soul layer answers one question (`Identity`, `Principles`, `Tone`, `Story`); Tone gains owner-made exemplars; proposals cannot lower `challenge` and each dial cools down for 28 days; drift is reported against an owner-set anchor. |

## Edge Devices, Voice, and Durability

| ADR | Status | Decision |
| --- | --- | --- |
| [ADR-020](./ADR-020-durable-turns-replay-and-attention.md) | proposed | Scoped request identities and conservative replay; approval evidence preserves boot-bound grants; cancellation does not imply external rollback; observations preserve provenance. |
| [ADR-021](./ADR-021-full-duplex-voice-at-the-edge.md) | proposed (front-harness restrictions superseded for carrier mode by [ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md)) | Full-duplex voice keeps audio at the edge and personal authority in the body; spoken-text projection preserves security checks and bounded listening privacy indicators. |
| [ADR-022](./ADR-022-one-small-node-many-devices.md) | proposed | Thin devices keep only bounded local audio processing with separate scoped credentials for Node, voice, and legacy links; shared-device voice never approves. |
| [ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md) | accepted | A realtime speech-to-speech service acts as a carrier: it handles small talk and public facts itself and delegates anything personal or consequential to the body over the voice surface with a projected persona and bridge-scoped credentials. |

## Amended, Superseded, and Historical ADRs

| ADR | Status | Decision |
| --- | --- | --- |
| [ADR-002](./ADR-002-trait-driven-extensibility.md) | accepted (amended by [ADR-013](./ADR-013-channels-as-gateway-clients.md), [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md), [ADR-024](./ADR-024-bounded-tachi-provider-broker.md), and [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)) | First-party in-process extension surfaces originally used broad trait contracts (`ModelProvider`, `Channel`, `Tool`, `Memory`, `MemoryStrategy`, `Observer`, `RuntimeAdapter`, `Peripheral`); obsolete and single-impl contracts are retired under ADR-019 §3 and ADR-026. |
| [ADR-003](./ADR-003-wasm-plugin-model.md) | superseded by [ADR-009](./ADR-009-wit-wasmtime-plugin-execution.md) | WASM plugins initially used Extism as the execution bridge. |
| [ADR-005](./ADR-005-pluggable-memory-backends.md) | accepted (default backend and retired backends superseded by [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) and `#745`) | Memory storage uses a backend-neutral contract; SQLite was the original default, now superseded by `memorycore` (`portable-kernel`) with SQLite, PostgreSQL, Qdrant, and Lucid retired. |
| [ADR-006](./ADR-006-runtime-channel-plugins.md) | superseded by [ADR-013](./ADR-013-channels-as-gateway-clients.md) | Runtime plugins were proposed as the target for optional channels; never shipped and superseded by out-of-process bridges. |
| [ADR-007](./ADR-007-gateway-extraction.md) | superseded by [ADR-013](./ADR-013-channels-as-gateway-clients.md) | Extracting the gateway into a separate optional process; never shipped and superseded by one always-on body process. |
| [ADR-008](./ADR-008-goal-mode-control-plane-and-usage-accounting.md) | superseded | Goal mode originally anchored on a local durable task control plane; durable task truth moved to Tachi. |
| [ADR-009](./ADR-009-wit-wasmtime-plugin-execution.md) | retired | WIT components and direct `wasmtime` replaced Extism; the WASM plugin host and `wit/` contracts were later removed in favor of MCP. |
| [ADR-010](./ADR-010-memory-authority-boundaries.md) | superseded by [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) | Originally proposed separating session history, curated memory, and enrichment authority; subsumed and implemented by ADR-025. |
| [ADR-012](./ADR-012-generation-scoped-live-config-apply.md) | proposed (narrowed by [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)) | Live config application uses generation-scoped publication and results; narrowed under ADR-026 to bounded runtime reloads without dynamic web-form reflection. |

## Notes on Pending Proposals

- ADR-006 and ADR-007 were implementation-gated roadmap decisions from [FND-002](../../foundations/fnd-002-documentation-standards.md); neither shipped and both are superseded by ADR-013.
- ADR-012 remains proposed (narrowed by ADR-026) until bounded runtime live-apply consumers meet its acceptance gates.
- ADR-014 records the accepted owner-selected minimum Slice A contract and grants no `#189`/`#190` activation.
- ADR-020, ADR-021, and ADR-022 remain proposed until the owner accepts them.
- ADR-024 remains proposed until the owner accepts the bounded broker pilot; its Amendment 1 end state and cutover/deletion gates remain separate implementation decisions.
