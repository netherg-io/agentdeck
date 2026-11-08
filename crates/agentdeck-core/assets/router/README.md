# model-route

Picks a delegate for a task from the T3 favorites (`~/.t3/userdata/settings.json`; older T3 builds kept them in `client-settings.json`, used as a fallback).
Jev (OpenRouter decisions API, key in `~/.config/model-route/openrouter.key`) judges capability, difficulty
and task size. Local policy balances expected quality against quota use; quota from ai-usage
(`~/.local/state/ai-usage/state.json`) gates task sizes.
Both Codex logins appear in ai-usage as provider `codex`; instances `codex` and `codex_second` are matched by
account id (`codex-main`, `codex-second`), so each sees and reserves only its own quota. The Claude logins work the
same way: `claudeAgent`, `claude_second`, `claude_third` and `claude_fourth` match `claude-main`, `claude-second`, `claude-third` and `claude-fourth`.

- `agentdeck route "<goal, inputs, read-only or edits, size>"`: JSON with `target` for `delegate_task`, `ranking`, `blocked`, `unprofiled`.
- Before routing in a task, call T3 `orchestrator_capabilities` and pass its `structuredContent` JSON on stdin to `agentdeck route --catalog`. Refresh after provider/model settings change or a catalog mismatch. Only model IDs and option descriptors are saved in `~/.local/state/model-route/catalog.json`; T3 settings stay app-owned.
- Every target's `briefHint` combines the shared delegated-agent policy with any family hint, in that order separated by a blank line. Append the entire string verbatim; hooks require it. The shared policy text lives only in `roster.json` (`briefHint`): no nesting, scope, verification, report format and the `BLOCKED:` contract. Children hand pending PR checks/reviews/merges to the coordinator without polling or watching, ending with `PR <url>, head <sha>, checks and reviews <state>`. Local command waits remain allowed; pending PRs are a normal completed handoff, not `BLOCKED:`.
- `agentdeck route --source claudeAgent "<task>"`: explicitly identify the orchestrator; otherwise detected from executable names in the process chain. Other sources: `codex`, `codex_second`, `antigravity`, `opencode`, `claudeAgent_openrouter`. Codex shadow homes identify separate accounts. Unknown source is reported and cannot receive shared-quota protection.
- `agentdeck route --source codex --validate-target PROVIDER MODEL -`: read the complete task from stdin and validate an already selected target. Output includes `validation.allowed` and its reason. A denied target exits 0; malformed input or router failure exits nonzero.
- `agentdeck route --effort medium|high|xhigh`: replaces the estimated difficulty when choosing the reasoning level of the returned target. The model choice is unchanged.
- OpenCode worker agent: `agentdeck plugin enable router` writes `~/.config/opencode/agent/agentdeck-worker.md` (short invariants-first prompt, temperature 0.1, force-push denied; project rules stay in the project's own AGENTS.md). Once OpenCode loads it, T3 lists it as the `agent` option and `route` adds `options.agent` to OpenCode targets. Restart the OpenCode server and refresh the catalog after enabling.
- `agentdeck route --exclude INSTANCE`: ban every favorite on that instance's account for this call (repeatable).
- `agentdeck route --check`: quota, per-size cost and allowed sizes per favorite; favorites without a family
  profile are flagged `UNPROFILED` (report-only, exit 0: they route on their Jev judgment with
  conservative max-burn quota, no manual roster edit required).
- Tests: `cargo test -p agentdeck --test route_golden --test hook_golden --test shell_golden --test mcp_call_golden --test workflow_golden` plus unit tests in `crates/agentdeck-router`. Golden snapshots in `crates/agentdeck/tests/golden/` pin the router CLI contract (`UPDATE_GOLDEN=1` rewrites them). All offline, with isolated quota/log/SQLite fixtures and mocked Jev.

Concurrent routes share a queue in `$XDG_RUNTIME_DIR/model-route-jev-queue`; compatible pending requests go to the judge as one batched decision. HTTP 429 is waited out per Retry-After within 50 s (see `crates/agentdeck-router/src/jev_queue.rs`).

Selection policy:
- Jev difficulty selects explicit effort: 0/1/2/3 maps to low/medium/high/xhigh. The exact T3 model descriptor supplies the option ID and supported values: Codex `reasoningEffort`, Claude `effort` (older Haiku: boolean `thinking`), OpenCode `variant`. Missing tiers fall back to a supported lower ordinary tier, or the lowest ordinary tier when none is lower. Opaque variants use an advertised default; without one, routing fails instead of guessing. Special Claude `ultracode`/`ultrathink` modes are never selected automatically. Antigravity's Gemini favorites remain eligible without a separate effort option; their exact model IDs encode the reasoning level and are preserved.
- Pass the complete returned `target`, including `options`. Hooks require the model's explicit reasoning option and accept valid caller-supplied levels without rewriting them, preserving unrelated explicit options. T3 inherits omitted options only for the same provider instance/model; explicit options replace rather than merge with the parent list. Already-prepared workflow payloads stay unchanged; a pre-dispatch rejection must be released as not dispatched and prepared again before retry.
- Custom non-Claude models under T3's Claude harness are rejected when they advertise reasoning: the installed T3 compiler currently ignores their knobs. Use a harness that applies that model's option instead of silently accepting an ineffective setting.
- Suitability is judged independently per model on four capability levels, from unable to specialized.
  `fit` is the combined probability of adequate/specialized levels; `eligible` requires `fit >= 0.7` and enough
  quota. Models without a roster family are judged on the same scale against the honest
  `unprofiled; capability unknown` criterion: an adequate Jev score still makes them eligible, a low one
  gates them out. They are flagged `profiled: false` (and listed in `unprofiled`), carry no `burn`, and
  their quota uses the highest profiled burn when quota data exists. If the account has no quota data,
  the existing policy allows it and marks quota unknown; no cost is claimed as measured. The judgment
  is an unverified prediction based on the model ID and task, not evidence of actual performance.
  Raw values determine eligibility; output rounding cannot admit a candidate below 0.7. Jev scores,
  confidence and all four probabilities must be in range, with probabilities summing to 1 within 0.02.
  These are model judgments, not calibrated success probabilities. `quality` is the independent
  capability score normalized to 0..1; its top level requires a substantial practical advantage for this
  particular task, not merely a generally stronger model.
- Difficulty and size are independent. Mechanical edits across many files remain routine; a small concurrency
  bug can be hard. Eligible candidates rank by `score = quality - sharedQuotaPenalty - paidCreditPenalty + cheapModelBonus`.
  In `roster.json`, `routing.sharedQuotaPenalty` defaults to **0.05**: shared-account use costs 0.05 score points (one Jev level is 0.33).
  `routing.cheapModelBonus` is **0.15** × `max(0, 1 - burn)` (Haiku burn 0.1: +0.135, Sonnet 0.6: +0.06, unknown or
  burn >= 1: 0): a cheap model wins a small quality gap, never a whole Jev level (0.33).
  For hard tasks (difficulty 2..3), all penalties and the bonus are halved. Low remaining quota is never penalized
  (user rule 2026-10-17): quota only decides `allowed`, i.e. whether the task fits. One model on several accounts
  (e.g. `codex` and `codex_second`) gets the mean of its per-account `quality`, `fit` and `confidence`: the copies
  differ only by Jev noise. Exact score ties prefer the account with fewer active agents, then the cheaper task,
  so parallel work spreads across accounts (user request 2026-10-17). A substantial quality gain can win even on the orchestrator's own account.
  Claude Opus/Sonnet/Fable share the Claude account; OpenCode and Claude's OpenRouter instance share credits, except OpenCode `opencode-go/*` models, which use the ai-usage `opencode-go` subscription account.
  `routing.paidCreditPenalty` (OpenRouter credit account, halved for hard work) is **0** since 2026-10-18: the user
  asked to stop penalizing paid OpenRouter Muse because subscription windows run out too often. Raise it again to
  make paid credits need a quality advantage over subscription capacity.
- `taskCost` is the maximum estimated task cost / spendable quota across applicable windows or balances, used
  only for exact ties after active-agent load and for near-equal target validation.
  Dollar costs and non-Codex quota costs are estimates. Measured zero is floored at 0.1 percentage point,
  because coarse logs do not prove free work. Unknown quota is allowed and flagged.
  `allowed` alone only checks quota, not suitability.
- `quotaEstimate` (per ranking entry, additive): structured quota provenance —
  `basis` (`measured` from Codex session logs / `estimate` from plan table × burn /
  `unknown` when no quota data), `detail` source string, `sampleSessions` (measured only),
  `staleMinutes` (null when unreported), `loadUnknown`, applicable `windows`
  (percent units: key, label, post-reserve `remainingPercent`, `resetsInHours`) and
  `balancesUsd`. Estimates only: never used for ranking, gating or validation; missing
  data is null/empty, never zero-filled. The legacy `quota` note, `taskCost` and ranking
  are unchanged.
  Failed/stale refreshes retain the last known windows and balances as budget ceilings.
  Only an account with no retained quota is unknown; expired resets never imply replenishment.
- Weekly reserve (`routing.weeklyReservePercent`) is **0** (user rule 2026-10-17): the task only has to fit the
  remaining quota. A nonzero value keeps up to that percent after estimated task cost, shrinking to the window's
  remaining-time percent near reset; missing or expired reset data keeps the full reserve.
  All applicable windows must pass: Fable's separate window cannot bypass Claude's common weekly limit.
  Antigravity's Gemini and third-party pools are matched by bucket keys, independently of the current active model.
- Before ranking, live T3 work across all projects reserves one focused task (size 1) per working agent,
  using its model's cost and applicable pools. This includes main turns, delegated/native subagents and pending
  queued/preparing/starting/waiting runs, including native children with no separate provider thread;
  idle sessions and terminal work do not count. A queued next run is
  counted separately from the current turn, and a delegated child is never counted twice for its run/turn.
  Unknown models reserve the highest profiled burn against every applicable pool. Remaining work size is
  an estimate; this is not an atomic reservation for route choices that have not been dispatched yet.
  `the Rust `load` module` uses Node's built-in SQLite read-only API: the standalone hook has no MCP transport to list live
  agents. Only projection metadata is read, never conversation bodies; no app database writes or cleanup.
  Missing/incompatible metadata is explicitly flagged rather than claiming there are no active agents. The quota/load snapshots are refreshed after Jev and before ranking.
- Only exact active favorites are candidates. Removed OpenCode providers can leave hidden stale entries in
  settings: entries absent from the saved T3 catalog are excluded, never remapped to another provider. Removing a
  favorite must not silently reintroduce that model. Disabled provider instances are excluded; an explicit
  instance setting overrides the legacy provider setting. `opencode models` is not used: a cold OpenCode
  location returns an empty or unsettled list (Zen, no `opencode-go`, unfiltered OpenRouter).
  Settings remain app-owned.
- Capability profiles describe strengths and gaps; local quota policy handles cost. Routine source searches,
  summaries and bounded implementation do not require a frontier specialist to qualify as adequate.
- `reason`, `sourceAccount`, `quality`, `score`, `activeAgents`, `activeLoad` and per-candidate penalties explain the decision.
  `jevConfidence` is the lowest size, difficulty and selected-model confidence. Below 0.4, output includes a
  warning and the top two eligible candidates with their quality and penalties.
  Target validation requires an exact active favorite with quota and fit, either the winner or
  within 0.02 of its score with no higher task cost. A favorite absent from the saved T3 catalog fails
  closed (no reasoning target), as does a model outside the active pool. The tolerance never restores an expensive near-equal choice.
  Valid Jev answers are cached for ten minutes by source, full task and judgment context; a trailing known
  `briefHint` is stripped before judging, so `prepare` (bare brief) and validation (brief + hint) share one entry. Target validation
  reuses them and refreshes quota/load, so a retry cannot alternate models just because Jev was asked again.
  A changed quota/load snapshot can still change the winner. Delegate exactly the text you routed plus its hint:
  any other edit (a reworded line, a dropped hint) is a new Jev sample whose noise can exceed the 0.02 margin, so a
  hook denial followed by an edited retry can alternate between models. After a hook denial, resend the same task
  text with the named target. After a workflow hook denial, run
  `release ... --not-dispatched` successfully before preparing again; `prepare` on a prepared step reuses
  its saved payload without routing. Never substitute the hook's target directly into that payload.
  After a delegation error choose the next `eligible` candidate in weighted ranking.

## Routed workflow replacement

Use `$t3-routed-workflow` ([skill](../../skills/t3-routed-workflow/SKILL.md)) inside the current T3 thread.
`agentdeck route workflow` validates a declarative dependency plan, routes each complete child brief through `agentdeck route`,
and checkpoints prepared payloads, real task IDs and final results in private run files. The coordinator uses
the current thread's `delegate_task`, `task_status` and `task_cancel` tools; T3 owns execution and completion
notifications. Supported commands: `init`, `next`, `prepare`, `record`, `release`, `stop`, `resume`, all bound to the
parent thread ID. Parallel branches default to two active tasks; successful dependencies release the next
steps, and resume keeps completed results. Unknown launches require read-only task-ID recovery before any
redispatch; T3 deduplicates only within the same MCP credential, so a server restart or a new credential can
duplicate a replay with the same `clientRequestId`. `release` with `--not-dispatched` returns only an
unaccepted prepared step to pending for fresh routing and a new request ID;
use it only for proven non-dispatch. Failed steps do not retry side effects automatically. One coordinator
mutates each run sequentially.
A settled T3 `completed` result whose first non-whitespace text is `BLOCKED:` is recorded locally as
`blocked`; `next.blocked` includes that step and its dependent steps, while independent branches remain
available. T3's transport status stays `completed`. Existing prepared payloads missing a new required hint
must follow the confirmed non-dispatch release/prepare procedure; do not edit or replay uncertain launches.
T3's 120,000-character task and 512-character generated-title limits are checked before dispatch; oversized
dependency text stays pending and requires a revised plan, without silent truncation or reserving capacity.

This replaces local ultracode orchestration with T3 child tasks. It does not execute native workflow JavaScript,
reproduce native phase UI, or schedule work without a coordinator. Native `Workflow` remains guarded below.

### Shell dispatch from JSON

`agentdeck route t3-mcp-call delegate_task PREPARED.json > RESPONSE.json` sends the exact `.payload` from
`agentdeck route workflow prepare` (or a plain argument object) through the current session's T3 MCP connection.
Then feed the actual `RESPONSE.json` to `agentdeck route workflow record`; never construct task IDs yourself.
It also accepts JSON on stdin (`-`) and supports `orchestrator_capabilities`, `task_status` and `task_cancel`.

Connection discovery uses an inherited `T3_SHELL_MCP_SOCKET`, inherited ACP variables, or this process's
ancestor Claude `--mcp-config`. Modern T3 Claude injects MCP through SDK `mcp_set_servers` frames rather
than command-line config: its provider's `binaryPath` must point to `~/.local/bin/claude-t3-shell`
(symlink to the agentdeck binary). The launcher forwards the official Claude CLI's args and byte streams,
captures its own SDK MCP frames in memory, and passes a private Unix socket path to its own child.
The socket is `0600` inside a per-launch `0700` runtime directory, removed when the child exits;
MCP removal/rotation clears/replaces the in-memory connection. Non-streaming CLI commands pass through.
Changing the provider path affects new provider sessions. An existing Claude session needs an approved
provider-session replacement under the process-ownership policy: a normal turn interrupt closes only
the SDK query and can retain an adapter with the old binary path. T3's supported per-thread
`provider-session.detach` command (via its authenticated orchestration API) preserves the native
conversation and app-owned children; the next turn opens an adapter with the new path. T3 itself
need not restart. Do not use instance logout or switch accounts for this.

The shell client never searches other sessions, reads T3's credential store, or starts provider CLIs.
Credentials remain in memory. Native Codex keeps its scoped MCP config in app-server memory; use its
native MCP tools/code mode when no shell connection is inherited. Probe from the target shell with
`agentdeck route t3-mcp-call orchestrator_capabilities` before preparing new launches.

Before delegation the bridge reads live capabilities, refreshes the catalog and invokes the same
`agentdeck route hook` checks with the actual provider instance as source. It retains target options, task text and
request ID, requires `mode: async`, and rejects explicit `runtimeMode`. Shell decisions are logged with
`harness: shell`. No transport retry is automatic; after a lost response recover the real task ID from
parent activity before redispatching. Existing workflow launch-recovery rules still apply.

```sh
agentdeck route workflow prepare "$state" "$step" --thread "$parent" --source "$source" > prepared.json &&
agentdeck route t3-mcp-call delegate_task prepared.json > response.json &&
agentdeck route workflow record "$state" "$step" --thread "$parent" < response.json
```

Keep prepared payloads and responses private (`umask 077`) until checkpointing/recovery is complete.

`roster.json`:
- Top-level `briefHint`: required policy for every routed child, composed before any family `briefHint`.
- `families`: regex per model family, first match wins. Profiles describe durable family strengths, not versions.
- `minEffort` (optional, per family): lowest ordinary effort the router selects or the hook accepts for that family (Muse Spark: `high`, user rule 2026-10-10). Difficulty above the floor still raises effort; explicit lower requests are denied with a repaired target.
- `spendToZero` (optional, per family): ignores any weekly reserve and uses task cost 0 for tie-breaks; the task must still fit the remaining quota (Muse Spark, user rule 2026-10-14).
- `briefHint` (optional, per family): fixed text for gaps a model reliably has (Muse Spark needs the long-command/PARTIAL guidance). The coordinator chooses and names skills in each task brief; family hints do not ask children to select skills. `agentdeck route` composes it after the shared policy and returns the entire string as `briefHint` (winner) and `requestedBriefHint` (validated target); the hook denies a `delegate_task` whose task lacks it verbatim, and `agentdeck route workflow prepare` appends it.
  Built 2026-10-04 from multi-generation benchmark trends (Artificial Analysis, Arena, Epoch, OSWorld, SWE-bench,
  vendor cards) plus the user's experience. GPT is ranked best at computer use and debugging from the user's own
  tasks, although OSWorld puts Claude first. Many 2026 numbers were vendor-reported; re-check after major releases.
- `burn`: relative quota use per task within an account. `plans`: per ai-usage provider and plan label, weekly % per task size 0..3 for a burn-1 model.
  Codex costs use the account's actual plan, mapped to raw log `plan_type` by `codexPlanTypes`.
  Only 10080-minute weekly windows count, including Plus's secondary window. Logs are separated by plan and
  reset epoch (600-second jitter tolerance). Attributable consecutive same-session/model intervals estimate a
  percent-per-token rate; session token-volume percentiles give size costs. At least 10 sessions and 10
  attributable percentage points are required; resets and snapshot jumps over 2 points do not train the rate.
  Rate intervals also require two minutes without another session's activity before the first snapshot.
  Versioned cost caches last 6 hours. Rates estimate cost and ignore cache-hit mix; two accounts on the same plan
  with nearly identical reset epochs cannot be distinguished by these logs. Insufficient data uses plan estimates.

Volatile areas deliberately left out of profiles: long-context leader, Arena text rank, instruction following,
hallucination rate, Terminal-Bench. OpenAI tier suffixes (sol, astra, luna, terra) change meaning between
generations, so price comes from quota data, never from the name.

`agentdeck route hook --harness <claude|codex|agy|opencode|shell>`: guards the delegation entry points below inside T3. The AGENTS.md rule
alone was ignored (agents used native subagents and never ran the router), so each harness calls this hook:

| Harness | Event | Registered in | Redirect mechanism |
|---|---|---|---|
| Claude (both instances) | PreToolUse `Agent` / T3 `delegate_task` | `~/.claude*/settings.json` | native subagent calls are denied; direct MCP `delegate_task` calls validate the requested target |
| Claude ultracode | PreToolUse `Workflow` | both Claude settings above | deny local starts/resumes inside T3; route each child task through T3 delegation |
| Codex | PreToolUse `spawn_agent` / `collaborationspawn_agent` / `Agent` / T3 `delegate_task` | `~/.codex/hooks.json`, trust hash in `~/.codex/config.toml` `[hooks.state]` | native calls redirect to an exact T3 target; MCP calls validate the requested target |
| agy (T3 ACP) | PreToolUse `start_subagent` / `invoke_subagent` / T3 `delegate_task` | `$GEMINI_HOME/config/hooks.json` (symlink into `~/.gemini/config/plugins/model-route/`) | native subagent calls are denied; direct MCP `delegate_task` calls validate the requested target |
| OpenCode | `execute.before` on `subagent` / T3 `delegate_task` | `~/.config/opencode/plugin/model-route.ts` | throws `Tool.Error` on a denied or failed hook |
| Shell bridge | explicit pre-dispatch check | `agentdeck route t3-mcp-call` | same MCP target validation; source from live scoped capabilities |

- Only processes T3 launched are routed (Claude: `t3-code` in `--mcp-config` or stream-json under T3's server;
  Codex/agy: app-server/ACP under T3's server, the native `t3 serve` (older T3 used the Node `apps/server/dist/bin.mjs`);
  OpenCode: server from T3's AppImage). Outside T3 native calls pass through. `delegate_task` exists only in T3, so it
  is always routed: a missed T3 detection must not silently disable routing. Router errors/timeouts deny and
  are logged. Native subagent calls have no escape hatch: they are always routed and handed back to
  `delegate_task`; MCP calls always validate. User choice: when the user explicitly asks for a model, a
  `delegate_task` whose task starts with `[route:user-choice]` may use any model in the live catalog, even
  one that is not a favorite, over quota or below the fit threshold. The hook allows it with a warning
  (validation reason and the router's own pick) and logs `override:user-choice`. Where the harness exposes
  its transcript (Claude `transcript_path`, agy `transcriptPath`), one of the last six genuine user messages
  must name the model (a distinctive model-id word such as `haiku`, `argon`, `sol`, or a Cyrillic spelling);
  answers the user picked in an AskUserQuestion dialog count (only the chosen option, not the question);
  other tool results, meta entries, notifications and the agent's own words do not count. Without a readable
  transcript the prefix is trusted. Reasoning options and family brief hints are still required.
  A roster family with `noReview: true` (Gemini Argon) is never selected for, and agents are never
  allowed to send it, an audit or review of any kind. A brief counts as one when it mentions audit/review
  (also Cyrillic аудит/ревью/рев'ю) or the `delegate_task` role is `review`. Only an explicit user choice
  lifts this, and the warning then names the rule. Low confidence is visible on
  native redirects and MCP decisions too. Complete instructions are judged via stdin, never reconstructed
  from logs; metadata is used only when instructions are absent. Route and validation on identical instructions
  reuse the same cached judgment. The CLI brief must match the delegated instructions to get a cache hit.
- Delegated children cannot delegate further: before routing, the hook maps the caller's native session id
  (Claude/Codex `session_id`, agy `conversationId`, OpenCode `sessionID` passed by the plugin; the shell bridge
  passes the T3 `parentThreadId`) to a T3 thread and denies every entry point when that thread is an app-owned
  subagent's child (`the Rust `load` module` `isDelegatedChild`, logged `deny:nested`). Earlier, a routing denial told such
  children to use `delegate_task`; lacking it, they silently did the work themselves. Unreadable metadata allows
  routing to continue; the shared `briefHint` forbids nesting as well.
- Native calls redirect to T3 delegation because aliases, inherited models and role configs cannot pin an exact
  model (on Codex a role model overrode a hook rewrite). Codex `SubagentStart` is advisory and unused. Codex's
  native tool name is `collaborationspawn_agent`; plain allows return context only, and a bare
  `permissionDecision: allow` without `updatedInput` fails.
- Ultracode `Workflow` runs script `agent()` calls without the `Agent` hook and fixes the model before its
  `agent.spawn` event, so the local entry point (including named workflows, `scriptPath`, `resumeFromRunId`) is
  denied inside T3. `TaskOutput`, `TaskStop` and `SendMessage` stay available; remote/cloud launchers are out of scope.
- Codex trust: after editing `~/.codex/hooks.json`, read `currentHash` via app-server `hooks/list` and write it as
  `trusted_hash` (no app-server trust method exists). A shadow `CODEX_HOME` uses a different hook key even
  when `hooks.json` and `config.toml` are symlinks; trust both the primary and shadow source-path keys.
- agy's T3 config dir is per provider hash (`~/.t3/userdata/providers/antigravity/<hash>/config`); re-link
  `hooks.json` if T3 creates a new one.
- Fresh Codex app-server `hooks/list` verifies disk registration and trust, not activation in an already loaded
  thread. Loaded threads retain old hook trust; `thread/resume` and reloading another app-server do not help.
  `config/batchWrite` with `reloadUserConfig: true` must reach the owning app-server, or it must be restarted
  with the user's approval. T3 currently exposes no direct reload request for its stdio-owned Codex server.
  Verify live dispatch separately. agy loads hooks per session.
  Decisions include candidate ranking and quota gates: `~/.local/state/model-route/decisions.jsonl`; touch `~/.local/state/model-route/debug`
  to also log raw hook input to `hook-input.jsonl` (contains task text).

## Instance capacity

Agentdeck's `[[instance]].max_parallel` (positive integer, absent = unlimited)
becomes `instanceLimits` in exported router JSON. The router reads the live
registry, counts all active T3 threads and subagents by exact provider instance
across coordinators and models, and excludes full instances. Queued and waiting
runs also occupy slots; completed/idle threads do not. Shared quota accounts do
not combine instance capacities. Unknown T3 load blocks limited instances.

The launch hook repeats the capacity check inside a shared SQLite transaction
and reserves the slot until T3 projects the launch. A failed dispatch reservation
expires after 120 seconds. The user-choice model override does not bypass this
capacity gate. Workflow plans enforce dependencies, with no coordinator-wide cap;
old `concurrency` keys are accepted and ignored.
