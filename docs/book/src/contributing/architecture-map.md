# Architecture and Contribution Map

Use this page when a change is larger than a typo and you are not sure which architecture, foundation, contributor, or maintainer documents apply.

This page is only a map. The linked files remain the source of truth.

## Start Here

1. Read the repo-root `AGENTS.md` first. It contains the compact, always-loaded safety and contribution contract.
2. Use the tables below to choose the architecture and foundation documents that match the change.
3. If the change crosses subsystem, config, security, or release boundaries, record the decision as an ADR (`docs/book/src/architecture/decisions/`) before implementing.

## Common Change Paths

| Change | Read first | Why |
|---|---|---|
| New provider | [Architecture overview](../architecture/overview.md), [Crates](../architecture/crates.md), [Custom providers](../providers/custom.md), [Provider configuration](../providers/configuration.md) | Providers are edge adapters behind the provider trait, with config, factory, attribution, and routing contracts. |
| New channel | [Architecture overview](../architecture/overview.md), [Crates](../architecture/crates.md), [Channel runtime lifecycle](../architecture/channel-runtime-lifecycle.md), [Channels overview](../channels/overview.md), the Telegram bridge in `bridges/telegram/` as the reference implementation (`zeroclaw-channels` is retired, #579) | Channels are user-visible trust boundaries; validate inbound, outbound, pairing, authorization, dispatch, and reply lifecycle behavior. |
| Channel dispatch, webhook ingress, reply intent, streaming drafts, listener lifecycle, or channel reload behavior | [Channel runtime lifecycle](../architecture/channel-runtime-lifecycle.md), [Request lifecycle](../architecture/request-lifecycle.md), [Gateway HTTP API](../gateway/api.md), [Testing](./testing.md) | Channel lifecycle changes need a single dispatch and turn path instead of one-off adapter or gateway mini-orchestrators. |
| New built-in tool or tool policy | [Tools overview](../tools/overview.md), [Built-In Tool Inventory](../developing/tool-inventory.md), [Tool execution lifecycle](../architecture/tool-execution-lifecycle.md), [ADR-004: Tool shared state ownership](../architecture/decisions/ADR-004-tool-shared-state-ownership.md), [Security overview](../security/overview.md), [Tool receipts](../security/tool-receipts.md) | Tools execute actions for the agent. First check whether the capability belongs in core, then validate registration, approval, dispatch, audit, receipts, localization, attribution, and shared-state ownership. |
| Dependency footprint or feature-profile change | [Crates](../architecture/crates.md), [Core-residency rationale](./core-residency-rationale.md), [CI & Actions](../maintainers/ci-and-actions.md) | Keep package-graph measurements separate from tool visibility and runtime costs; use root-scoped normal/build closure and preserve default compatibility. |
| Runtime, agent loop, state, provider token streaming, or tool-loop behavior | [Request lifecycle](../architecture/request-lifecycle.md), [Runtime state and persistence](../architecture/runtime-state-and-persistence.md), [Tool execution lifecycle](../architecture/tool-execution-lifecycle.md), [Crates](../architecture/crates.md), [FND-001](../foundations/fnd-001-intentional-architecture.md), [Testing](./testing.md) | Runtime changes often affect multiple user paths and need boundary-level tests. Provider token streaming stays runtime-owned; channel draft or typing-indicator streaming follows the channel lifecycle row. Tool-loop changes should name whether they affect approval, dispatch, receipts, observer events, history, or cancellation. |
| Cron, SOP, delegation, subagent, goal-mode, waiting, cancellation, or restart-recovery behavior | [Background work lifecycle](../architecture/background-work-lifecycle.md), [Delegation & SubAgents](../agents/delegation.md), [Testing](./testing.md) | Background execution is not one lifecycle. Name the current owner and status surfaces, distinguish durable records from restart-resumable work, and verify cancellation and recovery at the changed boundary. |
| Memory, session history, prompt context, tool results, files/media payloads, or context trimming | [Memory and payload lifecycle](../architecture/memory-payload-lifecycle.md), [Runtime state and persistence](../architecture/runtime-state-and-persistence.md), [History management](../agents/history-management.md), [Runtime internals](../agents/internals.md), [Testing](./testing.md) | Payload changes need clear owner, scope, durability, privacy, and truncation boundaries. |
| Gateway, web API, or dashboard behavior | [Gateway HTTP API](../gateway/api.md), [Building the web dashboard](../developing/web.md), [Request lifecycle](../architecture/request-lifecycle.md), [Security overview](../security/overview.md) | Gateway changes can affect auth, public exposure, generated API contracts, dashboard consumers, and review risk. Use the channel lifecycle row for webhook dispatch or reply behavior. |
| User-visible behavior or validation evidence for a command, terminal, daemon, browser, channel, provider, tool, background job, or install path | [User-boundary proof](./user-boundary-proof.md), [Testing](./testing.md), and the architecture or feature document for the changed surface | Match each behavior claim to the smallest credible proof that reaches the boundary where a user observes it. Add manual or environment-specific evidence only for a named gap in automated coverage. |
| Config schema, environment variables, defaults, or reload behavior | [Config lifecycle](../architecture/config-lifecycle.md), [Environment variables](../reference/env-vars.md), [Runtime state and persistence](../architecture/runtime-state-and-persistence.md), [Provider configuration](../providers/configuration.md), [FND-001](../foundations/fnd-001-intentional-architecture.md) | Config changes affect upgrade paths, reload behavior, source-of-truth boundaries, and may require migration or RFC discussion. |
| Generated references, mdBook preprocessors, docs snippets, or docs deployment | [Generated documentation pipeline](../architecture/generated-documentation-pipeline.md), [Building the docs locally](../developing/building-docs.md), and [Config lifecycle](../architecture/config-lifecycle.md) when config references change | Name the canonical source, materializer, tracked or build-only output, consumer, and drift check. |
| Docs gettext catalogs, locale registry, or catalog release pins | [Localization catalog lifecycle](../architecture/localization-catalog-lifecycle.md), [Docs & Translations](../maintainers/docs-and-translations.md), [Generated documentation pipeline](../architecture/generated-documentation-pipeline.md) | A catalog present in the repository is not proof that a site build consumes it. Verify the materializer or pin path. |
| CI, release, GitHub Actions, or allowed actions | [CI & Actions](../maintainers/ci-and-actions.md), [FND-004](../foundations/fnd-004-engineering-infrastructure.md) | Infrastructure changes are high-risk when they alter what code can run or ship. |
| Docs structure, contributor guidance, or knowledge organization | [FND-002](../foundations/fnd-002-documentation-standards.md), [Docs & Translations](../maintainers/docs-and-translations.md), this page | Documentation changes should reduce search cost and preserve the decision trail. |
| Production code health, error handling, or dead-code cleanup | [FND-006](../foundations/fnd-006-zero-compromise-in-practice.md), [Testing](./testing.md), repo-root `AGENTS.md` | Error discipline, unused code, and production readiness are review gates, not style preferences. |

## Foundation Documents In One Screen

| Foundation | Read when the change asks... |
|---|---|
| [FND-001: Intentional architecture](../foundations/fnd-001-intentional-architecture.md) | Does this fit the microkernel/runtime direction? Which layer should own it? |
| [FND-002: Documentation standards](../foundations/fnd-002-documentation-standards.md) | Where should knowledge live? How should docs stay navigable and durable? |
| [FND-004: Engineering infrastructure](../foundations/fnd-004-engineering-infrastructure.md) | How should CI, release automation, or GitHub Actions behave? |
| [FND-006: Zero compromise in practice](../foundations/fnd-006-zero-compromise-in-practice.md) | What quality bar applies to production code, errors, dead code, and release readiness? |

## Coding Agent Entry Points

Coding agents should use the same public docs as humans, plus the repository-local agent contracts.

- Follow the repo-root `AGENTS.md`. Inspect `.claude/skills/*/SKILL.md` and use the matching in-repo skill when one applies; the skill file is authoritative.
- Treat foundation documents as decision context. They explain why a review may ask for a split, an ADR, stronger validation, or a different owner.
- Keep private workflow mechanics out of public PR bodies, issue comments, and reviews. Public text should cite concrete behavior, source paths, commands, validation evidence, linked issues, and user-visible risk.
- If a generated or skill-authored draft conflicts with source code, current `AGENTS.md`, or a ratified foundation document, stop and reconcile before posting or implementing.
