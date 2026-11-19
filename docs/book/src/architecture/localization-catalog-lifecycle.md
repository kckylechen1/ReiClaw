# Localization catalog lifecycle

ReiClaw's CLI, prompt and tool text is English only and written as literals in the code; there are no runtime catalogs. The remaining localization branch is gettext catalogs that translate the mdBook documentation after English source and generated references have been assembled. A translated file being tracked in the repository does not prove that a particular site build consumes it. Use this page to follow a documentation catalog from English source through extraction, fill, validation and release pinning.

`locales.toml` is the registry for documentation locale codes and display labels. It drives docs locale builds and the generated language switcher.

## gettext documentation strings

English Markdown is the authored documentation source, but extraction also sees generated references, included snippets, and preprocessor output materialized for the extraction build. Before running mdBook with xgettext output, `cargo mdbook sync` calls the shared `prepare_generated_book_inputs()` path used by locale builds and single-locale serving. That path regenerates the CLI and config references, locale switcher, theme, keymap, and feature-matrix inputs from their canonical sources. Extraction also runs with the built peer-groups preprocessor, so a clean checkout does not depend on ignored files or binaries left by an earlier docs build.

`cargo mdbook sync` extracts English messages into `messages.pot`, normalizes the template, bootstraps or merges each locale without fuzzy matching, removes obsolete entries, and reports the untranslated delta. When a model provider is supplied, it fills missing translations through `tools/fill-translations`; otherwise it makes no provider calls. The maintainer guide owns the command options and operating procedure.

The fill tool treats one gettext entry as one source-to-translation mapping. It repairs or clears model responses that contain prompt leakage or a new machine-local absolute path, preserves required trailing newlines, writes incrementally, and removes fuzzy flags from accepted entries. `cargo mdbook check` separately parses every PO file and rejects suspicious generated responses, corrupted protected literals, and introduced local paths.

## Partial translation and fallback

The gettext preprocessor renders the English `msgid` when a locale has no usable translated value for that entry. A locale can therefore show translated navigation and paragraphs alongside newly added English prose. This mixed-language state means the English source advanced beyond the catalog's accepted coverage; it does not mean mdBook selected two languages for one page.

Common causes are:

- an English docs or generated-reference change added a new `msgid`;
- catalog sync merged the new source but no translation fill has run;
- a safety repair blanked a leaked, path-bearing, or otherwise unusable model response;
- a source edit replaced an old message with a new one;
- a locale catalog or release pin intentionally trails current `master`.

Fuzzy is a catalog-maintenance state, not a promise that the old value is safe to render. The current sync command disables fuzzy matching for new merges, while the fill tool can accept an existing non-empty fuzzy value and remove its flag. Review the resulting `msgstr`; do not infer publication behavior from the flag alone.

Translated locale builds disable full-text search. Only the primary locale, the first entry in `locales.toml`, receives the search index. This is a size decision in `build_locales`, not missing translation coverage.

## Catalog storage and release pins

Documentation PO catalogs live in `zeroclaw-labs/zeroclaw-docs-translations`, mounted at `docs/book/po` as a git submodule. The main repository records one gitlink commit, not each PO file. `messages.pot` and translation failure logs are generated artifacts and are not part of the pinned catalog set.

The release helper `scripts/release/refresh-translations.sh` owns the translation tag and main-repository gitlink update. By default it runs sync and the catalog check, commits and pushes catalog changes in the submodule, creates and checks out the matching `v<version>` tag, and stages the gitlink. Its `--no-translate` mode skips both sync and the catalog check, so it is only appropriate after the current catalogs have been validated separately. The translation-pin workflow initializes the exact pinned commit, checks PO syntax, and verifies that locale catalogs expose the same `msgid` set.

## Validation and review boundaries

- Ordinary English docs PRs may defer broad PO churn to a focused translation-cache pass. Review the English source and generated boundary in the original PR.
- Include PO changes when translation or catalog maintenance is the purpose, a locale is being added, the generated delta is small and reviewable, or a release pass is advancing the pin.
- Keep protected command syntax, config keys, product names, JSON/TOML literals, and placeholders intact. Translate surrounding prose rather than weakening machine-facing examples.
- Treat an English fallback as visible evidence of missing accepted catalog coverage. Fix or fill the catalog source, not the rendered HTML.
- Review submodule changes as release/catalog operations: inspect both the main-repository gitlink and the catalog commit it selects.

For detailed commands, provider configuration, batching, adding a locale, and release procedure, see [Docs & Translations](../maintainers/docs-and-translations.md). For the English source and generated-reference stages that feed gettext extraction, see [Generated documentation pipeline](./generated-documentation-pipeline.md).

## Source pointers

- Locale registry: `locales.toml`
- gettext extraction and merge: `xtask/src/cmd/mdbook/sync.rs`
- gettext safety checks: `xtask/src/cmd/mdbook/check.rs`
- gettext fill and repair: `tools/fill-translations/`
- Locale build and search behavior: `xtask/src/cmd/mdbook/build.rs`
- Release catalog refresh: `scripts/release/refresh-translations.sh`
