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
  router, the agent-process supervisor and gateway image generation.

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
agentdeck plugin        enable/disable built-in plugins (agent-process, ai-usage, router, imagegen)
agentdeck imagegen      generate or edit one image through Vercel AI Gateway or OpenRouter
agentdeck route         route a task through the router (`agentdeck route`, hook, workflow, shell bridge)
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
switch). Each module is a sidebar group with its on/off switch and its pages:
Quotas (sources with API keys, panel widget visibility, polling interval),
Router (pool, parallel limits, decision provider, model and key) and Agents
(processes, including podman containers started by agents). System → Modules shows each module's technical status. The usage
daemon re-reads quota sources, polling settings and new keys from `usage.env`
every cycle, so GUI edits apply without a restart. The GUI shares the registry with the CLI under `flock` and
works while the daemon is stopped. The Pool page lists every cached model per
registered instance, with search, an instance filter and sorting; models T3
marks legacy are hidden unless enabled or shown with the Legacy checkbox. New catalog
entries start disabled; only explicitly enabled entries are routing candidates.
T3 favorites are not used. Instance names can be edited without changing their
IDs or T3 thread history. The router reads the current registry directly.
Saved pool entries the catalog does not provide (custom models or options) can
be deleted; catalog models are switched off instead.

Module settings live in `instances.toml`; absent keys use the defaults:

```toml
[router]
decision_provider = "openrouter"          # or "vercel" (Vercel AI Gateway)
decision_model = "~typesafe/jev-latest"   # default for OpenRouter; required for vercel

[imagegen]
provider = "vercel"                       # or "openrouter"
model = "bytedance/seedream-5.0-pro"      # default; OpenRouter default: google/gemini-3.1-flash-image
transparent_model = "openai/gpt-image-1-mini"  # --transparent, always on Vercel

[usage_poll]
refresh_seconds = 120   # minimum 15
stale_seconds = 900     # cached quota data older than this is stale
```

Decision keys: OpenRouter reads `JEV_OPENROUTER_API_KEY` or
`~/.config/model-route/openrouter.key`; Vercel AI Gateway reads
`AI_GATEWAY_API_KEY` or `~/.config/model-route/vercel.key`. Quota sources of
kind `vercel` show the gateway's credit balance. Image keys: `AI_GATEWAY_API_KEY` /
`OPENROUTER_API_KEY`, then the `api_key_env` of the matching `[[usage]]` source,
then the same `model-route` key files.

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
- `imagegen` — `agentdeck imagegen` plus the `imagegen-gateway` skill, synced to
  every harness. The skill tells agents to use it only when no built-in image
  tool or other image skill works, because every call is billed. Image-only
  gateway models go through the Images API (`/images/generations`,
  `/images/edits`); Gemini/GPT image models go through chat completions.

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
