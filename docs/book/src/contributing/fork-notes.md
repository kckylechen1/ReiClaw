# Fork Notes

This page keeps the fork-specific facts that used to live in `AGENTS.md`. The short rules are in [`AGENTS.md`](https://github.com/kckylechen1/zeroclaw/blob/master/AGENTS.md); product direction is in [ADR-013](../architecture/decisions/ADR-013-channels-as-gateway-clients.md) and [ADR-017](../architecture/decisions/ADR-017-personal-agent-body-edges-and-delegation.md).

## Where new types go

`zeroclaw-config` is on `agent-runtime`'s mandatory dependency path. Moving
code between files in this crate does not remove downstream dependencies or
macro expansion. The extraction tracked in [#689](https://github.com/kckylechen1/zeroclaw/issues/689)
starts from `schema.rs` at 18,210 lines on `d95302025684`; its goals are explicit
ownership and smaller review/conflict surfaces, not an unmeasured build speedup.

1. **Shared wire and domain types go in `zeroclaw-api`.** A type two crates both name belongs there.
2. **Logic stays where it runs.** `zeroclaw-api` holds the shape; the owning crate holds the behavior. A consumer that only reads a result must not link the runtime that produces it.
3. **New config sections get their own module** under `zeroclaw-config/src/` (see `persona.rs`), not another block in `schema.rs`.
4. **Delete retired consumers first; extract retained behavior in bounded slices.**
   Keep declarations, serialization attributes and defaults together. State,
   validation, saving, loading and permission policy need named behavior owners;
   new behavior must name its module and existing state owner in the issue/PR
   before implementation. Do not add another behavior block to `schema.rs` by
   default or split declarations merely to claim compilation savings.
5. **Preserve contracts during mechanical extraction.** Keep existing public
   schema paths, deep-copy snapshot semantics, explicit log components and
   serialization/default behavior. Schema unit tests stay in
   `zeroclaw-config/src/schema/tests.rs`; only necessary imports and source-scan
   inputs change, without weakening assertions or coverage. Use one dependent PR
   per slice and retain the parent boundary for restacking.
6. **Separate extraction from declaration splitting.** The approved sequence is
   state, validation, saving, then loading. Inventory any remaining configuration
   resolution and permission-policy methods instead of calling them declarations.
   Split declarations by domain only after the affected config surface and its
   overlapping changes are stable. Current configuration is V3-only under the
   owner-approved retirement below. This is not a project-wide upstream freeze.

## Upstream relationship

### Current-only configuration (#382 / #647)

The owner retired V1/V2 configuration support. `zeroclaw-config::current`
accepts explicit V3 input; missing, invalid, older and future versions fail
before migration, salvage defaults or persistence can reinterpret them. The
V1/V2 lenses, conversion chain, filesystem upgrade, versioned generator,
`config migrate` command and gateway migration endpoint are removed. Native V3
generation, read-only `config validate`, section recovery with security
exposure gates, current retirement guards, secret handling and TOML save
semantics remain. The retained V3 shared-skills layout helper belongs to
`install`; TOML synchronization belongs to `toml_document`.

Upstream comparison commit
`cda933e6b9937deacbcc0d4d2cd7f1ae5bb4500e` still supports a V4 migration chain.
That behavior is deliberately skipped: this fork's equivalent is its V3
parser, generator and persistence boundary, and the owner no longer requires
historical input conversion. The development templates adapt upstream's native
provider shape, including the Ollama endpoint in `uri` instead of a credential
field, while preserving this fork's policy and runtime settings. This choice
does not freeze deliberate upstream ports or retire metadata consumers.

### Config discovery without display values (#647)

`Configurable` now derives `prop_schema()` from the existing field annotations.
Its shared `PropFieldSpec` carries paths, kinds and security classification;
`prop_paths()` is the path-only projection. Env lookup, config writes and
credential reports use this core view. The existing `prop_fields()` adapter
still owns formatted values, redaction and form presentation. Alias and channel
discovery reads typed maps and booleans, preserving complete stored alias keys.

Upstream comparison `cda933e6b9937deacbcc0d4d2cd7f1ae5bb4500e` still uses
value-bearing `prop_fields()` for these walks, including alias path splitting
and display-string channel state. There is no upstream core-view implementation
to port; this fork extends its existing derive and typed provider iterators.
The change removes that consumer coupling without adding another schema
registry or claiming a measured build/runtime speedup.

### Desktop backend continuity

Compared with upstream `zeroclaw-labs/zeroclaw#11523`, merged as
`ee04d0d44142674886c880e9f809838b49804aca`, this fork adapts abandoned-turn
settlement at the gateway owner. It does not import upstream's broader live
config authority. The existing conversation hub owns detached turns, while
`SessionBackend` owns history and request receipts. Only gateway-prefixed
running rows are settled, after exclusive data-directory ownership; failure
refuses startup. The empty lock file contains no second task ledger.

Upstream comparison head `87461d1ac5c9005b82216a223010471e63ab6718` has no
selected equivalent for this fork's shared-conversation approval recovery.
The owner approved retaining pending tool prompts through client detach within
the original timeout. This replaces the prior immediate offline revocation;
expiry, cancellation, deletion, restart and existing response authorization
remain fail-closed. See [runtime ownership and persistence](../architecture/runtime-state-and-persistence.md#client-detach-and-approvals).

This repository is an independent project (decision 2026-08-16). Upstream `zeroclaw-labs/zeroclaw` is a reference to port from deliberately, not a stream to track: no scheduled rebases and no wholesale merges.

- **Baseline:** history contains upstream work through upstream PR #9356; `master` `40cc158e8` (2026-08-16) is the last wholesale-aligned point.
- **Porting:** read the upstream diff, adapt it, validate it at the touched surface's risk level, and record provenance in the commit message ("ported from upstream #NNNN").
- **Dependency CVEs** stay covered by `cargo audit` and `cargo deny`.

### Completed tool failure recovery (#382)

Boundedly adapted from upstream [#11507](https://github.com/zeroclaw-labs/zeroclaw/pull/11507),
merged as `fcc23a22f6eaf199408fbb58377abe551c7d3e51`. Configured Agent turns use
completed canonical dispatch evidence: repeated identical failures prompt a
correction and allow one recovery retry before a visible, recoverable stop.
Completed results and receipts remain in the existing turn history. Successful
polling is advisory only; approvals, hooks and cancellation keep their existing
authority. The fork retains its extracted turn modules, raw outcome data path,
legacy channel detector and outer steering behavior. Only the existing live
daemon config handle is retained for pacing refresh; no upstream lifecycle,
additional configuration key, durable ledger or orchestration layer is imported.
See [runtime internals](../agents/internals.md#repeated-tool-failure-recovery).

### Deliberate behavioral divergences

| Divergence | Upstream | Status |
|---|---|---|
| `always_ask` outranks Full autonomy | returns `Approved` for Full before consulting `always_ask` (fail-open) | deliberate |
| Risk-profile `allowed_tools`: absent is not empty | maps `[]` to `None`, which is unrestricted | deliberate |
| Cron `allowed_tools = []` means deny-all | asserts the opposite in a test | deliberate |
| `ModelProvider::set_credential` with real 429 rotation | logs "cannot apply … Retrying with original key" | deliberate |
| WeChat state persistence is atomic and non-blocking | blocking, non-atomic write | deliberate |
| Portable memory kernel behind `memory-tachi` (`zeroclaw-memory/tachi`), default backend `tachi` | absent (defaults to SQLite) | deliberate; `tachi` is default in `zeroclawlabs`, while `--no-default-features`, `--features slim-control`, and default `zeroclaw-memory` stay AGPL-free |
| `vault:<NAME>` secret references resolved from Tachi Vault and preserved on disk | `op://` only at `87461d1ac5` | deliberate; Tachi Vault owns credentials |
| HyperMemory custom CRUD backend | absent | retired; never re-add |
| Builtin `calculator` / `weather` tools (`crates/zeroclaw-tools/src/{calculator,weather_tool}.rs`) | present | retired; implementations deleted, names reserved in `RETIRED_OPERATOR_TOOL_NAMES`. Weather, if ever needed again, is an HTTP skill / MCP wrapper, not a builtin |
| `knowledge` graph tool + `knowledge.db` + `[knowledge]` / `[knowledge_bundles]` config | present | retired; relationship memory belongs to the Tachi memory kernel (`memory-tachi`), not a second local graph. Leftover sections warn via `RETIRED_CONFIG_SURFACES` tombstones |
| GitHub Copilot provider family (`copilot.rs`, `CopilotModelProviderConfig`) | present | retired (#673 pattern); `copilot` / `github-copilot` sit in `RETIRED_MODEL_PROVIDER_NAMES`, so old configs fail closed with an explicit migration error instead of silently dropping the OAuth token |
| Browser tools (`browser.rs`+`browser/tests.rs`, `text_browser.rs`, `browser_open.rs`, `[browser]`+`[text_browser]` config) | present | retired; web interaction stays on `web_fetch`/`web_search_tool`/`http_request`, heavy automation goes to external harnesses via Tachi. `[browser]`/`[text_browser]` warn via `RETIRED_CONFIG_SURFACES` tombstones; names in `RETIRED_OPERATOR_TOOL_NAMES`; screenshot tool retained |
| OpenRouter chat provider family (`openrouter.rs`, `OpenRouterModelProviderConfig`) | present | retired; reached via a `custom` (OpenAI-compatible) slot at `https://openrouter.ai/api/v1` instead. `openrouter` is in `RETIRED_MODEL_PROVIDER_NAMES` (fail-closed). **Retained on purpose**: `openrouter_catalog.rs` + `openrouter_vendor_prefix` (public-catalog model discovery & pricing for compatible families), the `[memory]` embedder `"openrouter"` arm (embeddings axis), and OpenRouter-as-compatible-gateway prose. V1/V2 configuration input is no longer supported |

### Known gaps

- **MCP `__` auto-admit is opt-in.** The default `mcp_discovered_tool_policy` is `explicit_only`. Setting `auto_admit` on a risk profile admits any unlisted `<server>__<tool>`, so keep write tools off the MCP servers such a profile connects to.
- **Approvals.** Local grants persist in `data_dir/approvals.db` with an `approval_audit` trail. Node grants and receipts are not built yet (#62).

## ADR-023 voice consent and projection

The legacy `gateway-voice-duplex` event logger and its `VoiceDuplexConfig`
are retired under ADR-023 §8. The existing retired-transport and config-warning
registries identify both `channels.voice_duplex` and its historical
`channels_config.voice_duplex` spelling. Old event frames use the existing
unknown-message refusal and never become chat input. No old alias, excluded
tool list or grant is mapped to the current voice surface. The Node path and
the default-off `voice-surface` implementation below retain their authority.

The kernel's default-off `voice-surface` implementation adds owner consent
receipts to the existing User Model store and a closed `session_start`
projection. `UserModelStore` owns the additive table; Scope and existing
revision authority stay unchanged. The existing `persona_projection` Identity
and Principles renderers and effective Tone resolver supply the persona bytes.
The adapter, credentials and running gateway configuration are outside this
leaf. See [ADR-023](../architecture/decisions/ADR-023-realtime-voice-carrier-with-body-delegation.md)
for the API, query parameter, migration and fail-closed codes.

The locally available upstream comparison commit
`1c1cd118ac536a1dd1bc79b9cce69cbdc04e4f29` has no `UserModelStore`,
`persona_projection`, or voice export consent implementation under `src` or
`crates`. This is a fork-specific extension of the governed stores already
here, not a code port. No remote was contacted for this comparison.

## Phone candidate-history pages

The #379 history leaf extends the existing operator-only
`GET /api/user-model/candidates` route. Its sqlite User Model store remains the
single history owner. Legacy requests, including `pending=true`, keep the
existing `{ "candidates": [...] }` response with all matching history.

Opt in with `?limit=50` (accepted range: 1 through 200). Paged responses add
`next_cursor`, either `null` at the end or `{ "created_at_unix": 123, "id": "..." }`.
For the next page, pass both returned values as `before_created_at` and
URL-encoded `before_id`, retaining the same `pending` filter and `limit`.
Cursors without a limit, partial cursors, invalid limits and out-of-range
sqlite timestamps are rejected after operator authentication and before the
store opens. Bridge and Node tokens do not gain owner access.

Ordering is newest first by `(created_at_unix, id)`, including ties. Each
request selects at most `limit + 1` IDs before fetching their evidence; the extra
row determines whether another page exists. This bounds evidence materialization
without an index or schema migration. Metadata selection still scans and sorts
matching history; a page is not a database snapshot across requests. Candidates
inserted newer than the cursor appear on refresh, and pending rows reviewed
between requests disappear from subsequent pending pages.

The phone view requests 50 candidates at a time, appends unique IDs after an
explicit load, and keeps its existing rows and cursor when a page fails. Refresh,
Agent/tab changes, owner authorization failure and successful review mutations
reset the loaded view. Old in-flight reads cannot repopulate the reset view.
Gateways that ignore the new query and omit `next_cursor` produce an upgrade
message. Upgrade the gateway before serving the new web bundle; a rollback to
the older bundle keeps its legacy route contract. No data migration is needed.

### Upstream comparison for this leaf

Source inventory: upstream `zeroclaw-labs/zeroclaw` at
`329e66f76394b00af9d00b767e00ad298133961e`; fork baseline
`36b362f509a4dde72e686b4aca6b640b5537b495`.

| Source | Existing local equivalent | Decision and remaining gap |
|---|---|---|
| #9353, merged `1c7ef505b29ca28d7bcec3594c8c5c69c063c159` | Alias-scoped hydration and bounded browser transcript cache | Defer port: independent conversations, picker and per-conversation draft lifecycle remain a separate leaf. |
| #9355, merged `e7548768e0d21411008f130f324cfa06b10be4a1` | Alias-keyed `openChats` workspace | Defer port: upstream per-pane keys, session selection and sibling collision guards are not equivalent to this workspace. |
| #10544, merged `e5d556a17d4633218aa70f82ed8aab58e821a37d` | Existing authenticated HTTP attachment handles for shared intake | Defer port: the web composer has no image attachment control; preserve the fork's handle contract when adapting it. |

The pinned upstream tree has no `api_user_model.rs`, `Review.tsx` or `review.ts`
equivalent. This pagination leaf extends the fork's existing owner-review
surface and does not port the unrelated upstream lifecycle or upload changes.
It adds no endpoint, configuration, store or dependency, and retires no route.
Phone pairing/chat/attachment and PWA installability acceptance remain open.

## Hyperion (dormant)

Hyperion, the owner's quantitative-trading system, is not an active consumer of this fork (#267 to #269 are closed). If that work resumes, these rules still apply:

1. Memory goes through typed hapi-edge facade actions only. Never connect to a memory backend directly, never touch `data/hapi.db`, and never re-add HyperMemory.
2. Trading tools go through the hapi-edge MCP server only, never direct broker or data-vendor calls.
3. Real position writes require human OTP confirmation.
4. Timezone `Asia/Shanghai`; A-share lot 100 shares (STAR: minimum 200, then 1-share increments).

The companion stores (#49) are a separate, feature-gated, in-process exception.
The optional shared Portable Kernel (#560) supplies persistence for the Tachi
memory backend and companion store, with distinct exact roles and separate
database files. They never share a live database with, or connect to, a network
memory service. Hypermem remains Hyperion's adapter; the shared kernel does not
own ZeroClaw's Soul, User Model, personality policy or Tachi delegation bridge.

### Portable kernel adoption (#560)

`memory-tachi` pins `portable-kernel` 1.9.0 at Tachi revision
`b8b10642372ac6910afb2e2bab5265c1d2041a9e`, without default/admin features.
This retains the SQLite 3.54.1 security backport at
`c35ace357375f0e3a9db32d7a3bb529507a72291` and the vault-kit system-entropy
backport at `a4147e40e937f2863cd0e124d64061defe0ed433` from
[Tachi #2319](https://github.com/kckylechen1/Tachi/pull/2319).
The vault nonce and salt sizes, encrypted formats and fail-closed entropy
behavior remain unchanged. It does not adopt unrelated Tachi main changes.
The support workspace also retains the dependency security fixes from
[Tachi #2323](https://github.com/kckylechen1/Tachi/pull/2323), the exact
metadata key candidate streaming backport from
[Tachi #2320](https://github.com/kckylechen1/Tachi/pull/2320)
(`find_metadata_key_candidates`), and the scoped recent-read backport in
[Tachi #2322](https://github.com/kckylechen1/Tachi/pull/2322).
The scoped recent-read contract applies host agent, session, namespace and
lexical time predicates before the SQL limit, while the adapter retains its
final host admission check. Large agent allowlists use bounded queries and merge
their results before applying the global limit; each query hydrates at most that
limit of full rows.
The SQLite backport uses the previous
`b0c754a0cd3e4e7ffd0199691e08954942b492bd` baseline, adapting the native dependency
and connection APIs from [Tachi #2284](https://github.com/kckylechen1/Tachi/pull/2284).
It retains the snapshot-reopen fix in
[Tachi #2205](https://github.com/kckylechen1/Tachi/pull/2205), on top of the shared
kernel contracts in [Tachi #2198](https://github.com/kckylechen1/Tachi/pull/2198).
Generic memory requires `PortableKernel` / `zeroclaw-memory`; companion requires
`PortableKernel` / `zeroclaw-companion`. Existing runtime and status-probe paths
never upgrade a schema or adopt an unowned store. Backend/config names and
database locations remain unchanged.

Upstream comparison:
`zeroclaw-labs/zeroclaw@7f39e313e7fe554da1c85a83a4b5f00f6ef537e4` has no
Tachi/memcore/Portable Kernel adapter in its tree or memory dependency manifest.
This implements the fork's existing owner boundary, with no upstream code port.
The standalone `memcore` runtime dependency is replaced only after preserving
the existing memory, enrichment, governance and companion consumers. No store or
configuration key is retired. See the
[operator migration procedure](../setup/portable-memory.md).

## Yonbo device host ownership

The legacy N3B1 Yonbo device crate is retired from the ZeroClaw workspace. Its
complete 11-file source snapshot is preserved in [Zeroclaw-Robotic](https://github.com/kckylechen1/Zeroclaw-Robotic/tree/main/integration/zeroclaw/source/crates/zeroclaw-yonbo-node),
which owns future device engineering. N3B1 is distinct from Gateway Node v2.
This source retirement changes no running device, credential, or Gateway Node
contract and does not establish device replacement or physical acceptance.

## Restricted device chat admission

The #495 leaf under #377 adds an optional `gateway.bridges.<name>.device`
policy to the existing shared chat route. Its configuration is the authority
owner; the existing session backend and Conversation remain the history and
turn owners. See [restricted device bridges](../gateway/api.md#restricted-device-bridges)
for bindings, per-operation grants, revocation and downgrade discipline.
Chat authority never grants owner approval or Operator learning provenance.
ADR-023 Amendment 1 additionally binds chat policy to `device.risk_profile`,
reserves each device namespace, omits private prompt/memory context, denies
approval prompts, and caps tool rounds at three independently of surface.
The optional profile field is required for chat, including voice admission.

Upstream `zeroclaw-labs/zeroclaw` was inspected at
`5d4b18d335780af8bf15979b2f45e541f91c5eb9`, against fork baseline
`79a8320132f5866b3846ca561eaaf054241d37b5`. Upstream `src/gateway/ws.rs` and
`src/gateway/ws_approval.rs` contain chat/approval transport, but no equivalent
restricted bridge policy. This fork already owns shared Conversations, bridge
credentials and session scopes, so this leaf extends those owners rather than
porting the upstream session implementation. No existing route or consumer is
retired. Turn streaming/recovery, adapter integration and Node invocation remain
separate slices; #377 and #380 remain open.

## Non-Telegram turn control

The #497 leaf adds opt-in request/turn correlation to the existing shared
Conversation, following restricted bridge authority in #495. It does not borrow
Telegram source identifiers, replace the session ledger, or add another Agent.

Upstream comparison: `zeroclaw-labs/zeroclaw` at
`5d4b18d335780af8bf15979b2f45e541f91c5eb9`,
`crates/zeroclaw-gateway/src/ws.rs`; fork baseline `eaf007c95`.
The upstream socket lifecycle and cancel-token helpers are not equivalent to
this fork's shared Conversation and durable request receipts. Retain the local
owners and extend their transport boundary; do not port a second runtime.
The extension's bounded in-process replay cache is owned by ConversationHub.
It is not a durable input queue or an execution-retry promise. Missing recovery
state is unknown, never permission to repeat effects.

## Shared session deletion

Deleting a persisted gateway session also clears the live Agent history shared
by every socket on that session. ConversationHub owns a short-lived per-session
lifecycle mutex; the existing session execution queue still serializes turns
and their persistence. Deletion cancels active turns, waits for their writes,
then clears the backend and each live Agent under those guards. Local reset
revisions reject pre-deletion claims, continuations and unread conversation
frames for both legacy and negotiated sockets. Bytes already written to a
client cannot be retracted. Request tombstones and
the process-wide turn-control epoch are preserved; other sessions are unaffected.
With persistence disabled, DELETE retains its existing 404 response and does not
implicitly reset an in-memory conversation. This does not delete Soul, User Model
or the agent's long-term memory.

Upstream `zeroclaw-labs/zeroclaw` at
`641b766030805edfa999bb5aa6ff19a9ffd4339b`,
`crates/zeroclaw-gateway/src/ws.rs`, constructs an Agent per socket and has no
ConversationHub. The fork already owns shared conversations and replay receipts;
the gap is deletion of their retained live history. This is a bounded local fix,
with no upstream code port, new durable store, protocol or configuration key.

## Deferred MCP exact-name recovery

The #382/#376 leaf compares upstream `zeroclaw-labs/zeroclaw` at
`641b766030805edfa999bb5aa6ff19a9ffd4339b` against fork baseline
`fc9c8698b8f2558ac0d4e40320c756be27614b42`. Upstream's
[tool executor](https://github.com/zeroclaw-labs/zeroclaw/blob/641b766030805edfa999bb5aa6ff19a9ffd4339b/crates/zeroclaw-runtime/src/agent/tool_execution.rs)
resolves static and already-activated tools; its
[MCP wrapper](https://github.com/zeroclaw-labs/zeroclaw/blob/641b766030805edfa999bb5aa6ff19a9ffd4339b/crates/zeroclaw-tools/src/mcp_tool.rs)
forwards arguments to the server. This fork already owns deferred MCP stubs,
activation, tool search and access policy. The bounded gap is recovery when a
model calls a known, authorized deferred name before searching, plus compact
argument feedback. No upstream catalog or runtime is wholesale-ported.

The existing conversation `ActivatedToolSet` now owns the deferred set, policy
and activation notification shared by `tool_search` and exact-name recovery.
The MCP registry remains the connection/dispatch owner; the runtime retains
approval, hooks, current-turn exclusion and execution ownership. No new store,
configuration or default tool is added. Node/native catalogs are outside this
leaf. No consumer or dependency is retired.

OpenClaw's
[tool-search contract](https://github.com/openclaw/openclaw/blob/fe2b1a51b41b80941fe93cfb0dfbdc0def2401d1/docs/tools/tool-search.md)
is a mechanism reference for bounded discovery and recovery, not a code port.
OpenClaw's dispatcher contract differs; ZeroClaw keeps its existing tool loop and
only activates exact MCP names already admitted by its own policy. See
[MCP loading and argument recovery](../tools/mcp.md#deferred-loading) for behavior
and validation limits. Reverting this leaf restores search-first loading; no
persistent data migration is required.

## Default Tachi memory and retired SQLite / Lucid backends (#382 / #647)

Relative to immutable upstream decisions
[ADR-005](../architecture/decisions/ADR-005-pluggable-memory-backends.md) and
[ADR-010](../architecture/decisions/ADR-010-memory-authority-boundaries.md),
this fork converges default conversation memory on the embedded Tachi
(`portable-kernel`) store:

- `zeroclawlabs` enables `memory-tachi` by default and defaults
  `[memory].backend` and `[agents.<alias>.memory].backend` to `tachi` (`Tachi`).
  `zeroclaw-memory` keeps default features off, and both
  `--no-default-features` and `--features slim-control` remain 100% AGPL-free.
- `SqliteMemory`, `LucidMemory`, `MEMORY_SNAPSHOT.md` export/hydration, SQLite
  v3 memory migration, and `[storage.sqlite]` / `[storage.lucid]` configuration
  sections are retired.
- Parse-only config tombstones stay in `zeroclaw-config` (`sqlite`, `lucid`,
  `postgres`, `qdrant`, `[storage.*]`, `memory.snapshot_enabled`,
  `memory.snapshot_on_hygiene`, `memory.auto_hydrate`), emitting structured
  warnings and failing memory construction explicitly with guidance to run
  offline `zeroclaw memory migrate-native` (`--source`, `--destination`,
  `--approved-by`) rather than silently falling back.
- Non-memory SQLite stores remain untouched (`zeroclaw-infra` sessions, ACP
  sessions, approvals, Soul, User Model, audit, cron, devices, and knowledge
  graph).
- Upstream `zeroclaw-labs/zeroclaw` retains `SqliteMemory` and `LucidMemory`
  as built-in backends; do not port SQLite or Lucid memory re-introductions back
  into this fork without an explicit architectural reversal.
