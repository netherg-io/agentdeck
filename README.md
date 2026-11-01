# agentdeck

Linux-only manager for harness instances and accounts: Claude Code, Codex,
OpenCode and Antigravity (`agy`). One Rust binary holds the CLI, the iced GUI
and the usage polling daemon; the COSMIC panel applet is a separate binary.

What it does:

- Keeps a registry (`~/.config/agentdeck/instances.toml`) of harness
  instances, the model pool and quota sources; the CLI, GUI and daemon share
  it under a file lock.
- Polls usage quotas (5-hour, weekly and per-model windows, balances) in the
  background and publishes one `state.json` for the panel, the GUI and the
  router. Login happens only through the official CLIs inside agentdeck's PTY.
- Syncs shared state across instances: secondary accounts, Codex shadow
  homes, skills, instruction files, MCP lists, terminal wrappers.
- Ships built-in plugins with no standalone copies: usage polling, the model
  router and the agent-process supervisor.

## Install

```sh
cargo install --locked --path crates/agentdeck
```

Desktop entry (points at `agentdeck` on `PATH`):

```sh
install -m 0644 crates/agentdeck/agentdeck.desktop ~/.local/share/applications/
```

COSMIC applet (separate binary, libcosmic required):

```sh
cargo install --locked --path crates/agentdeck-applet
```

Then add **agentdeck** in the COSMIC panel settings. The applet reads the same
`state.json` the daemon publishes; no network calls, no credentials.

## CLI overview

```text
agentdeck instance      registry instances in instances.toml (list/add/rename/remove)
agentdeck pool          model catalog and explicit routing selections
agentdeck usage         [[usage]] quota sources (once/doctor/hooks plan)
agentdeck daemon        usage polling daemon (single writer)
agentdeck login <id>    official login CLI for an instance inside a PTY
agentdeck claude        sync and drift-check secondary Claude accounts
agentdeck codex         sync, check and fix Codex shadow homes
agentdeck opencode      show OpenCode accounts, switch the active one
agentdeck harness       detect binaries, run official updaters/installers
agentdeck t3            T3 providerInstances status, sync and import
agentdeck sync          background sync drift status and manual runs
agentdeck skills | instructions | mcp   shared skills, AGENTS.md links, MCP lists
agentdeck wrapper       terminal wrappers and legacy launcher cleanup
agentdeck service       install/remove/inspect the systemd user unit (never starts it)
agentdeck plugin        enable/disable built-in plugins (agent-process, ai-usage, router)
agentdeck route         route a task through the router (route.mjs port)
agentdeck router        refresh the generated router.json
agentdeck ps            live agent-process tree (read-only)
agentdeck migrate       copy-only migration from the old ai-usage tree
```

Every write command has `--dry-run`; tests and experiments run on temp dirs.

The daemon is a systemd user unit, managed but never auto-started by
agentdeck itself:

```sh
agentdeck service install   # renders the unit, daemon-reload only
agentdeck service status
systemctl --user enable --now agentdeck-daemon.service   # owner's call
```

## GUI

The same binary serves the iced GUI (English and Russian): instances with
quotas, the add wizard with login and the drift view (with the sync auto-apply
switch). Each module is a sidebar group with its on/off switch, its pages and
its technical status: Quotas (sources, panel widget visibility), Router (pool,
parallel limits) and Agents (processes). The usage daemon re-reads quota
sources every cycle, so GUI edits apply without a restart. The GUI shares the registry with the CLI under `flock` and
works while the daemon is stopped. The Pool page lists every cached model per
registered instance, with search, an instance filter and sorting. New catalog
entries start disabled; only explicitly enabled entries are routing candidates.
T3 favorites are not used. Instance names can be edited without changing their
IDs or T3 thread history. The router reads the current registry directly.

**Parallel tasks** on each instance sets its router limit; a blank value means
unlimited. The limit covers active T3 threads and subagents across every
coordinator and model on that instance, including queued and waiting runs.
Limits are stored as `max_parallel` in `[[instance]]`. Routing excludes full
instances, and the launch hook reserves a slot before dispatch. Failed dispatch
reservations expire after two minutes. If T3 load cannot be read, launches on
limited instances are blocked. Workflows only enforce dependencies; legacy
`concurrency` values are accepted and ignored.

Each instance has a **CLI** button that opens its CLI in a terminal with the
correct account. From a terminal, use `agentdeck instance run <id> -- <args>`.
Instance wrappers no longer add application menu entries. To remove legacy
Agentdeck provider shortcuts, use `agentdeck wrapper remove-launchers` (supports
`--dry-run`); the wrapper commands themselves are kept.

## Plugins

Built-in, enabled per machine with `agentdeck plugin enable <name>`:

- `ai-usage` — quota polling daemon unit, hooks and status-line integration.
- `router` — model pool JSON and the delegation router.
- `agent-process` — supervised background processes with the same CLI and
  registry as the standalone script it replaces.

Enable lays out exactly the plugin's files, hooks and instruction block;
disable removes them. Plugin data is kept.

## Migration from ai-usage

Step-by-step runbook: [`docs/MIGRATION.md`](docs/MIGRATION.md).

```sh
agentdeck migrate ai-usage --dry-run
agentdeck migrate ai-usage   # copy-only: ids, secrets, cooldowns
```

## License

GPL-3.0-or-later.
