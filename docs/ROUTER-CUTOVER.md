# Router cutover: standalone `model-route` to the plugin

Replace (one owner session, temp dirs only for checks):

1. `agentdeck plugin enable router` (lays out `router.json`,
   hooks, the `claude-t3-shell` symlink and the marker block).
2. `agentdeck route --check` must exit 0 with no node on PATH.
3. Point the T3 Claude provider `binaryPath` at the new
   `~/.local/bin/claude-t3-shell`; detach the provider session so
   the next turn adopts it, then probe `t3-mcp-call`.
4. In `~/.general/skills/t3-routed-workflow/SKILL.md` replace the
   `node`+`.mjs` helper path with the binary, same args: line 13
   `route.mjs --catalog` to `agentdeck route --catalog`; line 31
   helper path to `agentdeck route workflow`; line 34 `init`,
   line 60 `release`, and the `next`/`prepare`/`record`/`stop`/
   `resume` forms, to `agentdeck route workflow ...`; line 45
   `t3-mcp-call ...` to `agentdeck route t3-mcp-call ...`.

Roll back: `agentdeck plugin disable router` (keeps `router.json`),
restore the old provider `binaryPath`, detach the session again.
The standalone `~/.general/scripts/model-route` tree is untouched.

Status: the cutover is complete. The legacy Node router sources are
deleted; `agentdeck route` (route, hook, workflow, shell bridge,
`t3-mcp-call`) is the only router. No Node is needed at runtime or
in tests.
