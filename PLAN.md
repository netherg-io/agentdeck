# agentdeck — plan

Public, Linux-only Rust app (iced GUI + CLI in one binary) that manages harness instances/accounts for Claude Code,
Codex, OpenCode and Antigravity (agy). Subscription quotas and the model router require T3 Code;
instance management and API-key balances work independently. License: GPLv3 (ai-usage moves in with its git history).

## Decisions

- UI: iced, EN + RU via fluent. Distribution: GitHub releases and `cargo install` (no Flatpak: sandbox blocks config dirs).
- Registry `~/.config/agentdeck/instances.toml` is the source of truth; sync with a drift view (apply / import).
  GUI, CLI and daemon share it under `flock`.
- Login only through official CLIs (`claude auth login`, `codex login`, `opencode auth login`) with the right HOME,
  run inside agentdeck in a PTY (opens the login URL, accepts a pasted code); no external terminal needed.
- One shared profile per harness; accounts differ only by credentials, history and cache. Managed keys are generated
  and merged (no symlinks except skill dirs); harness-side edits come back through the drift view.
- Instance IDs are never renamed (T3 thread history is bound to them). Removal keeps the login dir by default and warns
  about T3 threads using the instance.
- Changes affect new sessions only; agentdeck never kills or restarts processes.
- Secrets: 0600 files; redacted in logs, diffs and the GUI.
- Plugins are bundled into agentdeck and have no standalone existence (owner, 2026-10-24): ai-usage, the router and
  agent-process live only in this repo/binary; their old standalone copies (`~/.local/bin/ai-usage*`,
  `~/.general/scripts/model-route`, `~/.general/scripts/agent-process`) are retired during migration (deletion and
  stopping old services need the owner's OK at that moment). Enable lays out exactly the plugin's files, hooks and
  instruction block; disable removes them, so the feature is gone; plugin data is kept. No downloads, no `.so`.
- Router plugin: only with T3. Instructions are marker blocks `<!-- model-route:begin -->…<!-- model-route:end -->`
  in global instruction files (symlinked files written once). Phase 1: the Node router is copied into this repo and
  extracted from the binary on enable (to agentdeck's data dir); later it is
  ported to Rust (`agentdeck route`).
- Model pool lives in agentdeck (instance ID + exact model + options + enabled). Its UI shows all cached models;
  new entries start disabled and T3 favorites have no effect. The Rust router derives its pool and account/usage
  maps from the current registry; generated JSON remains an explicit compatibility export.
- T3 Code is required for subscription quotas (owner, 2026-11-13): Claude, Codex, Antigravity and OpenCode Go
  read T3's caches; the old provider polling code is deleted. T3's Settings → Providers → Health check interval
  (`providerHealthRefreshInterval`) must be nonzero. Agentdeck still polls API-key balances (OpenRouter, Vercel,
  DeepSeek). `[[usage]]` binds subscription sources to instances; OpenCode Go defaults to `opencode`.
- Claude and Antigravity quota status lines are removed permanently (owner, 2026-11-13). No quota capture hooks.
  After upgrading, `agentdeck plugin enable ai-usage` removes old hook registrations.
- ai-usage plugin: subscription cache reads, balance polling and the daemon unit. Only the COSMIC applet stays
  a separate binary.
- Optional systemd user service `agentdeck daemon`: T3 cache reads, balance polling + background sync. Additive changes auto-apply;
  overwrites/deletions wait for a GUI decision via a desktop notification. GUI works without it.
- T3 itself is not managed (owner, 2026-10-24): no start/stop/service control of T3; agentdeck only edits T3
  `providerInstances` and reads its caches/db.
- agent-process plugin (owner, 2026-10-24): the bash `~/.general/scripts/agent-process` is ported into the binary
  with an identical CLI (`start/list/status/stop`, exit codes, `--force`/`--grace-seconds`/`--no-escalate`), identical
  registry (`$XDG_RUNTIME_DIR/ai-agent-processes/<id>.json`, same keys) and env tags (`AI_AGENT_PROCESS_ID/OWNER`).
  Enable = `~/.local/bin/agent-process` -> agentdeck (argv[0] dispatch) plus its instruction marker block ("start
  servers only via agent-process ..."); disable removes both. `agent-process.test.sh` moves into the repo and must pass
  against the port. The general process-ownership policy (never stop user-owned processes) stays hand-written and is
  never edited by agentdeck; only the agent-process usage lines move into the plugin's block.
- Processes page (owner, 2026-10-24): live tree of agent-related processes grouped by owning session (agent-process
  registry owner, T3 `t3code-*.scope` units mapped to T3 threads, harness processes and descendants by ancestry/env),
  with CPU/RSS/IO from /proc and per-scope cgroup stats. Stop = SIGTERM to the process group, grace, then optional
  SIGKILL, identity checked by pid + start ticks, after a confirmation that lists what will stop. The owner's click in
  the GUI is the approval; the CLI keeps agent-process ownership rules, so agents get no new bypass. T3 sessions are
  interrupted through T3 when it is reachable, not killed.
- Codex shadow-home drift (incident 2026-10-23, thread f1b31172): Codex added a new top-level entry
  `~/.codex/.hook-trust`; a running Codex 2 created its own real `.hook-trust` dir in `~/.codex-t3/second`, and on the
  next start T3 refused ("already exists and is not a symlink") and disabled the instance. agentdeck therefore:
  - checks every shadow home: each entry except the private ones (`auth.json`, `models_cache.json`) must be a symlink
    into the main home, and every main-home entry must have its link;
  - shows violations in the drift view (daemon: notification) with a fix: merge the content into the main home when it
    is mergeable (`.hook-trust` records are keyed per home), back up the shadow entry, replace it with the symlink;
    unknown non-mergeable content is backed up and needs a decision, never silently dropped;
  - shows instance health from `~/.t3/caches/<id>.json`; T3 caches the failed status, so after a fix agentdeck offers
    to re-enable the instance by toggling `enabled` false→true in `providerInstances` (T3 rebuilds it; no restart).
- Shared management across harnesses: instructions (AGENTS.md), skills (replaces `sync-skills`, real copies for agy), MCP.
- Harness CLIs: detect and update via their own updaters; fresh install via official installers.
- Tests use fixtures in temp dirs only; every write command has `--dry-run`.

## Milestones

| # | Scope |
|---|---|
| M0 | Research (read-only): Claude, Codex, OpenCode/agy multi-account mechanics; T3 settings and catalog; ai-usage move; router inventory; Rust stack |
| M1 | `core` + CLI: registry with flock, Claude/Codex adapters, T3 integration, wrappers (`claude@2`, `.desktop`), pool schema + `pool` commands, shadow-home check/fix, agent-process port, dry-run, backups |
| M2 | Move ai-usage: git history, usage module on the registry, `agentdeck daemon`, COSMIC applet, migration of the current install (stopping the old service needs owner approval) |
| M3 | iced GUI: instances + quotas, add wizard with login, drift view, plugins page, processes page, EN/RU |
| M4 | Background sync in the daemon: inotify + debounce on managed keys, safe auto-apply, notifications |
| M5 | Shared instructions, skills, MCP |
| M6 | Router plugin (Node): pool page, router JSON, router switched from favorites to pool, favorites import |
| M7 | OpenCode and agy adapters; harness update buttons |
| M8 | Router port to Rust: `agentdeck route`, hooks call the binary |
