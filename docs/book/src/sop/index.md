# Standard Operating Procedures (SOP)

The legacy SOP engine, definition loader, authoring CLI/RPC and `procedure_v1`
path have been retired. The pages below document the historical format and
runtime; they do not describe an available execution surface.

Explicit `[sop]` configuration, including an empty table, is rejected. The
lowercase `ZEROCLAW_sop` and `ZEROCLAW_sop__*` overrides are also rejected.
Review the old approval groups, policies and input guards before removing them;
they are not automatically converted to current policy or Tachi permissions.
Loading, migrating or saving a file containing SOP configuration stops without
rewriting that file. New generated configurations omit the retired block.

A historical `<data_dir>/sop/runs.db` is left in place and reported at daemon
startup. ZeroClaw does not read, migrate or delete it. Review and archive the
file, or remove it manually when it is no longer needed.

- [How SOPs run](./how-it-works.md): the pre-removal runtime contract, event flow, and getting-started walkthrough, kept as a historical reference.
- [Syntax](./syntax.md): the historical file layout and trigger/step syntax.
- [Cookbook](./cookbook.md): historical SOP patterns.
- [SOP Fan-In](./fan-in/overview.md): event fan-in formats for SOP triggers. MQTT, filesystem, and AMQP were wired live sources and the daemon's periodic maintenance tick dispatched cron triggers before the run side was removed; webhook, peripheral, and calendar triggers were defined and matched but never routed to a live event source.
- [Observability](./observability.md): where run state and audit entries were stored before the run-side removal (historical reference).
- [Worked Example](./example.md): the stagehand StageX auto-update bot walkthrough, from build to draft PR, driven by a deterministic SOP over an AMQP feed (pre-removal; kept as a worked example of the format).
