# Concepts

These are the terms Quickstart and the config reference assume. Each one has a deeper page linked at the end of its definition.

**Alias.** An alias is the name you assign to a configured instance, then
reference elsewhere to point at it. You choose the name freely; other parts of
the config wire things together by that name. Aliases are lowercase ASCII
letters, digits, and single underscores, must start and end with a letter or
digit, and cannot contain `__` or hyphens. In the config they appear as the
`<alias>` segment of a section header, such as `[agents.<alias>]` or
`[providers.models.<type>.<alias>]`. See
[Reference → Environment variables → Alias grammar](../reference/env-vars.md#alias-grammar).

**Model provider.** A model provider is ZeroClaw's abstraction over an LLM
endpoint. Every chat-completion request goes through a provider, whether the
target is a remote API, a self-hosted server, or a local Ollama model.
Providers are typed by vendor family, and you can run several named instances
of the same family. In the config each one lives at
`[providers.models.<type>.<alias>]`. See
[Model Providers → Overview](../providers/overview.md).

**Risk profile.** A risk profile is a named autonomy and sandbox posture. Its
level is `readonly`, `supervised` (the default), or `full`, controlling whether
tools run automatically, prompt for approval, or are blocked. Each agent
references exactly one risk profile. In the config it lives at
`[risk_profiles.<alias>]`. See
[Security → Autonomy Levels](../security/autonomy.md).

**Runtime profile.** A runtime profile is reusable operational tuning: agentic
mode, tool-iteration caps, action and cost budgets, timeouts, context limits,
and delegation policy. It is separate from the risk profile, which governs
autonomy. Quickstart installs the `unbounded` preset for new agents; adjust the
fields afterward. In the config it lives at `[runtime_profiles.<alias>]`; `rei markdown-schema`
lists its fields.

**Multi-agent.** ZeroClaw runs many agents from one install. Each agent has its
own set of references (risk profile, model provider, channels), its own
workspace directory, and its own memory backend. An agent can spawn an
ephemeral **SubAgent** that inherits its parent's identity and security policy,
and agents can talk to each other when they share a **peer group**. In the
config each agent is an `[agents.<alias>]` block. See
[Agents → Runtime internals](../agents/internals.md).

**Peer group.** A peer group declares an opt-in cross-agent communication set on
a single channel. Membership is mutual: two agents are peers only when both
appear in the same group's member list. In the config it lives at
`[peer_groups.<name>]`. See
[Agents → Runtime internals](../agents/internals.md).

**Skill bundle.** A skill bundle is a reusable group of skills attached to an
agent or channel by alias. It controls which skills load and from where: a
source directory plus optional include and exclude lists. Skills themselves are
reusable instructions and optional tool definitions. In the config a bundle
lives at `[skill_bundles.<alias>]`. See [Tools → Skills](../tools/skills.md).
