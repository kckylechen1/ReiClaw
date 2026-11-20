# Portable memory kernel

The `memory-core` feature uses the shared Portable Kernel for two
ReiClaw-owned databases. It is enabled by default on the root `reiclaw`
crate and stays optional on `reiclaw-memory`. Root builds using
`--no-default-features --features agent-runtime,gateway` exclude this AGPL
dependency; enabling those features without disabling defaults retains it. The backend name
`memorycore`, the crate feature `reiclaw-memory/memorycore`, configuration keys and
existing database locations are described below.

| Store | Owner role | Location |
|---|---|---|
| Generic memory | `reiclaw-memory` | `<data_dir>/memory/memorycore.db` |
| Companion memory | `reiclaw-companion` | `[companion_memory].store_dir/companion-memory.db`; default `<data_dir>/companion/companion-memory.db` |

Both require the `PortableKernel` profile. The roles are not interchangeable.
The kernel supplies storage and typed contracts; ZeroClaw continues to own
memory admission, Soul and User Model authority, personality policy and agent
isolation. Hypermem supplies Hyperion's behavior independently. There is no
shared live database or new network memory service.

## Opening a store

A missing database can be created with its exact profile and owner role.
Existing stores must already have the current schema and matching identity.
Runtime and status checks refuse older, unowned, wrong-role or wrong-profile
stores instead of upgrading or adopting them, and say so in the error.

## Retired legacy stores

The one-time offline conversions are retired: there is no command that converts
a native SQLite memory store (`<data_dir>/memory/brain.db`, the removed `sqlite`
and `lucid` backends) or a standalone `memcore` schema 38 database. Selecting a
removed backend fails memory construction, and ZeroClaw neither reads nor
converts a leftover `brain.db`. A refused legacy or foreign kernel store is not
upgraded in place. Restore a store written by a supported release or start a
fresh one; do not change schema versions, role stamps or sidecars to bypass
admission.

## Retired OpenClaw importer

The `rei migrate openclaw` command is retired. ReiClaw no longer imports an
OpenClaw workspace through this CLI. Source workspaces and existing memory
stores remain on disk. The `rei memory` commands continue to manage supported
current stores, including listing, inspection, deletion and reindexing.
