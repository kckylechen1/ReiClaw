---
id: ADR-024
title: Bounded Tachi provider broker for inference
date: 2026-11-17
status: proposed
relates-to:
  - ADR-012
  - ADR-017
  - ADR-019
  - ADR-020
  - ADR-023
  - crates/zeroclaw-api/src/model_provider.rs
  - crates/zeroclaw-config/src/secrets.rs
  - crates/zeroclaw-providers/src/factory.rs
  - crates/zeroclaw-providers/src/reliable.rs
  - crates/zeroclaw-runtime/src/agent/turn/provider_call.rs
  - src/main.rs
---

# ADR-024: Bounded Tachi Provider Broker for Inference

## Status

Proposed on 2026-11-17. This record does not change runtime behavior, activate a provider, amend an accepted ADR, delete code, or reorder the execution queues by itself.

Owner acceptance would approve the **bounded pilot** and its ownership boundaries, not a universal provider gateway or a commitment to migrate every provider and OAuth flow. Each implementation slice remains subject to #374 dispatch, its named issue, and the repository's replace-before-delete rules.

## Context

ZeroClaw is becoming a personal controller: the body owns identity, sessions, approvals, tools, and device authority, while Tachi owns delegated harness execution. The body's memory backend already links Tachi's portable kernel as a library and opens a ZeroClaw-owned database. L2 delegation calls the separately running Tachi daemon over MCP.

Model-provider ownership is still much wider. ZeroClaw carries dozens of provider families, several wire protocols, credential resolution, OAuth profiles, retries, routing, pricing, and model metadata. The owner wants one operational place to select models, rotate credentials, and manage OAuth, with a correspondingly smaller body.

Tachi already has useful control-plane pieces: provider profiles, catalog and alias resolution, revision-checked plan/apply, vault-backed credential material, health, and runtime publication. Those pieces do not constitute a complete chat data plane. Existing Tachi lane calls are shaped around bounded internal consumers rather than ZeroClaw's full conversation, streaming, tool, cancellation, and attribution contracts.

There is also a negative precedent. Tachi's bounded provider-adapter/broker PR [#1770](https://github.com/kckylechen1/Tachi/pull/1770) closed without merging; [#1776](https://github.com/kckylechen1/Tachi/pull/1776) retired the unused universal model gateway, deleting 15,965 lines across 133 files. Issue [#1682](https://github.com/kckylechen1/Tachi/issues/1682) was an explicit owner withdrawal of universal wire ownership. OAuth issue [#1684](https://github.com/kckylechen1/Tachi/issues/1684) additionally lacked both a named production consumer and a supported provider contract.

ZeroClaw can now be that real consumer. That justifies a bounded replacement experiment, not restoration of the retired platform.

## Decision

### 1. Build one consumer-driven broker path, not a universal gateway

Tachi may become the inference broker for explicitly admitted ZeroClaw production paths. The first pilot contains exactly one effective live path, one upstream wire dialect, and API-key authentication. Its purpose is to prove replacement and remove concrete ZeroClaw provider/authentication code after parity.

The first pilot does not include:

- all providers, arbitrary OpenAI-compatible endpoints, or provider-count targets;
- a universal Anthropic/Gemini/OpenAI translation layer;
- OAuth, email OAuth, TTS/STT, embeddings, multimodal, or realtime voice;
- forced migration of other harnesses or editor clients to Tachi inference;
- repository or process unification;
- automatic direct-provider fallback.

If weekly model selection were the only requirement, the existing TOML/provider path would be sufficient. The broker is justified only by a demonstrated reduction in runtime ownership and retained ZeroClaw code.

### 2. Keep the repositories, processes, and stores separate

ZeroClaw and Tachi remain separate processes with independent failure and upgrade boundaries. A single install/status/upgrade experience may present them as one product, as ADR-017 already requires, but one product does not require one PID or one Git repository.

The pilot creates no new durable store. Provider profiles, revisions, credentials, health, and audit ownership resolve into the existing Tachi catalog/vault/control-plane surfaces rather than a parallel broker database.

The Vault store and its cryptographic/format crate are not moved into ZeroClaw. `vault-kit` does not provide the store, lease, pool, health, or client contract. If subprocess credential resolution later justifies shared code, extract a narrow, supported client for lease/resolve/status only; do not copy store ownership or expose unrestricted raw retrieval.

### 3. Divide authority explicitly

| Owner | Authority |
| --- | --- |
| ZeroClaw body | Soul and User Model, conversation and session ownership, agent/purpose selection, permitted data egress, tool and Node approval, execution, local budgets, emergency stop, and final owner-facing answers |
| Tachi broker | Admitted provider profiles, upstream endpoints, credential selection, upstream connection, request-scoped retry/fallback within the admitted candidate set, upstream attempts, served identity, usage, and pricing evidence |
| Shared versioned contract | Alias admission, capabilities, route binding, invocation/attempt IDs, cancellation, terminal outcomes, receipts, and compatibility revisions |

ZeroClaw decides **which agent and purpose may send which data**. Tachi selects deployment and credential only inside the resulting authorized candidate set. Neither side silently duplicates the other's routing policy.

### 4. Freeze the effective path before designing the wire

The name `openai.default` does not identify a sufficient contract. Current OpenAI construction can select Codex OAuth/Responses, API-key Responses, or legacy Chat Completions depending on configuration (`crates/zeroclaw-providers/src/factory.rs`).

Slice 0 records, without exposing credentials:

- configured and effective provider kind, wire API, and authentication mode;
- requested and actual recipient/model;
- streaming and non-streaming consumers;
- tools, reasoning/history extensions, cancellation, and fallback behavior;
- context/output limits, usage, pricing, and error requirements;
- every retained caller that the cutover would affect.

If that inventory cannot determine one bounded path, the pilot stops before broker implementation.

### 5. Use a narrow versioned admission and invocation contract

The broker is not an open HTTP forward proxy. Its minimum contract has two stages:

1. **Resolve/admit** an authorized logical alias and purpose. Return an opaque, immutable route binding containing the admitted revision, actual recipient/model, capabilities, limits, and pricing reference. Unknown aliases, unsupported capabilities, and unauthorized recipients fail before any upstream send.
2. **Invoke** with a request ID, route binding, conversation/turn correlation, input/history, tool schemas and results, sampling/output limits, a spend allowance covering all attempts, and a deadline. The caller cannot supply an arbitrary upstream URL, credential reference, or authentication header.

ZeroClaw's invocation ID is unique within an authenticated local-principal, agent, conversation, and purpose scope. The route binding belongs to that same scope and is immutably associated with both the admitted route and a canonical request-payload hash. Invoke, receipt read, and cancellation all verify current authorization within that scope.

A same-scope duplicate invocation ID with the same payload returns the existing in-flight state or receipt without another upstream send. The same ID with a changed payload is a conflict; cross-scope use is denied. Tachi assigns attempt IDs within that invocation. Handles are epoch-scoped and expire; after a broker restart, stale handles fail closed. A missing receipt never proves that a request was not sent. These rules require no persistent exactly-once queue.

The selected upstream dialect's payload and streaming events may be preserved where that is the smallest correct implementation. The local admission/metadata contract still requires explicit versioning and capability declaration.

Tachi does not execute ZeroClaw tools. Tool calls remain response data; the body retains parameter validation, approval, and execution.

### 6. Add request-scoped receipts; never put route truth in global provider state

Every invocation and upstream attempt carries an invocation/attempt identity. A receipt distinguishes:

- requested logical alias and served provider/model;
- admitted route and policy/pricing revisions;
- usage and terminal outcome;
- an opaque credential-version handle in protected audit only.

Current `ChatResponse` and `StreamEvent` shapes do not carry all of this route/attempt metadata (`crates/zeroclaw-api/src/model_provider.rs`). The ZeroClaw slot therefore includes a request-scoped receipt extension rather than a mutable "last route" field shared by concurrent requests.

Public session/UI receipts contain real provider/model, request ID, usage, and outcome. Vault names, key suffixes, token hashes, and other credential identifiers remain in protected audit.

Unknown usage or cost remains unknown; it is never serialized as a known zero. Strict-budget callers must reject unknown price evidence or apply an explicit conservative policy.

Tachi is authoritative for upstream-attempt usage and cost. ZeroClaw retains body-level budgets and the owner-facing accounting projection. The caller's admitted spend and deadline allowances cover the sum of all broker attempts, not only the final response. Usage settles idempotently by invocation/attempt identity: duplicate or retransmitted receipts never double-charge, and later authoritative usage reconciles provisional or unknown usage instead of appending another charge. Cancelled, failed, and fallback attempts follow the same accounting contract.

### 7. Give upstream retry and fallback one owner

For an admitted broker route, Tachi owns the upstream retry/fallback budget. ZeroClaw does not add a second provider retry/fallback chain around it.

In particular, a partial stream error or disconnect is not proof that the upstream did not accept the request. ZeroClaw must not blindly replay an outcome-unknown invocation through its non-streaming recovery path (`crates/zeroclaw-runtime/src/agent/turn/provider_call.rs`). Automatic retry is permitted only when the retained upstream contract proves that no upstream send occurred, proves that the request was rejected before acceptance/execution, or explicitly supports safe idempotent replay of the same invocation. A generic status code or disconnect alone is not such proof. An owner who deliberately requests another invocation after being informed of an uncertain outcome creates a separate, freshly authorized operation; automatic recovery never performs that substitution.

The first version prefers a typed `outcome-unknown` failure over a persistent exactly-once queue.

Bounded resources are part of the protocol, not a later optimization. Slice 0 freezes numeric limits for request/history/tool-schema bytes, individual stream events, cumulative retained tool arguments and buffered stream data, concurrent invocations, admission queues and backpressure, and connect, idle-read, and total deadlines including retry/backoff time. Slow-consumer handling and permit/socket release are specified for every exit. No store lock is held across upstream inference, and broker saturation must not starve Tachi control, vault, or health operations.

Cancellation and completion are equally explicit:

- ZeroClaw explicitly cancelling or aborting its broker invocation closes or cancels that invocation's upstream task and releases its resources;
- a PWA/WebSocket subscriber disconnect only detaches that subscriber and does not automatically cancel the conversation-owned turn;
- normal completion, length truncation, protocol error, connection loss, explicit turn/invocation cancellation, and unknown outcome remain distinct;
- EOF never fabricates successful completion;
- permitted partial output is preserved without pretending the turn completed;
- cancellation or disconnection does not promise that upstream computation or billing stopped.

### 8. Use a dedicated local capability, not general loopback or vault trust

The preferred transport is a dedicated owner-only Unix-domain socket with peer-UID checks and controlled socket lifecycle. Peer UID proves the local user, not that the caller is the ZeroClaw process; it is not presented as strong application isolation against arbitrary same-user processes.

A loopback HTTP alternative requires a separate, revocable broker token, disabled proxies and redirects, and strict endpoint/Host/Origin policy. It does not inherit general MCP loopback trust or Vault owner capability.

The ZeroClaw principal may only:

- invoke admitted aliases;
- query their capabilities and request-scoped receipts;
- cancel its own invocations.

It cannot read upstream keys, list or administer Vault, change endpoints, create profiles, modify shared aliases, or begin OAuth administration. Requests containing caller-selected URLs, credentials, or upstream authentication headers are rejected before transmission.

PWA clients, gateway Nodes, and edge devices continue to enter through ZeroClaw and receive no broker token, Vault forwarding, or generic HTTP proxy. Broker deployments that would call the broker itself or create recursion are invalid.

### 9. Record metadata, not conversation contents

Broker logs and audit contain controlled metadata, not prompts, completions, tool arguments/results, response bodies, or raw upstream error bodies. The existing Tachi LLM recorder's full prompt/result capture is not part of this path.

Privacy acceptance uses unique marker strings in prompt, tool arguments, and upstream errors and proves their absence from broker logs, audit records, error reports, and broker-owned persistence. Omitting an `Authorization` header is not sufficient evidence of privacy.

### 10. Separate default selection, session switching, and credential rotation

These operations have different scope:

1. **Owner changes a default alias binding.** Tachi uses its revision-checked plan/apply path. Existing sessions retain their admitted route; new sessions receive the new binding.
2. **A session explicitly switches models.** The owner selects another alias from that session's authorized set through the existing owner-selected provider profile and session lifecycle; the model-requested `model_switch` tool is retired (#717) and model attempts to invoke it stay rejected. A switch applies at a provider-call boundary and never edits the global default. No streaming response changes provider implicitly.
3. **A credential rotates.** The pinned provider recipient and model do not change. Each new request/attempt receives the currently admitted credential version; an existing stream retains its own snapshot.

Route pinning never defeats revocation or expands authorization. Every request still undergoes current security admission. Before cutover, the retained-revision or pinned-deployment rule must be compatible with Tachi's catalog revision checks.

ZeroClaw `model_routes` may become purpose-to-alias mappings. Summary, classifier, advisor, and other purpose-specific routes remain distinct; they do not all collapse into `body/default`. Upstream fallback is declared in one place only.

### 11. OAuth migration is a later per-flow decision

Tachi may eventually own OAuth login, token storage, refresh, account status, and revocation for a retained provider. Storing a token alone does not transfer that lifecycle.

Each real flow migrates independently with its existing provider behavior as evidence:

- one writer per refresh token;
- single-flight refresh, wait-and-reread, and failure backoff;
- atomic persistence and version-conflict handling;
- preservation of an old refresh token when the upstream omits a replacement;
- account identity, required scopes, endpoint/header behavior, and PKCE/device-flow details;
- revoke fencing so a late refresh cannot recreate a logged-out credential;
- an honest distinction between local deletion and actual upstream revocation.

The broker pilot neither requires nor proves a general OAuth framework. Non-inference OAuth, including email, is outside this ADR.

### 12. Availability changes only at cutover, and must be explicit

Until a live path cuts over, ADR-017 remains unchanged.

A cutover that makes Tachi the sole inference path for model-dependent L0/L1 work requires an amendment to ADR-017 §3/§4 and the corresponding direction statement in #388. The new contract is:

> Tachi provides independent inference and delegation capabilities. When Tachi is unreachable, ZeroClaw retains local state, configuration, emergency-stop access, and authorized non-model operations. Model-dependent L0/L1 calls return typed `inference-unavailable`; L2 retains its existing unavailable/no-local-launch rule. ZeroClaw never silently uses cached upstream credentials for direct fallback and never blindly replays an outcome-unknown invocation.

"Body remains available" must not be described as "ordinary chat remains available." Model-dependent cron, summary, advisor, and similar work are affected too.

There is no automatic direct-provider runtime fallback. A bounded migration rollback is an owner-initiated operation: stop new broker admission, account for in-flight requests, restore the prior verified configuration/version, and do not blindly replay unknown outcomes.

A permanent direct path would be a different availability decision. It would also retain ZeroClaw credential/adapter ownership and could not be described as a sole-broker architecture.

Emergency-stop engage/status remains independent of broker readiness, broker credentials, and model-secret resolution. Issue #714 is a hard safety gate for cutover. Issue #713 remains an independent fix for retained direct Vault paths; broker credential selection must not reproduce its refusal-to-raw-get fallback.

Vault auto-lock, last-known-good credential use, explicit credential revocation, and broker disable are separate policies. Cutover must define which block new requests and how each treats in-flight work; existing last-known-good behavior is not automatically the broker safety contract.

### 13. Voice and other specialized providers remain outside the first path

Realtime voice is not a chat-alias consumer. ADR-023 requires actual recipient identity and consent before audio is sent, not merely an audit record afterward. A voice route cannot share a chat alias that permits cross-provider fallback.

TTS/STT, embeddings, multimodal/vision, reasoning-history extensions, and provider-hosted tools likewise require named consumer evidence and their own acceptance slices. Text-chat parity does not authorize their deletion.

## Migration slices

These are proposed bounded slices, not authorization to implement them as one campaign.

| Slice | Deliverable | Gate |
| --- | --- | --- |
| 0 | Effective-path inventory, ownership and failure contracts, numeric resource limits, predeclared observation window and latency/availability criteria, and proposed ADR-017/#388 amendment | Stop if the actual wire/auth/consumer set is not one bounded path |
| 1 | One production profile in Tachi's existing catalog/alias/control plane; no traffic | Unknown alias, revision drift, and unauthorized recipient/key refuse; no upstream send; no parallel store |
| 2 | Default-off local broker surface and one upstream adapter | Deterministic fake-upstream parity for requests, history, tools, stream events, resource bounds, cancellation, errors, and usage |
| 3 | Optional ZeroClaw `tachi` slot with receipts, capabilities, cancellation, cost, and typed errors; old default retained | CLI, `/ws/chat`, tool rounds, and actual non-streaming consumers pass; missing metadata or cancellation parity blocks cutover |
| 4 | Live-default cutover with time-bounded owner rollback | Installed-pair, owner-authorized real-endpoint acceptance and the predeclared observation window pass; upstream sees only Tachi credentials; ZeroClaw no longer holds that path's upstream key; Tachi stop preserves estop/status and yields typed inference-unavailable; no direct fallback or blind replay |
| 5 | Additional named consumers and individual OAuth flows | Each has independent parity, refresh/revoke/restart/failure acceptance; no consumerless flows are built |
| 6 | Bounded deletion of replaced ZeroClaw modules | Each PR names retained callers and the proven replacement; shared guarantees keep discriminating tests |

Provider consolidation in #688 follows demonstrated replacements. It is not a license to delete still-used providers in advance.

## Acceptance

The pilot is accepted only when all applicable checks pass:

1. The effective live path and all retained consumers are recorded without secret values. Numeric resource limits, the observation window, and latency/availability criteria are frozen before results are collected.
2. Direct and broker paths produce equivalent requests, tool history, streamed events, usage, and terminal outcomes against a deterministic fake upstream.
3. Parallel tools, fragmented UTF-8/tool arguments, call IDs/results, reasoning/history extensions used by the path, truncation, missing completion, 401, 429/Retry-After, 5xx, and outcome-unknown remain distinguishable and correct.
4. A contract-proven pre-execution rejection may recover within Tachi's retry budget, but a generic status code or disconnect is not treated as such proof. When a fake upstream has accepted a request but emitted no result before disconnect, the test proves that no blind second upstream send occurs.
5. A same-scope duplicate invocation with an unchanged payload causes no second upstream send; a changed payload conflicts; cross-scope invoke/read/cancel is denied. After restart, stale handles fail closed and a missing receipt is never treated as proof of non-transmission.
6. Adversarial inputs exceed request/history/schema bytes, event size, cumulative tool/stream retention, concurrency, queue, connect/idle/total deadline, and slow-consumer limits. Backpressure and cleanup hold, permits/sockets are released, no store lock spans inference, and saturation does not starve Tachi control/health operations.
7. Explicit ZeroClaw turn/invocation cancellation propagates, releases upstream resources, preserves permitted partial output, and never fabricates success. With two attached subscribers, one PWA/WebSocket subscriber disconnect detaches only that subscriber while the conversation-owned turn continues.
8. Changing alias A to B does not migrate an existing session implicitly; a new session receives B; explicit session switching does not alter the global default.
9. Rotating credential A to B requires no ZeroClaw provider rebuild, and the next request uses B. A disabled/refused credential cannot reappear through raw retrieval.
10. Unauthorized aliases, caller-selected URLs/headers, web origins, and PWA/Node credentials are rejected before any upstream send.
11. Unique privacy markers from prompts, tool arguments, and upstream errors appear in no broker logs, audit, error output, or broker-owned persistent files.
12. Receipts and cost accounting attribute every attempt to actual served model, usage, and pricing revision; unknown values remain unknown. Duplicate receipts do not double-charge, later authoritative usage reconciles rather than appends, and the admitted spend/deadline allowance covers cancelled, failed, and fallback attempts.
13. With Tachi stopped, estop/status still work and affected inference returns typed unavailable without direct fallback.
14. Before default cutover or deletion, an owner-authorized test of the installed ZeroClaw/Tachi candidate pair against the retained real endpoint proves chat/tool continuation, rotation, explicit cancellation, restart and unsupported-peer refusal, and rollback. The predeclared observation window meets its latency/availability criteria.
15. The completed pilot names concrete ZeroClaw production adapters/authentication modules that can now be deleted, with retained-consumer evidence. If there are none, the data-plane experiment has not justified itself.

## Stop conditions

Stop expansion, and if necessary retire the new data plane, when any of these is observed:

1. The first production path requires multiple dialects or a general OAuth framework.
2. ZeroClaw and Tachi must both retain the same provider quirks, refresh, or retry logic.
3. Cancellation, absence of blind/unsafe outcome-unknown replay, request identity/scope, resource bounds, tool history, or cost attribution cannot be proven against the fake upstream.
4. Operation requires Vault-admin authority, arbitrary caller-selected URLs/headers, or content logging.
5. Everyday availability requires a permanent automatic direct fallback.
6. The predeclared observation window shows that Tachi background work materially degrades interactive latency or availability beyond a bounded isolation fix.
7. A successful pilot produces only a promise of future simplicity, with no concrete deletion candidates.

The fallback architecture after a failed pilot is the pre-broker boundary: Tachi owns credentials/control plane where useful, while ZeroClaw retains a small set of direct protocol clients. Control-plane and data-plane adoption need not succeed or fail together.

## Consequences

Positive:

- One runtime owner can manage admitted endpoints, upstream credentials, request-time key selection, and served-model evidence.
- Credential rotation can affect new requests without rebuilding ZeroClaw provider instances.
- OAuth can later gain one refresh owner per account rather than competing writers.
- ZeroClaw can delete concrete provider and authentication machinery after replacement, instead of merely hiding provider names.
- Separate processes retain independent recovery and prevent a Tachi HTTP failure from directly exiting the body.

Negative:

- Tachi gains a production inference data plane and its operational burden.
- Model-dependent inference acquires a new daemon availability dependency at cutover.
- A versioned cross-repository protocol, receipts, compatibility tests, and revision management become permanent work.
- Incorrectly layered retries can duplicate upstream requests; incorrect aliases can corrupt identity and cost attribution.
- Some code moves to Tachi rather than disappearing. Line counts remain budgets and evidence, not the goal.

## Alternatives considered

- **Vault references with direct providers.** Lowest complexity and the fallback if the pilot fails. It does not remove upstream credentials from ZeroClaw or centralize request-time authentication and provider adaptation.
- **Move `vault-kit` or the whole Vault store into ZeroClaw.** Rejected. The format/crypto crate is not the Vault service, and moving the store reverses the machine-wide credential ownership boundary without simplifying providers or OAuth.
- **Extract only a narrow Vault client.** Allowed later for lease/resolve/status ergonomics, but orthogonal to inference brokerage and not a substitute for it.
- **Merge repositories or processes.** Rejected for this pilot. Product integration does not require it, and process fusion would weaken failure isolation.
- **Restore the retired universal Tachi gateway.** Rejected. Only named production paths with acceptance and deletion evidence are admitted.
- **Use cc-switch/magpie as the required runtime dependency.** Not the default. Their designs are references; this ADR does not add another general aggregator daemon.
- **Keep all direct providers indefinitely.** Rejected as a growth strategy, but existing providers remain until their consumers migrate or their behavior is explicitly retired.

## References

- [ADR-012](./ADR-012-generation-scoped-live-config-apply.md): generation-scoped live configuration apply.
- [ADR-017](./ADR-017-personal-agent-body-edges-and-delegation.md): body, L0/L1/L2, delegation, availability, licensing, and package-management contracts.
- [ADR-019](./ADR-019-one-body-many-hands.md): body authority and shared-code extraction.
- [ADR-020](./ADR-020-durable-turns-replay-and-attention.md): scoped request identity and conservative replay.
- [ADR-023](./ADR-023-realtime-voice-carrier-with-body-delegation.md): voice carrier, recipient identity, and consent.
- ZeroClaw #374, #382, #388, #688, #713, and #714: execution order, pruning, direction, provider consolidation, and safety gates.
- Tachi [#1682](https://github.com/kckylechen1/Tachi/issues/1682), [#1684](https://github.com/kckylechen1/Tachi/issues/1684), [#1770](https://github.com/kckylechen1/Tachi/pull/1770), and [#1776](https://github.com/kckylechen1/Tachi/pull/1776): prior gateway/OAuth scope and retirement evidence.
- Source review on 2026-11-17 used ZeroClaw `3805d2ac9a165fd6f41a7f5e11b424d0ff975605` and Sigil `dd973675a110d651e1449367f890fa70dfffd4db` (`codex/2113-eval-receipts`). The Sigil revision identifies the inspected evidence; it is not a claim that these contracts already exist on its main branch.

## Amendment 1: the end state is zero providers in the body

Owner direction, 2026-11-18. This amendment fixes the destination; it does not accept the pilot or change any slice gate.

### Target end state

ZeroClaw holds no model-provider families, provider adapters, or upstream credentials. The body keeps exactly one generic inference client that talks to the Tachi broker over the contract in §5–§8, and every model-dependent path is admitted through that broker. The sole-broker availability contract in §12 is therefore the intended destination, not one possible outcome among several; the "permanent direct path" in §12 is rejected as an end state.

### How the body gets there

The route is unchanged: consumer-driven slices, each proving parity for a named production path and then deleting the replaced ZeroClaw adapter and credential handling (Migration slices 3–6). A universal gateway is still not built ahead of its consumers; Tachi [#1776](https://github.com/kckylechen1/Tachi/pull/1776) remains the precedent for why.

One reduction does not wait for the broker. Families that are only a URL and header preset over the OpenAI-compatible client carry no protocol behavior of their own, so they collapse into the existing `custom` family with an explicit endpoint `uri` now, together with the unused `xai` family and its OAuth (owner decision, tracked by #688). Families with their own wire protocol, authentication, or quirks (for example `openai` with its Codex OAuth and Responses paths, `anthropic`, `gemini`) leave the body only through broker slices.

### What changes downstream

- Provider deletions after the URL-preset collapse cite the broker slice that replaced them; #688 no longer tracks provider-count targets for native-protocol families.
- At cutover the owner accepts that model-dependent work, including cron, summaries, and advisors, returns typed `inference-unavailable` while Tachi is unreachable, with no direct fallback (§12).
- The stop conditions still apply. If the pilot stops, the fallback architecture in "Stop conditions" governs the families that remain, and this end state is revisited by a later amendment rather than pursued around the failed pilot.
