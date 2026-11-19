# Claude Code Skills

The repo ships a set of [Claude Code skills](https://docs.claude.com/en/docs/agents/skills) under `.claude/skills/` that automate filing issues, opening PRs, squash-merging, and changelog generation.

Each skill lives in its own directory with a `SKILL.md` file. Claude Code loads them automatically when you open the repo; invoke them by describing what you want in plain language, or by explicit reference (e.g. `/squash-merge 1234`).

## Available skills

| Skill | Use it when |
|---|---|
| `github-issue` | Filing a structured issue (bug report or feature request) |
| `github-pr` | Opening or updating a PR with a fully-populated template body |
| `squash-merge` | Landing an authorized, ready PR into `master` by squash, with final-head matching and verified CI/review policy |
| `changelog-generation` | Preparing `CHANGELOG-next.md` for a release: summarises merges since the last tag |
| `skill-creator` | Creating, editing, or benchmarking the skills themselves |
| `zeroclaw` | Operating the running ZeroClaw instance (CLI + gateway API) |

## Squash-merge strategy

Squash is this fork's landing policy. Verify that the live repository merge options match that policy; editing these documents does not change GitHub settings. The `squash-merge` skill uses GitHub's merge operation with a conventional subject, original commit provenance in the body, and a final-head precondition. A provenance list does not preserve the original commits as ancestry.

### Why the skill exists

The skill makes repository binding, authorization, final-head validation, and merge-result verification consistent. GitHub supports [configurable default squash messages](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/configuring-commit-squashing-for-pull-requests); using its defaults is acceptable when the resulting message meets project conventions. There is no blanket ban on the merge UI.

Automation uses `gh pr merge --match-head-commit --subject --body-file`, or an equivalent explicitly bound connector/API operation. Never direct-push a squash to `master` as a substitute for the PR merge process, and never assemble multiline public text as shell source.

### Format

- **Subject:** `<PR title> (#<number>)`: must be conventional commits with a scope (`feat(scope): ...`, `fix(scope): ...`).
- **Body (multi-commit PR):** bulleted list of `- <short sha> <commit subject>` from the PR branch
- **Body (single-commit PR):** full commit body, or blank if there isn't one

### Pre-flight checks

The skill verifies the bound repository/PR, final head and base, actual required checks, policy gate, mergeability, and required reviews. `REVIEW_REQUIRED` and unresolved substantive change requests are blockers, not prompts to bypass review. Pending CI is monitored while independent work continues. Routine conflict fixes are completed when authorized, followed by review and fresh checks; otherwise the concrete blocker is reported.

Validation follows `AGENTS.md`: docs gates with an explicit actual PR base for docs-only work, the shared Cargo wrapper and owning legs for code, and extra checks only for a named gap. Missing CLI tooling can use an equivalent authenticated API; it does not justify skipping safeguards. A failed or ambiguous merge call is followed by readback before any retry.

Dependent PRs may temporarily target their parent. After a parent squash, replay only the child's increment onto the updated `master`, retarget, inspect the final diff, and validate the resulting head. See AGENTS.md rule 9.

### Invocation

```
> squash-merge 1234
```

or explicit:

```
> /squash-merge 1234
```

The skill prepares the concrete merge result before acting. If the user already authorized the bounded review/repair/merge task, it proceeds without another ritual yes. A read-only request, new target/scope, permission change, or material risk exception needs its own decision; actual GitHub protections and substantive review requirements always remain in force.

## Changelog generation

`changelog-generation` builds `CHANGELOG-next.md` for a release by querying `gh` for merged PRs since the last tag, grouping them by conventional-commits prefix, and formatting them into the house changelog style. Use it as part of the release runbook, before dispatching `release-stable-manual.yml`.

## Editing the skills

Skills are plain Markdown with YAML frontmatter. Keep the name, description, and concrete trigger phrases (`"review 1234"`, `"triage issues"`, etc.) aligned with the workflow. An authoring guide can help with structure; edit the repository-owned files and run the relevant repository Markdown checks. A repo-policy edit does not require installing or changing a personal/plugin skill.

When a skill's behaviour diverges from the governing policy, update the skill and its references together. `AGENTS.md` owns this fork's repository binding, authorization, validation, and stack policy; skills implement that policy. The review protocol continues to own substantive evidence and review formatting. Repository workflow skills are versioned with this repository, not installed into a personal skill library as part of a repo-policy edit.
