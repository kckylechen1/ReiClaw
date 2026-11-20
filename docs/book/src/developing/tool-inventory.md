# Built-In Tool Inventory

Use this page when deciding whether an agent-callable tool should stay in the
core binary, become feature-gated, ship as a skill
package, or use an MCP or CLI-backed integration.

This is a classification map, not a removal plan. Do not remove or externalize a
tool until the replacement preserves the operator contract: config, security
policy, tool receipts, audit visibility, compatibility, and rollback.

The runtime registry source of truth is
`crates/zeroclaw-runtime/src/tools/mod.rs`, especially `default_tools`,
`all_tools_with_runtime`, and `register_skill_tools_with_context_and_runtime`.
The shared tool implementations live primarily under `crates/zeroclaw-tools/`.

## Classification Buckets

| Bucket | Meaning | Next action |
|---|---|---|
| Keep built-in | Part of the baseline agent contract or tightly coupled to runtime policy, receipts, memory, sessions, or delegation. | Keep in core unless the agent contract changes through an RFC. |
| Feature-gate candidate | First-party behavior still belongs in ZeroClaw, but the dependency, platform, binary-size, or operator-risk cost should not affect minimal builds. | Add or tighten a feature/config gate before considering removal. |
| Externalize later | Useful capability, but the long-term owner should be a plugin, skill package, MCP server, or external CLI because the behavior mostly wraps a product, vendor API, or optional workflow. | Keep compatibility until the external surface is real and documented. |
| No action yet | Current evidence is not enough to choose a different home. | Leave in place and revisit with source, usage, and replacement evidence. |

## Keep Built-In

These tools form the minimum local agent work surface. They are registered by
`default_tools` and again by the full registry.

| Tool(s) | Why they stay |
|---|---|
| `shell` | Executes local commands under ZeroClaw's shell policy, sandbox, runtime adapter, path guard, and receipts. |
| `file_read`, `file_write`, `file_edit` | Own the workspace file contract, persistence behavior, path guard, and audit surface. |
| `glob_search`, `content_search` | Provide local discovery without requiring shell-specific command syntax. |

These full-registry tools should also stay built in because they are runtime,
memory, coordination, or operator-control primitives rather than optional
product integrations.

| Tool(s) | Why they stay |
|---|---|
| `memory_store`, `memory_recall`, `memory_forget`, `memory_export`, `memory_purge` | Long-term memory is a first-party runtime contract and uses shared memory ownership rules. |
| `cron_add`, `cron_list`, `cron_remove`, `cron_update`, `cron_run`, `cron_runs`, `schedule` | Scheduling affects autonomous execution, ownership, and run history; keep it policy-visible in core. |
| `reasoning_subagent` | The live zero-tool Advisor is a bounded child with explicit context, lineage, and budgets. ADR-027 adds confined read-only modes and mid-run communication without handing out the parent registry or memory authority. |
| `send_message_to_peer` | Retain authorized named-peer collaboration and each recipient's identity/policy. ADR-027 replaces implicit detached launching with distinct delivery, continuation, and new-work admission; peer membership is not shared memory or tool authority. Legacy `spawn_subagent` and `delegate` remain retired. |
| `ask_user`, `escalate_to_human`, `reaction`, `poll`, `channel_room` | These are channel-bridging operator interaction primitives with late-bound channel handles and receipts. |
| `sessions_current`, `sessions_list`, `sessions_history`, `sessions_send` | Session visibility and message sending must share the daemon/gateway session backend and agent ownership boundaries. |
| `read_skill` and skill-defined tools with `kind = "shell"`, `kind = "http"`, or `kind = "builtin"` | Skills are an intended extension surface, but the runtime bridge that turns installed skills into tools is core. |

## Feature-Gate Candidates

These tools are first-party today, but they deserve explicit feature/config
boundaries because they add platform, dependency, network, or UI surface area.

| Tool(s) | Boundary | Classification |
|---|---|---|
| `browser`, `browser_open`, `text_browser` | Removed from the model surface; implementations deleted. | Retired: heavy browser automation belongs to external harnesses via Tachi; web interaction stays on `web_fetch` / `web_search_tool` / `http_request`. Names reserved in `RETIRED_OPERATOR_TOOL_NAMES`; `[browser]` / `[text_browser]` sections warn via retired-surface tombstones. |
| `http_request`, `web_fetch`, `web_search_tool` | Config-gated network access. | Keep first-party while SSRF, allowlist, provider routing, and receipt behavior remain ZeroClaw-owned. Revisit only after MCP/plugin replacements can express the same network policy. |
| SOP tools (`sop_list`, `sop_execute`, `sop_advance`, `sop_approve`, `sop_status`, `sop_workshop`) | Removed with the SOP run side. | Retired: no agent-facing SOP run tools remain; runs are Tachi-side ProcedureRuns through the procedure_v1 seam. |
| WASM plugin tools | Removed with the WASM plugin host. | Retired: extensions go through MCP servers. |
| `calculator`, `weather` | Removed from the model surface; implementations deleted. | Retired outright. The names stay reserved in `RETIRED_OPERATOR_TOOL_NAMES` so no assembly path can re-register them. A weather lookup, if ever wanted again, belongs to an HTTP skill or MCP wrapper. |
| `knowledge` | Removed from the model surface; tool, graph store and config retired. | Retired: its private `knowledge.db` graph was a second store next to the Tachi memory kernel's own edges. `[knowledge]` / `[knowledge_bundles]` sections warn via the retired-surface tombstone. |
| `backup`, `data_management` | Implementations and operator routes retired. | Host-managed backup and retention own the active config/data directories. Existing archives remain on disk; `[backup]` / `[data_retention]` warn and schedule no work. |
| `screenshot` | Captures the retained visual/UI surface. | Uses the shared filesystem and policy guards. |
| `security_ops` | Retired from the model surface. | Default-off diagnostics with no operator surface; the module stays compiled but unreachable, and enabling the section only emits a startup/reload notice. See "Retired Operator Tools" below. |
| `verifiable_intent` | Config-gated trust policy. **The `vi_verify` tool is temporarily withheld from the model-visible registry.** | Keep gated and first-party; intent issuance and verification affect trust policy and should stay first-party until the credential boundary is stable. No chain verifier exists yet, so `vi_verify` is not registered even when `verifiable_intent.enabled = true`; enabling the section now only emits a warning naming that gap, at process startup and again on each daemon reload. The issuance and verification library paths are unchanged. Restore registration only behind a verify-and-evaluate path that consumes a verified chain result. |

## Retired Operator Tools

These first-party tools are no longer part of the model-visible registry on any
assembly path (ordinary, SubAgent, skill elevation, minimal or full
composition). The capability moved to a trusted surface or was deleted.
The `model_routing_config`, `model_switch`, and `proxy_config` implementations
are deleted; their names stay on the retired list, and the registry totality
test asserts that no built-in assembly path registers them. The list does not
block an MCP or plugin tool that reuses one of these names.

| Tool(s) | Trusted surface today |
|---|---|
| `model_routing_config` | `GET/PUT/DELETE /api/config...` (gateway config API, settings UI). Mutation actions additionally reject raw `api_key` values. |
| `model_switch` | Explicit owner selection. The web model selector and `/model <type>.<alias>` command write `agents.<alias>.model_provider` through the gateway config API and reconnect the chat session. The agent cannot switch its own model through a tool. |
| `proxy_config` | `PUT /api/config/proxy.*` persists the canonical `[proxy]` config; the daemon applies it at process startup (runtime global plus, for `scope = "environment"`, process env) and refreshes the runtime global on daemon reload. The `apply_env` / `clear_env` actions were deleted outright: live process-env manipulation had no consumer and restart application replaces it. |
| `security_ops` | None (default-off diagnostics, unreachable by design). `security_ops.enabled` stays parseable; enabling it emits a startup/reload notice. |
| `backup` | Host-managed backups; the in-process engine and `/api/agents/<alias>/backup*` routes are retired. |
| `data_management` | Host-managed retention; the in-process engine and `/api/agents/<alias>/data-retention*` routes are retired. |

`[backup]` and `[data_retention]` are retired config tombstones. Remove them
after arranging host-managed scheduling, encryption and retention; their old
values no longer create backups or purge data. Existing archives are unchanged.
The retired names remain reserved against external tool registration so no
extension can claim one and ride it back onto the provider wire.

## Externalize Later

These are the strongest candidates for moving out of the core binary once the
replacement surface exists. Until then, keep them compatible and policy-visible.

| Tool(s) | Likely long-term home | Why |
|---|---|---|
| `composio` | MCP server or CLI-backed integration. | These mostly wrap third-party products and authentication models that can evolve independently from the core runtime. |
| `claude_code`, `claude_code_runner`, `codex_cli`, `gemini_cli`, `opencode_cli` | Removed; retired raw launchers. | These Parent-visible raw CLI launchers were removed from the registry (#197 wall 2); harness execution goes through the typed subagent / Tachi paths instead. |

## No Action Yet

Leave these surfaces in place until another design slice produces better
evidence:

- `tool_search` and deferred MCP activation: part of the current MCP discovery
  flow, but the exact long-term boundary depends on the v0.8.2 plugin/MCP work.
- Session reset/delete tools: implementations exist, but the agent registry does
  not register the destructive unscoped variants by default. Keep that boundary
  unless an operator/admin surface explicitly needs them.

## Migration Rules

Before moving any tool out of core, the replacement must answer:

1. Which config remains first-party, and which config moves to the plugin,
   skill, MCP server, or CLI?
2. How does the replacement preserve autonomy checks, allow/deny lists, tool
   receipts, audit logs, and attribution?
3. How do existing configs fail or migrate when the built-in tool disappears?
4. Can operators see that the capability is installed, enabled, disabled,
   blocked, or missing?
5. What is the rollback path if the external package breaks?

If code proof is needed for a future slice, choose one low-blast-radius
candidate from the Externalize later table and prove the replacement path
without deleting the built-in tool in the same PR.

## Tachi delegated work

`tachi_start`, `tachi_status`, `tachi_result`, `tachi_cancel` and `tachi_watch`
are built-in L2 control tools. They use Tachi profiles and the existing session
request-binding store, with no local worker launch. They remain closed under
`[tachi]` defaults and are excluded from L1 profiles. See
[external work through Tachi](../agents/delegation.md#external-work-through-tachi)
for their request, polling and recovery contract.

## ADR-026 retired overlapping builtins

`git_operations`, `execute_pipeline`, `llm_task`, `file_upload`,
`file_upload_bundle`, `file_download`, `image_gen`, `image_info`, and `TodoWrite`
are removed from the runtime registry. Repository and file operations use the
retained policy-controlled shell and file primitives; optional network/media
workflows can be supplied by explicitly admitted MCP tools. `reasoning_subagent`
remains the bounded reasoning entry point. `task_list` keeps durable commitments,
and `schedule` retains its minimal-profile membership.

The retired `pipeline`, `image_gen`, `file_upload`, `file_upload_bundle`, and
`file_download` config sections produce retirement warnings and cannot register
a capability. Existing authentication, allowlists, approval policy, and Node or
Tachi access are not transferred to a replacement automatically.
