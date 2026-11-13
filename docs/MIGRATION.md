# Migrating ai-usage to agentdeck

Owner-run cutover. Service changes and retirement commands below are for the
owner to run; agents do not run them. Migration copies files and keeps the old
install for rollback.

## Upgrade to T3 subscription quotas

After installing a build with this change, run `agentdeck plugin enable ai-usage`
to remove the old quota hook registrations. Claude and Antigravity quota status
lines have been removed permanently; no quota capture hooks replace them.

Subscription quotas (`claude`, `codex`, `antigravity`, `opencode-go`) require T3
Code with their provider instances enabled. In T3 **Settings → Providers → Health
check interval**, select a nonzero interval (`providerHealthRefreshInterval` in
milliseconds). At `0`, only manual refresh updates the cache and quotas go stale.
OpenRouter, Vercel AI Gateway and DeepSeek balances are still polled by agentdeck.

## 0. Install the binary and fill the registry

```sh
cargo install --locked --path crates/agentdeck
agentdeck t3 import --dry-run
agentdeck t3 import
```

## 1. Copy the old config

```sh
agentdeck migrate ai-usage --dry-run   # inspect the id → instance/usage table
agentdeck migrate ai-usage             # copy-only: ids, secrets, history
agentdeck usage doctor                 # sources, T3 refresh setting, state freshness
```

Claude and Codex sources are matched to registry instances by login directory;
Antigravity is matched only when there is one registered agy instance. Unmatched
subscription sources are reported and skipped; add them with `agentdeck usage add`
and an explicit `--instance`. OpenCode Go defaults to T3's `opencode` instance.
Legacy source fields in existing configs still load but are no longer required.
Old provider snapshots and cooldown files are not copied.

## 2. Install the new unit and remove obsolete quota hooks

```sh
agentdeck plugin enable ai-usage --dry-run
agentdeck plugin enable ai-usage       # installs the unit and removes old hook registrations
agentdeck service status               # installing the unit does not start it
```

## 3. OWNER cutover

```sh
systemctl --user disable --now ai-usage.service
agentdeck usage once --compat-write-ai-usage
agentdeck usage doctor
systemctl --user enable --now agentdeck-daemon.service
```

The new daemon keeps writing the old state paths for compatibility with old
applet builds. `usage doctor` should report fresh state and no disabled T3 refresh.

## 4. OWNER rollback (before retirement)

```sh
systemctl --user disable --now agentdeck-daemon.service
systemctl --user enable --now ai-usage.service
```

The old config and state tree were copied, never moved. If the old collector
requires quota hooks, restore its registrations from the cleanup backups.

## 5. OWNER retirement (after at least one week)

```sh
rm ~/.local/bin/ai-usage*
# Remove the old GNOME extension, then log in again.
rm ~/.config/ai-usage/config.toml ~/.config/ai-usage/secrets.env
```

Keep `~/.local/state/ai-usage/` while old applet builds need its `state.json`;
the new daemon continues writing it. Old generated quota hook scripts are no
longer needed.
