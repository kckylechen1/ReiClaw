# Routing

Routing happens at the **agent layer**. Each agent points at exactly one provider; channels point at agents.

Two layers of decisions:

1. **Per-call backend selection**: "use the cheap model unless this prompt looks like reasoning." Each routing target is its own `[agents.<alias>]` entry with its own `model_provider`. Channels are routed to whichever agent should handle their traffic.
2. **Provider reliability**: vendor-redundancy lives behind a single OpenAI-compatible aggregator. Configure an aggregator such as OpenRouter as one `custom` slot and let it handle vendor fan-out at its endpoint.

## Per-agent dispatch

Define each routing target as its own agent, then point channels at the agent that should handle their traffic.

Each channel binds to one agent. Channels move between agents by editing `channels = [...]` on the agent that should pick them up; `Config::validate()` makes sure references resolve.

For bounded multi-step reasoning inside a single conversation, the `reasoning_subagent` tool runs one contract-admitted child that receives only a bounded context bundle and returns a structured report to the parent's tool loop (see the [Tools overview](../tools/overview.md)). The legacy `spawn_subagent` tool, which ran an ephemeral child under the parent's own identity and permissions envelope, is retired (see [Delegation & SubAgents](../agents/delegation.md)).

## Hint-based model routes

A narrower mechanism: `[[model_routes]]` lets an agent override the configured `model_provider` for prompts marked with a hint string. Useful when one agent should occasionally reach for a different model without spinning up a second agent. Each route entry carries a `hint` (the string a prompt must declare to fire it), a `model_provider` (the dotted `<type>.<alias>` profile to switch to, e.g. `anthropic.reasoner`), and a `model` (the provider-local model id, e.g. `claude-opus-4-7`). Configure routes through the gateway or `zeroclaw config set`; `rei markdown-schema` lists the field schema in its `model_routes` section.

Routes fire for matching explicit hints or hints selected by the configured
classifier. The default request path uses the agent's primary `model_provider`.

`model_provider` is always a provider profile reference in dotted `<type>.<alias>` form, such as `anthropic.sonnet` or `openai.default`. The profile carries the endpoint, credential reference, compatibility flavor, fallback chain, and configured default model. The `model` field is provider-local state under that profile.

### Complexity hints and the retired cost selector

The runtime profile can map the three complexity tiers to ordinary route names:

```toml
[runtime_profiles.default.auto_classify]
simple_hint = "fast"
standard_hint = "default"
complex_hint = "reasoning"
```

The former `cost_optimized_hint` field had no runtime reader and is retired.
Existing keys under `[runtime_profiles.<alias>.auto_classify]` and the matching
`ZEROCLAW_runtime_profiles__<alias>__auto_classify__cost_optimized_hint`
environment override are ignored with the `cost_optimized_routing_removed`
warning. Remove that field; there is no automatic replacement.

Names such as `cost-optimized` and `cheapest` remain valid ordinary route hints.
They select only the route explicitly configured under that name. Provider
`pricing`, live pricing and cost accounting remain available; prices do not
automatically choose a route.

## Owner model selection

Owner selection uses the same provider-profile contract as config-backed routing.
In web chat, the model selector and `/model <type>.<alias>` command persist
`agents.<alias>.model_provider` through the gateway config API, cancel the
active turn when needed, and reconnect the session with that selection.
`/model` without an argument lists the current and available selections.

The `model_switch` model tool and its in-turn switching protocol are deleted.
The agent cannot request its own provider rebuild through a tool. Configured
route hints, query classification, vision routing, and provider fallback keep
their existing dispatch behavior.

## Observability

Per-agent dispatch decisions are visible in tracing logs:

```
INFO channel=telegram.home routed to agent=fast
INFO agent=fast model_provider=anthropic.haiku turn_id=...
INFO model_provider=anthropic.haiku stream complete tokens={input=512, output=128}
```

For production deployments, wire the log output to Loki / Grafana. See [Operations → Logs & observability](../ops/observability.md).

## See also

- [Overview](./overview.md): provider model and per-agent dispatch
- [Configuration](./configuration.md): full `[providers.*]` schema
- [Provider catalog](./catalog.md): every canonical slot
