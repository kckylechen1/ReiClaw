# API Reference

Full rustdoc for every public type in the workspace, generated from the `///` comments on each type, function, and module. Use this when you need to know the exact shape of a struct, the methods on a trait, or what a function returns: anything the generated reference exposes better than prose can.

The rustdoc is not published; build it locally:

<div class="os-tabs-src">

#### sh

```sh
cargo doc --workspace --no-deps --open
```

</div>

## How to navigate it

- The sidebar on the left lists every crate in the workspace
- Click `reiclaw-api` first; that's where the public traits (`Provider`, `Channel`, `Tool`) live
- Use `cmd/ctrl+F` in the rustdoc page to search within a crate
- Click on any trait to see implementors across the workspace

## Crate index

| Crate | What it exposes |
|---|---|
| `reiclaw` | Top-level umbrella with re-exports |
| `reiclaw-api` | Public traits: `Provider`, `Channel`, `Tool`, `StreamEvent` |
| `reiclaw-config` | Config schema, autonomy types, secrets |
| `reiclaw-runtime` | Agent loop, security, SOP, onboarding |
| `reiclaw-providers` | Every LLM-provider implementation |
| `reiclaw-provider-dispatch` | Attributed model calls through the provider interface, without concrete providers |
| `reiclaw-gateway-client` | WebSocket client SDK for channel bridges and chat clients |
| `reiclaw-gateway` | HTTP/WebSocket gateway |
| `reiclaw-tools` | Agent-callable tools |
| `reiclaw-memory` | Conversation memory, embeddings |
| `reiclaw-infra` | Tracing, metrics |

See [Architecture → Crates](./architecture/crates.md) for a plain-English description of how the crates fit together.
