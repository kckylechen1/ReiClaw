# API Reference

Full rustdoc for every public type in the workspace, auto-generated from the `///` comments on each type, function, and module. Use this when you need to know the exact shape of a struct, the methods on a trait, or what a function returns: anything the generated reference exposes better than prose can.

**[Open the rustdoc →](/api/reiclaw/index.html)**

## How to navigate it

- The sidebar on the left lists every crate in the workspace
- Click `reiclaw-api` first; that's where the public traits (`Provider`, `Channel`, `Tool`) live
- Use `cmd/ctrl+F` in the rustdoc page to search within a crate
- Click on any trait to see implementors across the workspace

## Crate index

| Crate | What it exposes |
|---|---|
| [`reiclaw`](/api/reiclaw/index.html) | Top-level umbrella with re-exports |
| [`reiclaw-api`](/api/reiclaw_api/index.html) | Public traits: `Provider`, `Channel`, `Tool`, `StreamEvent` |
| [`reiclaw-config`](/api/reiclaw_config/index.html) | Config schema, autonomy types, secrets |
| [`reiclaw-runtime`](/api/reiclaw_runtime/index.html) | Agent loop, security, SOP, onboarding |
| [`reiclaw-providers`](/api/reiclaw_providers/index.html) | Every LLM-provider implementation |
| [`reiclaw-provider-dispatch`](/api/reiclaw_provider_dispatch/index.html) | Attributed model calls through the provider interface, without concrete providers |
| [`reiclaw-gateway-client`](/api/reiclaw_gateway_client/index.html) | WebSocket client SDK for channel bridges and chat clients |
| [`reiclaw-gateway`](/api/reiclaw_gateway/index.html) | HTTP/WebSocket gateway |
| [`reiclaw-tools`](/api/reiclaw_tools/index.html) | Agent-callable tools |
| [`reiclaw-memory`](/api/reiclaw_memory/index.html) | Conversation memory, embeddings |
| [`reiclaw-infra`](/api/reiclaw_infra/index.html) | Tracing, metrics |

See [Architecture → Crates](./architecture/crates.md) for a plain-English description of how the crates fit together.

## Regenerating the API reference

The rustdoc ships with every doc deploy. For local builds:

<div class="os-tabs-src">

#### sh

```sh
cargo mdbook refs     # generates CLI + config reference + rustdoc
cargo mdbook build    # rebuilds the full book including rustdoc bridge
```

</div>

See [Maintainers → Docs & Translations](./maintainers/docs-and-translations.md).
