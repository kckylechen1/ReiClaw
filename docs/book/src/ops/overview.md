# Operations: Overview

How to run ZeroClaw in production. The surface is intentionally small: one
binary, one config file, and one install root with a handful of runtime stores.
Most "operations" is "systemd and journald".

This section covers:

- [Service & daemon](./service.md): keeping the process alive
- [Logs & observability](./observability.md): reading what the agent did
- [Cost tracking](./cost-tracking.md): token spend and per-model cost
- [Troubleshooting](./troubleshooting.md): when things break
- [Network deployment](./network-deployment.md): exposing the gateway, tunnels, reverse proxies

## The shape of a deployment

A typical always-on ZeroClaw install is:

```text
zeroclaw service                          — systemd / launchctl
└── zeroclaw daemon                       — the single long-running process
    ├── gateway listener  :42617          — REST / WebSocket / webhook intake
    ├── channel pollers                   — Telegram long-poll (outbound poll)
    ├── channel listeners                 — WebSocket (inbound stream)
    ├── cron scheduler                    — scheduled SOPs and jobs
    └── agent loop  (one per session)     — provider call + tool execution
                                            ▲ driven by any listener, poller,
                                              gateway request, or cron fire

on disk (everything but the binary can move)
├── ~/.zeroclaw/config.toml               — configuration
├── ~/.zeroclaw/.secret_key               — master key for the encrypted secrets store
└── ~/.zeroclaw/data/                     — runtime state
    ├── memory/                           — agent memory backend
    ├── sessions/                         — per-session conversation stores
    └── state/                            — scheduler, cost, health, misc runtime state

logs                                      — journald / launchctl (platform-native)
```

Everything except the binary can move. The data dir defaults to
`~/.zeroclaw/data/` (the legacy `~/.zeroclaw/workspace/` name is still
accepted); config paths resolve per environment (Homebrew vs. bootstrap vs.
XDG), and log destinations are platform-native by default. For the full store
map, see [Runtime state and persistence](../architecture/runtime-state-and-persistence.md).

## What to monitor

Four signals matter:

### 1. Service liveness

Is the process running?

<div class="os-tabs-src">

#### Linux

```sh
systemctl --user is-active zeroclaw
```

#### macOS

```sh
launchctl list | grep -c com.zeroclaw.daemon
```

</div>

If it's dying repeatedly, check [Troubleshooting → Daemon keeps restarting](./troubleshooting.md).

### 2. Channel and component health

The gateway exposes a component health snapshot at `/health` (public, no secrets) and `/api/health` (authenticated). Channels, providers, and other long-running components register themselves in the `components` map as they start, report OK, or error.

<div class="os-tabs-src">

#### sh

```sh
curl -s http://localhost:42617/health | jq
```

</div>

```json
{
  "status": "ok",
  "paired": true,
  "require_pairing": true,
  "runtime": {
    "pid": 4821,
    "updated_at": "2026-06-08T09:00:00+00:00",
    "uptime_seconds": 3600,
    "components": {
      "channel:telegram": {"status": "ok", "updated_at": "…", "last_ok": "…", "last_error": null, "restart_count": 0},
      "channel:matrix":   {"status": "error", "updated_at": "…", "last_ok": "…", "last_error": "401 Unauthorized", "restart_count": 3}
    }
  }
}
```

Each component carries `status` (`starting` / `ok` / `error`), `last_ok`, `last_error`, and `restart_count`. Watch for `status: "error"` and climbing `restart_count`.

### 3. Provider reliability

Providers surface as components in the same `/health` snapshot. Use the structured
log stream for request latency, failures, and token usage. See
[Logs & observability](./observability.md) for event fields and filtering.

### 4. Tool-call events

Use authenticated `/api/logs` queries and the `/api/events` stream to inspect tool
calls and failures. The retained `log` and `verbose` observers also report runtime
activity. The in-process Prometheus observer and `/metrics` endpoint have been
retired under ADR-026; `observability.backend = "prometheus"` loads as `none` with
a retirement warning. Log persistence settings remain effective independently
of that legacy backend selection.

## Capacity

A single ZeroClaw instance can handle:

- Multiple concurrent conversations across all channels
- Tool calls at whatever rate the provider and sandbox allow
- Long-running agent loops (tool chains of 20+ calls)

Scale laterally by running one instance per workspace. Don't try to run two daemons on the same workspace: SQLite's single-writer model will produce lock contention and ultimately corruption.

For multi-tenant hosting, see the proposal in #2765 (closed, historical, the architecture for in-process multi-workspace routing).

## Backups

What to back up:

- `~/.zeroclaw/data/memory/*.db`: conversation memory (`memorycore.db`, plus `audit.db`)
- `~/.zeroclaw/data/sessions/`: persisted session state
- `~/.zeroclaw/.secret_key`: master key for the encrypted secrets store (if used). **Without it, the config's encrypted secrets are unrecoverable.**

A plain `tar czf zeroclaw-$(date +%F).tar.gz ~/.zeroclaw` covers everything. Restic, borg, or Duplicacy work fine for incremental backups.

`~/.zeroclaw/data/memory/response_cache.db` is a regenerable LLM response cache; it's safe to include in a full-directory backup or to exclude to save space. Tool receipts are in-band HMAC tokens in the conversation history (see [Tool receipts](../security/tool-receipts.md)), not an on-disk log, so there is nothing separate to back up for them.

## Updates

The service does not auto-update. Watch the GitHub release feed. Typical update cadence:

1. Read the release notes
2. Back up `~/.zeroclaw/`
3. Update the binary (`brew upgrade`, bootstrap re-run, or `cargo install --force`)
4. `zeroclaw service restart`
5. Verify the `/health` endpoint reports `status: "ok"` with no component in `error`

This fork accepts only config files with explicit `schema_version = 3`. Older, future and unversioned files are rejected without conversion or rewriting. Run `zeroclaw config validate` to check the current file and `zeroclaw config list` to inspect effective values. `zeroclaw config generate` emits a native current-schema example; it does not accept a version argument.

## See also

- [Setup → Service management](../setup/service.md): install/remove/logs per platform
- [Logs & observability](./observability.md)
- [Troubleshooting](./troubleshooting.md)
- [Network deployment](./network-deployment.md)
