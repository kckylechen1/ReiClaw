# Skills

Skills are reusable instructions and optional tool definitions that ZeroClaw can load into an agent session. Use them for repeatable workflows such as code review checklists, deployment runbooks, support playbooks, or domain-specific tool wrappers.

Skills live in one of three locations:

- Per-agent workspace skills under `<install>/agents/<alias>/workspace/skills/<name>/`.
- Shared skill bundles under `<install>/shared/skills/<bundle>/<name>/`. Agents load these when their config lists the bundle in `agents.<alias>.skill_bundles`.
- The global skill directory under `<install>/data/skills/<name>/`. The CLI can install there as a fallback, but agents do not load global skills automatically.

Use bundles for skills an agent should load during runtime. A bundle is configured under `[skill_bundles.<alias>]`; when its `directory` is omitted, ZeroClaw resolves it to `<install>/shared/skills/<alias>/`.

```text
<install>/shared/skills/<bundle>/<name>/
```

For hand-authored local skills, use `SKILL.md` or `SKILL.toml`. Use `SKILL.md` for instructions plus simple metadata. Use `SKILL.toml` when the skill needs structured prompts or tool definitions. ZeroClaw also understands `manifest.toml` for registry-style skill packages, but `SKILL.md` and `SKILL.toml` are the recommended local authoring formats.

To share one set of skills across agents, point a `[skill_bundles.<alias>]` entry at a directory of skills and assign the bundle to the agents that need it.

## Create a Markdown skill

Create a bundle, then scaffold an instruction-only skill into it:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills bundle add ops
zeroclaw skills add release-check \
  --bundle ops \
  --description "Check release readiness before tagging" \
  --edit
```

</div>

The `skills add` command writes `SKILL.md` under the resolved bundle directory and opens it in your editor. Replace the generated instructions with the workflow you want the agent to follow:

```markdown
# Release check

Review the release notes, changelog, version tags, and migration notes before confirming that a release is ready.
```

The directory name becomes the skill name. ZeroClaw uses the first non-heading paragraph as the description when no frontmatter description is present.

`SKILL.md` also supports simple frontmatter for metadata:

```markdown
---
name: release-check
description: Check release readiness before tagging
version: 0.1.0
author: zeroclaw_user
tags: [release, docs]
---

# Release check

Review the release notes, changelog, version tags, and migration notes before confirming that a release is ready.
```

Supported frontmatter fields are `name`, `description`, `version`, `author`, and `tags`.

## Create a TOML skill

A skill can also be a structured TOML manifest (`SKILL.toml`). The `[skill]` table requires `name` and `description`; `version` defaults to `0.1.0` when omitted; `author`, `tags`, and `prompts` are optional. Tool entries may use `kind = "shell"`, `kind = "http"`, or `kind = "script"`. Keep tool descriptions narrow and concrete so the model knows when to use them.

### Slash command options and localizations

A skill tagged `slash` is surfaced as a chat-channel slash command (e.g. `/search`). It may declare typed `[[skill.slash_options]]`; a skill that declares none falls back to a single required free-text input. Both the command description and each option description accept an optional `description_localizations` map keyed by locale code. Unknown or unsupported locale codes are dropped with a warning rather than failing registration, so a typo never wedges command registration.

```toml
[skill]
name = "search"
description = "Search the web"
tags = ["slash"]
# Localized command descriptions, keyed by locale code.
description_localizations = { fr = "Rechercher sur le web", ja = "ウェブを検索" }

[[skill.slash_options]]
name = "query"
description = "The search query"
type = "string"
required = true
# Localized option descriptions, same form.
description_localizations = { fr = "La requête de recherche" }
```

## Manage installed skills

List the full inventory:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills list
```

</div>

List exactly what one agent loads at runtime:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills list --agent default
```

</div>

List one bundle directly:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills list --bundle ops
```

</div>

Audit an installed skill or a local skill directory:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills audit release-check
zeroclaw skills audit ./release-check
```

</div>

Install a skill from an explicit local directory or Git URL:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills install ./release-check --bundle ops
zeroclaw skills install https://example.com/zeroclaw-release-check.git --bundle ops
zeroclaw skills install ./release-check --agent default
```

</div>

Install one skill by name from a Git catalog repository (a repo whose skills live under `skills/<name>/`):

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills install https://github.com/vercel-labs/skills --skill find-skills
```

</div>

Install destination precedence is:

1. Explicit `--bundle <alias>`.
2. The target agent's single assigned bundle. `--agent <alias>` chooses the target agent; when omitted, ZeroClaw uses the active runtime agent.
3. The global directory under `<install>/data/skills/`.

If the target agent has multiple bundles, pass `--bundle` so the destination is unambiguous. If ZeroClaw falls back to the global directory, the skill is installed and listed, but no agent loads it automatically. Attach it to a bundle to make it available at runtime.

Remove an installed skill:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills remove release-check --bundle ops
zeroclaw skills remove release-check --agent default
```

</div>

Removing from a bundle archives the skill directory so it can be recovered. Removing from the global directory deletes the global copy after the existing path-containment checks pass.

Skill authoring and test execution belong in the external coding workbench.
The in-process `skills test` command and automatic creation or review loops are
retired; run reviewed tests through the normal authorized development workflow.

If `zeroclaw skills list` shows a skill but the agent does not use it, check the runtime view:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw skills list --agent default
```

</div>

When the skill appears only in the global group, install it into a bundle and ensure the agent lists that bundle in `agents.<alias>.skill_bundles`.

## Script safety

ZeroClaw audits skills before loading or installing them. Script-like files such as `.sh`, `.bash`, `.ps1`, and files with shell shebangs are blocked by default.

If you intentionally use script-bearing skills, enable `skills.allow_scripts`. Keep this disabled unless you trust the skill source and have reviewed what the scripts do.

For Python-specific execution patterns, interpreter policy, and native versus Docker trade-offs, see [Running Python skills](./python-skills.md).

## Registry retirement and local loading

ReiClaw no longer clones or refreshes open-skills, the default skill registry, or
extra registries during loading or bare-name installation. Existing checked-out
files are left in place. To keep a reviewed skill available, install its local
directory into an assigned bundle or explicitly install from a Git URL as shown
above. Registry-name and `registry:<name>/<skill>` sources no longer install.

Remove the retired `skills.open_skills_enabled`, `skills.open_skills_dir`,
`skills.registry_url`, `skills.extra_registries`, `skills.skill_creation`,
`skills.skill_improvement` and `skills.install_suggestions` settings. Config-file
and `REI_skills__*` overrides report retirement warnings, including the legacy
`REI_OPEN_SKILLS_ENABLED` and `REI_OPEN_SKILLS_DIR` environment variables.

The retained loader reads and audits the current workspace and assigned bundle
directories on each load. It preserves bundle inclusion rules and workspace
precedence; script policy is still controlled by `skills.allow_scripts`. The
existing skill API, bundle editor and scaffold commands remain available until
the external workbench cutover.

## Prompt injection

The default `full` mode includes skill instructions in the system prompt. Use
`compact` to include metadata and read details on demand through `read_skill`:

```toml
[skills]
allow_scripts = false
prompt_injection_mode = "compact"
```

## See also

- [Tools overview](./overview.md)
- [Security overview](../security/overview.md)
- [Tool receipts](../security/tool-receipts.md)
