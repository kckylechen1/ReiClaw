---
id: ADR-023
title: Realtime voice carrier with delegation to the body
date: 2026-11-16
status: accepted
relates-to:
  - ADR-014
  - ADR-015
  - ADR-016
  - ADR-017
  - ADR-019
  - ADR-020
  - ADR-021
  - ADR-022
  - crates/zeroclaw-gateway/src/ws.rs
  - crates/zeroclaw-gateway/src/ws/surface.rs
  - crates/zeroclaw-gateway/src/voice_duplex.rs
  - crates/zeroclaw-config/src/bridge_access.rs
  - crates/zeroclaw-runtime/src/agent/persona_projection.rs
  - crates/zeroclaw-memory/src/companion/user_model.rs
---

# ADR-023: Realtime Voice Carrier With Delegation to the Body

## Status

Proposed and accepted by the owner on 2026-11-16. Acceptance records the architecture; it does not by itself change a protected contract or activate a feature. Each implementation slice below is reviewed separately.

The owner resolved the alias question on acceptance: the voice surface uses the owner's main agent, so the robot and every other surface share one Soul (§3).

Amendment 1 (accepted by the owner on 2026-11-16) binds what a delegated turn may do to the device bridge credential, adds a later owner tier through speaker verification, and reorders the remaining slices. Where it conflicts with §1–§4, Amendment 1 governs.

Amendment 2 (accepted by the owner on 2026-11-17) fixes one wire protocol for every carrier and moves provider-specific code out of the voice client. Where it conflicts with §1, §4 item 4 or slice 4, Amendment 2 governs.

### What it supersedes in ADR-021

This ADR is the new ADR that ADR-021 §10 requires before a front harness may answer directly or gain another tool. For the carrier mode defined here, it replaces these parts of ADR-021:

- §1: the restriction that end-to-end speech models may be used only for recognition, synthesis, or the §10 front harness;
- §9, engine table: the row that limits cloud realtime models (`gpt-realtime-2.1`, Qwen-Omni realtime) to benchmarks;
- §10, "One tool": the single-model-tool rule, for the carrier's provider-side search only (§2 below);
- §10, "It must not": "answer any substantive question" and "decide whether a question is simple enough", which are replaced by the routing rule in §2 below; and "have a persona of its own", which is replaced by the kernel projection in §3 below;
- §10, "Voice and words": the rule that Identity, Principles and User Model records are not exported to the front harness, which is replaced by §3 below;
- §10, "Everything it says is reported": the statement that front-spoken reports live in the existing conversation record, which is replaced by §5 below;
- §12, third item: "Letting the front harness answer simple questions directly";
- Acceptance item 8, for carrier mode, which is replaced by this ADR's Acceptance.

All other ADR-021 decisions stay in force, including §2 (device and adapter split), §3 (echo cancellation), §4 (body streaming), §5 (interruption), §6 (playback evidence), §7 (bounded listening; voice input is never owner evidence), §8 (attention), the rest of §9, the §10 fallback to the cascaded path, and the §11 latency targets.

### What it amends elsewhere

- **ADR-017 §3 and §4.** ADR-017 places spoken everyday questions such as the weather in the body's L0 and states that "the owner talks only to the body". Under this ADR, public, non-personal questions may be answered by the carrier while it speaks with the body's projected persona. Everything personal or consequential still goes to the body. ADR-017 carries an amendment note.
- **ADR-019, Yonbo Client.** The rule that "the adapter injects no persona or memory of its own" is tightened. The adapter's carrier instructions contain only routing and delegation rules; identity comes only from the kernel projection (§3).

## Context

- The owner tested two realtime speech-to-speech services as the robot's voice: MOSS Live (`moss-live-1.0`), which the Robotic adapter already integrates, and OpenAI `gpt-realtime`.
- MOSS Live cannot turn off automatic replies and has no forced tool choice. It does offer native client delegation:
  - the service emits `session.delegation.created` with an objective, bounded context, and a purpose of `task` or `reasoning`;
  - the client answers with `session.commentary.append` (speakable progress or result), `session.thinking.append` (silent facts) and `session.instructions.append`;
  - it also accepts a persona `{name, aliases, prompt}` and instructions through `session.update`, and emits `input.transcript.done` with the final user text.
- OpenAI Realtime can disable automatic responses (`create_response: false`). That would allow the strict cascaded mode of ADR-021 §1.
- The owner rejected the strict mode for the robot. Making every greeting or weather question wait for a full body turn is the wrong experience. Small, public, non-personal requests should be handled by the voice service directly, including with its own search. Everything else should be delegated, and the robot should say so ("好的，我去查查") instead of falling silent.
- The owner also requires that the body stay the single source of identity and memory, and that the robot know the owner: the Soul and owner-approved parts of the User Model.
- The voice adapter as first written connects to `/ws/chat` without `surface=voice` and authenticates with a paired token (`Zeroclaw-Robotic/crates/yonbo-voice/src/gateway.rs`). On any gateway without `gateway.operator.token_hashes`, paired tokens keep legacy operator authority (`crates/zeroclaw-gateway/src/operator_auth.rs`, which logs a WARN for this case). In that configuration the adapter's text can be recorded as operator input with owner ingress (`crates/zeroclaw-gateway/src/ws.rs`, owner-ingress handling), and the voice credential can call management APIs. Slice 1 removes that combination.
- The adapter currently writes its own identity into the carrier instructions (`Zeroclaw-Robotic/crates/yonbo-voice/src/moss.rs`, the default instructions naming the robot and the owner).

## Decision

### 1. The realtime voice service is a carrier

The realtime speech-to-speech service (MOSS Live, OpenAI Realtime, or a successor) is a **carrier**. It hears, speaks, and handles a bounded set of requests by itself. It is not the brain:

- it holds no memory beyond the open conversation window and writes none;
- its only capabilities are speech, the provider's built-in public search, and delegation to the body; it has no tool that acts on the owner's accounts, devices, files, messages, or schedules;
- it cannot approve anything;
- it receives no gateway credential; the adapter holds all credentials.

The carrier and its provider are chosen in the adapter's configuration. Replacing the carrier does not change the body. Under Amendment 2 it does not change the voice client either.

### 2. What the carrier answers, and what it delegates

The carrier decides the routing itself, under these rules.

It may answer directly:

- greetings, small talk, and conversational repair (asking the owner to repeat);
- public, non-personal facts, including results from its built-in search: weather, time, public knowledge, simple arithmetic;
- acknowledgements that it is delegating ("好的，我去查查"). These never claim that work has started or finished; status comes from the body's receipts (ADR-020 §8).

It must delegate to the body anything involving:

- the owner's memory, history, preferences, relationships, or private data;
- tasks, reminders, devices, messages, purchases, or any action;
- the body's commitments, opinions about the owner, or decisions;
- anything it is unsure about.

The boundary is enforced by the carrier's instructions and by measurement (§7), not by the carrier's good will; prompting is not enforcement. This residual risk is accepted and bounded by §1: a misrouted request can produce wrong words, but it cannot act, approve, or write memory.

§1 confines the carrier only. A *delegated* request reaches the body, so "cannot act" also requires the body to confine device-bridge turns. Amendment 1 defines that confinement.

### 3. The kernel projects a voice persona; nothing raw leaves the body

**Which agent.** The voice surface connects to one configured agent alias. The projection and every delegated turn use that same alias, so the carrier sounds like the agent that answers delegated requests. That alias is the owner's main agent (owner decision, 2026-11-16), so the robot shares the one Soul used on every other surface. The voice surface's limits come from the surface rules: unverified input, no approval by voice, and no owner ingress. They do not come from a separate restricted agent. What a delegated turn may *do* is bound to the device bridge credential, not to the surface (Amendment 1).

**Where it is delivered.** When a voice-surface connection opens (`/ws/chat` with `surface=voice&voice_provider=moss-live`, using the device bridge `chat` operation; the credential also holds `cancel` for §4), the body returns a versioned `voice_projection` field on the initial `session_start` acknowledgement. `connected` is the later, optional `connect` negotiation reply; it carries no projection. Voice must be declared in the query, not introduced by a later `connect` frame.

The connection must authenticate with a `[gateway.bridges.<name>]` device credential whose device configuration allows `chat` for that exact agent alias. Paired tokens, anonymous connections, owner bridges and bridges without device configuration are refused at HTTP upgrade with HTTP 403, code `VOICE_DEVICE_CREDENTIAL_REQUIRED`; they receive no projection. Device alias and operation restrictions remain enforced by `BRIDGE_BINDING_REJECTED`; an initial session-scope mismatch retains the existing HTTP 403 text response.

- This changes the protected `/ws/chat` contract. It is versioned and stays behind the default-off `voice-surface` feature, like ADR-021 §4.
- No new bridge operation is added.
- The adapter places the projection in the carrier's persona and instructions for that window only. It is never written to disk on the adapter or the device.
- The adapter's own instructions contain only routing and delegation rules (§2, §4), never an identity or a form of address.

**What it contains.** The projection renders the same bytes as the corresponding `persona_projection` layers for that alias:

- Identity: name, self-description, primary language, pronouns, and the fixed honesty line;
- Tone: the five Tone levels as rendered;
- Principles: the owner-authored list;
- owner facts for voice: only User Model heads that carry the owner's voice export consent (below).

**What it never contains:**

- the Story layer, both `self` and `bond` entries;
- Tone exemplars, which are owner lines;
- any User Model head without voice export consent;
- session history, memory search results, approvals, or credentials.

**Voice export consent.** User Model heads get a separate, owner-only export-consent field. It is distinct from `Scope`: `Scope` decides where a head applies, and consent decides whether its text may leave the body to a carrier.

- Consent is recorded per exact head revision and bound to the carrier provider identity. A new head revision or a different provider requires fresh consent.
- Nothing has consent by default.
- Revocation takes effect on subsequent projection reads immediately; a projection already delivered to a carrier cannot be recalled. The adapter must close a window to discard bytes it already received.
- This is a change to the protected User Model store. It needs its own reviewed slice.

**Kernel wire and consent API (slices 2–3).** All new surfaces require `voice-surface`:

- `voice_provider` is a case-sensitive identifier of 1–64 ASCII lowercase letters, digits or hyphens, starting with a letter or digit. Examples: `moss-live`, `openai-realtime`. The adapter declares it; the kernel does not attest which external service the adapter contacts.
- `voice_projection` v1 has exactly `version` (1), `provider`, `identity`, `tone`, `principles`, and `owner_facts`. The last four are rendered strings, with an empty string for an absent optional section. Identity and Principles use `persona_projection` renderers; Tone uses its same effective-dial resolver and renderer, excluding exemplars. These three persona strings apply the same `trim_end()` as `persona_projection`, without its inter-section separators or final assembled newline. Owner facts use the bounded User Model renderer after consent, provenance and Scope filtering for the alias, channel `wss`, and canonical session key `gw_<session_id>`. No raw revision objects are sent.
- Operator-only `GET /api/user-model/heads/{revision_id}/voice-consent?provider=moss-live` returns the latest `receipt` or `null` (no consent).
- Operator-only `PUT /api/user-model/heads/{revision_id}/voice-consent` accepts `{"provider":"moss-live","action":"grant","expected_consent_revision":0}`. `action` is `grant` or `revoke`; CAS starts at 0 and thereafter uses the returned receipt's `revision`. Grant requires that exact fact revision to remain the current head. Revoke can also retire an old head. Conflicts return HTTP 409, code `VOICE_CONSENT_CONFLICT`; receipts identify the server-assigned reviewer `owner_operator`.
- Both consent handlers require configured `gateway.operator.token_hashes` and an independent operator token. An empty hash list returns HTTP 403, code `VOICE_CONSENT_OPERATOR_CONFIG_REQUIRED`, with instructions to configure it; legacy paired-token operator authority never authorizes reading, granting or revoking voice consent.
- `UserModelStore` owns the additive `user_model_voice_consent_receipts` table in the existing owner-only `user_model.db`. Feature-enabled open creates the table and append-only triggers idempotently. The unique `(head_revision_id, provider, revision)` index orders consent decisions. There is no backfill, new store, Scope rewrite or default grant. Older binaries ignore the table; re-enabling the feature reuses its receipts. Active heads, provenance and consent are read together in one transaction, without an export cache.

**Fail closed.** If the projection cannot be produced, the adapter does not open the carrier session. That covers an unreadable Soul store; the degraded "identity unavailable" rendering; legacy `SOUL.md`/`IDENTITY.md` without an owner Identity revision; an unknown alias; a missing or invalid `voice_provider`; and unreadable User Model consent. The robot gives a fixed local cue instead of speaking without a Soul.

Missing device credentials, a missing/invalid provider, and unknown or disabled aliases refuse the HTTP upgrade with a typed error (`VOICE_DEVICE_CREDENTIAL_REQUIRED`, `VOICE_PROVIDER_REQUIRED`, `VOICE_PROVIDER_INVALID`, `VOICE_UNKNOWN_ALIAS`, `VOICE_ALIAS_DISABLED`). Store/Identity failures send a WebSocket `error` and close before `session_start`, with code `VOICE_SOUL_UNAVAILABLE`, `VOICE_OWNER_IDENTITY_REQUIRED`, `VOICE_USER_MODEL_UNAVAILABLE`, or `VOICE_PROJECTION_UNAVAILABLE`. A late switch to voice returns `VOICE_QUERY_REQUIRED`. These errors carry no projection. Non-voice acknowledgement frames are unchanged, and ignore `voice_provider`.

A disabled alias is refused before projection construction with `VOICE_ALIAS_DISABLED`: HTTP 403 at upgrade, or a WebSocket `error` and close if it becomes disabled before projection admission.

Immediately before sending the projection, the gateway rechecks the original device bridge credential, device configuration, exact alias, `chat` operation, session restrictions, alias enablement and authentication mode. A reduction after projection admission returns `VOICE_AUTHORIZATION_CHANGED` and closes without a projection.

### 4. Delegation goes through the voice surface

When the carrier delegates:

1. The adapter submits one `/ws/chat` turn on the voice surface, with a scoped request identity (ADR-020 §2). The turn carries two separately labelled inputs:
   - the carrier-authored objective and context, labelled carrier-authored;
   - the raw final transcript of the window's recent user speech, labelled unverified speaker transcript.
2. Neither input is owner evidence, and neither satisfies an approval (ADR-021 §7).
3. Carrier-handled exchanges from the same window (§5) may be included in the turn as quoted, labelled context. The body does not read them from storage.
4. The body's spoken-text result goes back to the carrier as speakable commentary; silent facts go back as thinking.
5. If the carrier cancels a delegation, the adapter requests cancellation of that scoped turn (ADR-021 §5).

The adapter authenticates with a device bridge credential (`[gateway.bridges]`, with the `chat` and `cancel` operations), not with a paired token that holds operator authority.

### 5. What the carrier handled is reported, not remembered

Each exchange the carrier handled itself is reported to the body as an untrusted presentation event. It carries the user's words, labelled unverified speaker transcript, and the carrier's reply, labelled carrier output.

- These events are stored in the voice session's presentation-evidence store from ADR-021 §6, with the same storage owner, retention and deletion. That store is identified in the implementation review.
- A carrier-handled exchange has no body turn, so its event is bound to the admitted device, conversation, listening window and exchange sequence number instead of a request and turn identity. Duplicates and reconnects cannot attach it to another window.
- They are not `user` or `assistant` session rows.
- They have no owner ingress. They never reach owner learning, User Model candidates, or the ADR-016 weekly reflection. Household members, television and false wakes can all produce transcripts.
- They may enter long-term memory only through a later owner-reviewed path, which is out of scope here.

### 6. Failure behavior

- **Body or gateway unreachable when a window opens.** There is no projection, so no carrier session opens (§3).
- **Body lost mid-window.** The carrier says it cannot reach the body and the window closes. It does not continue small talk without the body.
- **Carrier unreachable.** The adapter falls back to the ADR-021 cascaded path if that path is configured; otherwise the window does not open.

### 7. Measured before enabling

The following are measured on a fixed test set before carrier mode is enabled for daily use:

- **Delegation routing.** Count personal or action requests that the carrier answered itself. The target is zero on the test set, and every miss is reviewed.
- **Fabrication while waiting.** Check whether the carrier invents an answer while a delegation is pending. MOSS cannot disable automatic replies, so this must be observed, not assumed.
- **Latency.** Measure from the owner stopping speaking to first audio for a direct answer, and to the acknowledgement for a delegated request. ADR-021's targets are the reference.

### 8. Scope

- **Device Node.** The robot's Node role, meaning capabilities over `/ws/nodes`, is out of scope. It follows ADR-022 and the kernel capability catalog (#630). One inventory identity may group the robot's Node credential and the adapter's bridge credential, but the two are not interchangeable and are revoked separately (ADR-022).
- **`voice_duplex`.** The speaker Client designed here uses `ChatSurface::Voice` and does not use `gateway/src/voice_duplex.rs`, which only logs events. This settles the ADR-017 §5 "keep until the speaker Client is designed" row: the module may be removed in its own reviewed PR.

## Consequences

Positive:

- Small talk and public lookups feel immediate. Personal questions and actions still go through the body; under Amendment 1 v1, the body declines them on voice until the owner tier exists.
- The robot knows the owner through a reviewed projection, without raw Soul or unconsented User Model access.
- Carriers can be swapped between providers without a body change.
- The voice-credential and owner-ingress defects found in Context are corrected as part of adopting the voice surface.

Negative:

- The routing boundary in §2 depends on the carrier following instructions. A misroute can produce wrong words but no action, provided the body confines device-bridge turns (Amendment 1).
- Projected persona text and consented owner facts go to a third-party provider.
- There are two voice paths to maintain: carrier mode and the cascaded fallback.
- The protected `/ws/chat` contract and the User Model store both change.

## Implementation slices

1. **Credential and surface correction.** This is configuration and build only; there are no source changes:
   - build the gateway with `voice-surface`;
   - give the adapter a `[gateway.bridges]` device credential with the `chat` and `cancel` operations, and connect with `surface=voice`;
   - configure operator token hashes so that paired tokens lose management authority;
   - before the cutover, list the clients that rely on paired-token management.
2. **Voice export consent** on User Model heads: owner-only, per head, bound to the provider.
3. **Handshake projection:** add a versioned `voice_projection` to the voice-surface `session_start` acknowledgement, with tests for fail-closed behavior and for the exclusion of the Story layer, exemplars and unconsented heads.
4. **Adapter delegation** (Robotic repository):
   - map carrier delegation to scoped voice-surface turns with separately labelled inputs, commentary and thinking replies, and cancellation;
   - remove the adapter's own identity text from the carrier instructions.
5. **Presentation-event intake** for carrier-handled exchanges into the ADR-021 §6 store, with tests that no owner ingress and no session rows are created.
6. **Measurement** under §7, recorded as evidence before daily use.

Issue #374 remains the execution index. Each slice is one reviewed PR.

## Acceptance

1. A delegated voice turn records the carrier-authored objective and the speaker transcript as separately labelled, unverified inputs, never as operator input, with tests.
2. The voice credential cannot call operator-gated APIs.
3. The projection matches the corresponding `persona_projection` layers byte for byte, including pronouns and the honesty line. It contains no Story, exemplar, or unconsented User Model content. Every degraded or unknown state fails closed.
4. Carrier-handled exchanges create no session rows and no owner ingress.
5. The §7 measurements are recorded, and the owner accepts them before daily use.

## Amendment 1: ingress tiers bound to the bridge credential, and owner speaker verification

Accepted by the owner on 2026-11-16.

### A1. Why

- `ChatSurface` is a presentation register requested by the client, "never an authority grant" (`crates/zeroclaw-api/src/chat_surface.rs`). The runtime, memory and tool crates do not read it.
- A device-bridge connection builds the bound alias with that agent's own risk profile and tools (`build_ws_session` in `crates/zeroclaw-gateway/src/ws.rs`). The voice surface removes owner ingress and approval by voice; it does not narrow tools. For an agent bound at `full`, only tools in `always_ask` prompt (`crates/zeroclaw-runtime/src/approval/mod.rs`), so unverified speech delegated by the carrier would reach that agent's tool set.
- Lowering a voice turn to `supervised` does not fix this. The device credential cannot answer approvals (`crates/zeroclaw-config/src/bridge_access.rs`), so prompts either wait until timeout or, with an `approval_route`, reach an owner channel and bypass A4. Auto-approved tools would still run.
- Memory writes do not depend on the surface. The gateway consolidates each turn (`consolidate_turn`, called from `ws.rs`), and the agent stores user messages when `auto_save` is on (`crates/zeroclaw-runtime/src/agent/agent.rs`). Household speech, television and false wakes would therefore enter the owner's memory.
- Read-only is not the same as safe. Answers are spoken aloud to a room, and private text in the body's prompt can also leave through tool arguments such as a search query, whether the trigger is a bystander's request or an injected search result.

### A2. Decision: a delegated turn is confined by its credential

Permissions for device-bridge turns bind to the bridge credential, independent of `surface`.

The rules below define the public tier, which every device bridge gets in v1. The owner-tier slice (A3) may relax exactly the memory and cron/schedule deny entries, `exclude_memory`, and the User Model exclusion, and only for a bridge explicitly marked owner-tier, through its own reviewed change. Shell and process, file, messaging and channel, session, and `tool_search` stay denied for every device bridge.

**Profile and validation.** `[gateway.bridges.<name>.device]` names a `risk_profile`. Config validation requires all of the following:

- the profile exists and its level is not `full`;
- `allowed_tools` is `Some` and contains none of the fixed device deny set: shell and process tools, file tools, messaging and channel tools, memory tools, session tools, cron and schedule tools, and `tool_search`;
- MCP tools are listed by their full `<server>__<tool>` name, and `mcp_discovered_tool_policy` is `explicit_only`;
- every allowed tool is in `auto_approve`, `always_ask` is empty, and `approval_route` is unset.

Voice admission refuses a device bridge without a valid profile, with a typed error. In addition, any approval prompt raised on a device-bound turn is denied immediately; it is never routed and never waits.

**Session construction.** A device-bound session builds the same alias, with the same Identity, using the bridge's profile instead of the agent's own, and with the existing `exclude_memory` seam. That seam replaces the memory backend with `NoneMemory`, strips memory tools and forces `auto_save` off, so the turn has no memory recall, no auto-context and no agent-side memory writes.

- The gateway's `consolidate_turn` does not run for device-bound turns.
- The User Model section is not injected.
- Of the Soul, the prompt receives exactly the layers in the voice projection (§3): Identity, Principles and the Tone levels. Story entries and Tone exemplars are excluded.
- Session transcript rows for the delegated turn are still written, as today.

**Session namespace.** A device bridge's `sessions` and `session_prefix` must not overlap with any other bridge's. On `/ws/chat`, a non-device connection cannot create or attach to sessions in that namespace, and a device connection cannot attach to sessions outside it.

**Public tier (v1, the only tier at first).**

- Allowed tools: `web_search_tool` and allow-listed search MCP tools named individually. Time and simple arithmetic come from the prompt and the model; no tool is needed.
- The only owner facts available to voice are the consented `owner_facts` in the projection (§3).
- Personal questions and requests with side effects are declined in speech, and the robot points the owner to another surface.

**Cost.** Slice 1 includes a lower tool-iteration cap for device-bound turns. A per-bridge rate limit and a daily budget follow in their own slice, before resident wake (A5). Whether device-bound turns use a lighter, faster model is an owner decision recorded with that slice.

### A3. Owner tier through speaker verification (later slice)

- Speaker verification runs in the adapter on the owner's host, which already carries the window's audio. It uses the speaker-embedding API of sherpa-onnx v1.12.14, the release pinned by the Robotic keyword-spotting adapter (`Zeroclaw-Robotic/crates/yonbo-audio/src/kws.rs`).
- The owner's enrollment template is stored owner-private on that host. It never enters a repository and is never sent to a carrier or any cloud service.
- Every user speech segment is verified, not only the wake word. A delegation is owner-tier only when every user segment since the previous delegation in that window verified as the owner. Otherwise it uses the public tier. Mixed-speaker windows therefore fall back to public.
- The adapter holds two device-bridge credentials, public and owner, and selects one per delegated turn. The gateway trusts credentials only, never a client-declared identity.
- The owner tier may add owner-private reads, such as memory recall and the User Model. The owner chooses to hear these answers aloud by asking.
- Voice verification never unlocks side effects, because recordings can be replayed and voices cloned. The one exception: the owner may enable an owner-curated allowlist of low-risk, reversible actions, such as reminders and notes, for the owner tier.
- Thresholds are tuned with real household audio, including television and family members, after resident wake runs. Like §7, they are measured before the tier is enabled.

### A4. Side effects through approval (deferred)

Voice-originated proposals that need approval wait for two things: durable turns (ADR-020) and an owner approval client. The owner bridge with `approval_response` already exists in configuration; only the client is missing. When built:

- approval cards show the real tool arguments and "source: voice, unverified";
- approval requests from voice are rate-limited;
- the approval text is treated as attacker-controlled.

### A5. Slice order

This replaces the order of the original slices 4–6.

1. Bridge risk profile, validation, device-bound session construction, session namespace and tool-iteration cap (A2), in one gateway PR.
2. An allow-listed search MCP for the public tier.
3. Original slice 4: adapter delegation and persona.
4. Per-bridge rate limit and daily budget.
5. Original slice 5: presentation-event intake.
6. Original slice 6: measurement (§7).
7. Resident wake on the robot.
8. Speaker verification and the owner tier (A3).
9. The approval flow (A4).

### A6. Acceptance additions

1. An agent at `full`, reached through a public-tier device bridge, cannot call shell, file, messaging, memory, session or cron tools. Non-device paths are unchanged.
2. Device-bound turns write no memory, run no consolidation, and receive no User Model, Story or exemplar text.
3. A device bridge whose profile fails A2 validation is refused for voice. An approval prompt on a device-bound turn is denied without waiting.
4. On `/ws/chat`, device session namespaces cannot be shared with non-device connections.
5. When it is built, the owner tier is reachable only with the owner credential, and mixed-speaker windows use the public tier.

## Open questions

- Whether carrier-handled exchanges should ever be offered for an owner-reviewed memory path.
- Which low-risk reversible actions, if any, the owner tier may perform (A3).
- Whether device-bound turns use a lighter model, and the daily budget (A2).

## Amendment 2: one carrier protocol, providers outside the client

Accepted by the owner on 2026-11-17.

### B1. Why

- The Robotic voice client speaks the MOSS Live wire protocol directly. `crates/yonbo-voice/src/moss.rs` mixes three things: the MOSS protocol, the adapter's own carrier instructions, and the tool-call and audio-turn bookkeeping.
- MOSS framing also appears in `session.rs`, `events.rs`, `presentation.rs` and `tasks_runtime.rs`. `gateway.rs` hard-codes the provider (`moss-live`).
- Switching to OpenAI Realtime or another carrier would mean rewriting the client, which contradicts §1.
- ZeroClaw core is already retiring per-provider families (#699, #707). A per-provider layer in each voice client repeats the same cost in another place.
- The resident wake trial on 2026-11-17 found that MOSS rejects a `response.create` that carries instructions and a text `conversation.item.create` (`invalid_public_request`). Provider gaps are real, and they belong in one place.

### B2. Decision: the voice client speaks only the OpenAI Realtime protocol

- The voice client implements one carrier protocol: the OpenAI Realtime WebSocket protocol, GA event set (for example `response.output_audio.delta`, not the beta `response.audio.delta`). The implementing PR records the reference revision it was checked against. Translators are tested against the same event set.
- The client contains no provider module and no provider-specific identifier, event or framing.
- Providers that speak this protocol are reached directly, or through a pinned external gateway such as the LiteLLM proxy `/v1/realtime` endpoint.
- A provider that does not speak it, such as MOSS Live, gets a standalone translator. The translator exposes an OpenAI Realtime endpoint to the client and speaks the provider's protocol upstream. It is the only provider-specific code we maintain, and it lives outside ZeroClaw and outside the voice client.

**The subset.** Translators implement exactly what the client uses:

- session configuration: instructions, function tools, audio format, server voice activity detection;
- audio input;
- speech start and stop;
- input transcripts;
- output audio and output transcripts;
- function calls, function outputs, and `response.create` following them;
- response completion and cancellation;
- out-of-band notices (below).

`response.create` with per-response instructions, user text messages and `conversation.item.truncate` are outside the subset. A translator answers anything outside the subset with an `error` event carrying code `unsupported_by_provider`, and an upstream rejection with `provider_rejected`. It never fakes success.

**Speech the client initiates.** This replaces the "commentary" and "thinking" wording of §4 item 4:

- A delegation result returns to the carrier only as the function output for its call, followed by `response.create`. Facts the carrier should use but not read aloud go in the same output, marked as not for speech.
- Speech with no pending call goes as an out-of-band notice: a system-role message item followed by `response.create`. Examples are a task notice and the §6 "cannot reach the body" line. The MOSS translator maps this pair to MOSS session commentary.
- A provider that can express neither form refuses the window at admission. It does not degrade silently.

### B3. Provider identity and consent

- `voice_provider` (§3) names the external service whose servers receive the projection. It does not name the wire protocol or a local endpoint.
- It is configured explicitly next to the carrier endpoint. The client never infers it from the endpoint or the protocol.
- A gateway between the client and the provider must map each configured realtime model to exactly one upstream provider. Fallbacks, load balancing and aliases with more than one deployment are forbidden for realtime models.
- Changing that upstream is a provider change and needs fresh voice export consent (§3).
- "Pinned" means a fixed release version and a reviewed configuration kept in the Robotic recipe.

**No persistence in the middle.** §3's rule that the projection is never written to disk extends to every translator and gateway on the path:

- no payload or prompt logging;
- no observability or callback hooks that receive content;
- no prompt or response storage.

They may log event names, counts and timings only.

### B4. Topology, credentials and endpoints

- The client, the translators and any gateway run on the owner's host, the same host that holds the owner-private material in A3. None of them runs on the robot.
- Provider credentials sit in owner-only files on that host. They belong to whatever serves the carrier endpoint: the translator or the gateway, or the client's private configuration when it connects to a provider directly. They never enter a repository.
- A carrier endpoint is either `wss://`, or plaintext `ws://` to a literal loopback address. A loopback endpoint requires a local shared secret, kept in an owner-only file, from every client.
- The client still holds the device bridge credential for the body (§4). No carrier process receives it.

### B5. Behavior that moves out of the provider code

- **Carrier instructions.** Routing and delegation rules are provider-neutral text in the client. Identity comes only from the projection (§3).
- **Turn bookkeeping.** Call ownership, duplicate suppression, cancellation, barge-in and timers are provider-neutral. They key on protocol identifiers, not provider identifiers.
- **Input transcript timing.** Input transcription may arrive after the function call that triggers a delegation. The client waits a bounded time for the transcript of the triggering speech. After that, it marks the speaker transcript input as unavailable instead of omitting it silently.

**Wake greeting.** A carrier cannot be relied on to speak first; MOSS cannot, per B1. After a wake, the client may send an owner-provided local prompt clip so the carrier answers with a greeting.

- The clip is its own input turn. The client sends it, then holds live microphone audio until the carrier has closed that input. The clip and owner speech therefore never share a segment or a transcript.
- The clip is adapter-authored. Its transcript is excluded from the §4 speaker transcript. In §5 events it is labelled adapter-authored, never as the user's words.
- It is excluded from speaker verification (A3), and it must not be recorded in the owner's voice.
- It contains no owner data.

### B6. Slice changes and acceptance additions

A5 item 3, the original slice 4, now covers the following, as separate reviewed PRs:

1. **The voice client as an OpenAI Realtime client.** This includes:
   - carrier persona from the projection;
   - delegation results and notices under B2;
   - the wake greeting under B5;
   - `voice_provider` and the carrier endpoint as configuration;
   - removal of `moss.rs` and of MOSS framing from every module named in B1.
2. **A MOSS Live translator**, under B2–B4.
3. **The Robotic slot-A recipe.** The resident wake trial found three manual steps; the recipe must cover them or check them in preflight:
   - the microphone array's capture device;
   - the speaker route after boot;
   - the keyword-spotting thresholds.

   If the recipe adopts a gateway, it also carries the gateway's pinned version and reviewed configuration, plus a preflight that fails on more than one upstream per realtime model or on any content logging.

Labelled delegation inputs (§4 items 1–3) need a gateway change and stay a separate slice.

Acceptance additions:

1. **Switching carriers.** Switching between MOSS Live (through the translator) and an OpenAI Realtime provider changes only configuration: endpoint, model, `voice_provider` and credential location. Neither switch changes the voice client source.
2. **No provider code in the client.** The voice client has no provider module or provider identifier. Every frame it sends validates against the pinned GA event set.
3. **Translator errors.** The translator answers each out-of-subset event with `unsupported_by_provider`, and an upstream rejection with `provider_rejected`, with tests.
4. **Provider identity.** `voice_provider` comes only from configuration, with tests that it is never derived from the endpoint.
5. **Greeting exclusion.** Clip text never appears in a delegated turn's input or in a §5 user-words field, with tests.
6. **Resident wake regression.** It passes through the translator: wake, greeting, a public question, and a delegated question. Like §7, this is recorded evidence from the robot, not CI.
