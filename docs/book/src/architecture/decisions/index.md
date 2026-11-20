---
type: architecture
status: accepted
last-reviewed: 2026-11-20
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
| [ADR-011](./ADR-011-multi-agent-runtime-boundaries.md) | accepted (communication clarified by [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md)) | Configured agents retain explicit identity, policy, and memory boundaries. Authorized peer communication is retained; delivery, continuation, and new-work admission have separate authority. |
| [ADR-013](./ADR-013-channels-as-gateway-clients.md) | accepted (Node deferral amended by [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md); extended to UIs by [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md)) | Channels and user-facing UIs are out-of-process gateway clients; the gateway is the only external door and the runtime owns the turn lifecycle. |
| [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md) | accepted (voice amended by [ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md); L1/L2 and peer boundaries amended by [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md)) | One personal body owns workflow closure, Clients/Nodes, and approvals; bounded read-only L1 and mid-run communication complement general external execution through Tachi and independent peer relationships. |
| [ADR-019](./ADR-019-one-body-many-hands.md) | accepted (collaboration clarified by [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md)) | The body retains Soul, approval, and safety authority. Processes/crates split for real boundaries; L1 and named peers keep distinct identities and scopes. Clients, Nodes, and body-initiated MCP remain separate contracts. |
| [ADR-024](./ADR-024-bounded-tachi-provider-broker.md) | proposed | Tachi brokers inference through its provider/vault control plane and a narrow local data plane. Amendment 1 fixes the end state: zero model-provider families or upstream credentials remain in the body. |
| [ADR-025](./ADR-025-long-context-sleep-memory-and-delegation-continuity.md) | accepted (continuity clarified by [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md)) | Existing session and memory owners support context, sleep, and review. Task-scoped continuation, live SSE, candidate promotion, and feedback learning require their named implementation and recovery acceptance; acceptance of the ADR is not delivery. |
| [ADR-026](./ADR-026-headless-rust-kernel-external-workbenches-and-complexity-budget.md) | accepted (Amendment 1 via [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md)) | Headless Rust kernel with external workbenches and file-first configuration. Remove redundant implementation while preserving peer communication, personal commitments/reminders, scheduling, and scoped skill use; workbench/broker cutovers retain their own replacement gates. |
| [ADR-027](./ADR-027-owner-workflow-closure-and-scoped-services.md) | accepted | Owner workflow closure through bounded L1 communication, independent peers, and general Tachi delegation; scoped recoverable continuation, atomic candidate dispatch, evidence-based attention learning, quiet sentinels, and approved external services. Implementation slices remain separately gated. |

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
