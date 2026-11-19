# Translations

ZeroClaw has two independent translation layers — app strings (Mozilla Fluent `.ftl`) and
docs (gettext `.po`). Both are filled locally via a configured AI provider before opening
a PR; translation is never a CI operation.

Use [Docs & Translations](docs/book/src/maintainers/docs-and-translations.md) for
the contributor workflow, provider configuration, coverage checks, batch tuning,
and failure handling. ReiClaw publishes no releases.
