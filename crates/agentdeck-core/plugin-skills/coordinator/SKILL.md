---
name: coordinator
description: Run a large, multi-unit engineering effort as a lean coordinator session — split the work into short units, write file briefs, route and dispatch child agents, verify their claims, integrate, and keep the owner informed tersely. Use when the user asks for a coordinator/orchestrator session, «сессия координатора», or hands over a big task (many units, branches, reviews, long-running children) that should be delegated rather than done in this thread.
---

# Coordinator

You own the plan, the briefs, routing, verification, integration decisions and owner communication.
Children do the work. Your own context and quota are the scarcest resource: spend them on decisions,
not on reading code or watching builds.

Global agent rules (routing, process ownership, waits) and the project's
`AGENTS.md`/`CLAUDE.md` always apply; this skill only adds the coordination method.

## Setup (once per effort)

- Durable backlog = GitHub issues, not local files: on start read
  `gh issue list --label coordinator-backlog --state open` in the public coordination repo AND in each private
  repo the project skill names. A mixed item: neutral
  public issue + private issue number as plain text, details only in private. Every backlog item is such an issue (labels `coordinator-backlog` +
  `next-release` when it ships next; first body line `Coordinator backlog item`, then report/PR link, closing
  criterion, status). Close an issue as soon as its fix is merged with green CI (evidence comment: PR + SHA); do not wait for release or prod checks. The in-git status file references issue
  numbers. Public issue/PR text: no secrets, host paths, app names with suffixes, account IDs, IPs, player ids,
  private repo/submodule URLs or model ids (hooks don't check issues; briefs must say so).
- Coordination dir outside git-tracked sources, e.g. `<project>/.dev/coord-<YYYYMMDD>/plans/<topic>/`
  (ask only if the project has no obvious place). It holds:
  - `tasks.txt` — append-only log, one line per event: local time, unit, model, taskId, result SHA,
    decision. This is the source of truth after context compaction.
  - `<topic>/rules.txt` (or the first unit's brief) — the shared rules header every brief references:
    contract/spec paths, repo rules, build limits, DO NOT list, integration protocol, completion
    discipline (`PARTIAL:`/`BLOCKED:` first line).
  - one brief file per unit and round: `task-<unit>-c<N>.txt`.
- Record owner decisions verbatim (log line + a memory entry) the moment they are given.

## Units

- One narrow deliverable, about 45–60 min of agent work, owned files listed, a STOP RULE with a
  `PARTIAL:` protocol, and a final-response length cap. Split anything larger; quality of cheap models
  drops on long contexts.
- Name the rabbit hole and forbid it: «if X fails, report the failing task and stop — no deep
  investigation». A child that spends an hour on archaeology is a coordination bug.
- Implementation units push only their own branch from their own worktree. Integration is a separate,
  short unit: merge the integration branch, run the FULL gates, fast-forward push only, never with a
  red gate.
- Research units are read-only and write one report file.
- Parallel units must not share files; when they would, serialize and say why in the log.

## Briefs

Write the brief to a file; the task text is a one-paragraph summary plus «Your complete brief is the
file `<path>`. Read it first and follow it exactly», plus the router's `briefHint` verbatim.
Skills are the coordinator's choice: cheap models rarely load skills on their own, so the task
text and the brief's first line name them: `SKILLS: load <a>, <b> before anything else.` Pick per unit:
`debug-flow` for any bug, red check, flake or unexplained behaviour; `ponytail` for code changes;
`impeccable` + `agent-browser` for UI; the project's own skill and remote-build skills as
the project requires. Name only what applies.
Typed header fields (short, only those that apply) replace prose rules: `SKILLS`, `owned files`, `source refs`
(repo@ref the child must read: origin/main vs candidate pin), `preserve` (behaviour that must not change), `acceptance`,
`evidence dir` (durable, never /tmp), `experiments max`, `start grant` / VM budget, `review_policy` (per the `babysit-pr`
skill: fix real bugs, decline the rest with a written reason + follow-up) and `push_budget` (default 2: initial + one
batch of the head findings), `stop`, `final cap`. PR handling follows `babysit-pr`; where this skill or project rules
conflict with it, `babysit-pr` wins (incl. rebasing a behind PR branch with `--force-with-lease`).
For concurrency/security units require a state/lock/caller map before code; start each review round as a fresh task
with head SHA + findings + objections. Keep shared rules files consolidated: one statement per rule, no history.
A brief contains: WHY (owner goal/decision), inputs with exact paths/SHAs, prior results and
coordinator decisions, YOUR STEP (lettered parts), what is NOT yours, CHECKS, STOP RULE, FINAL
RESPONSE format. Reference the shared rules file instead of repeating it. Each review round or
continuation is a new brief file and a new task with a new `clientRequestId`. Do not keep sending continuations into one child thread: its context grows every round (cheap models degrade past ~250k tokens); after 2 rounds start a fresh task with a brief that points at the reports.

Long tasks (> ~30 min: stands, benches, gate campaigns, batches, long investigations) also get a
PROGRESS/RESUME section: after each stage the child writes a human-readable summary (`progress.md`
next to the evidence: done, key numbers, next) plus machine state, so any stage can be resumed,
skipped or restarted alone. Split them into parts for cheap models. When one is interrupted, read its
progress file and continue from it — never start over.

## Dispatch

- Route every child; provider concurrency is enforced by agentdeck routing (no own per-provider cap) and the user's model preferences in memory. Request high effort for most units and xhigh for complex, security, release-critical or long multi-step units (owner default; trivial units may use lower with a logged reason); honor an explicit owner choice.
- Briefs end at the implementation deliverable: they say push (or rerun failed jobs), then finish with the PR result line — never wait on bots/CI in-turn. The coordinator owns acceptance/integration: it watches pending PRs, then dispatches the next short unit (findings fix round, or merge + issue close when green and approved).
- Dispatch async, log the taskId immediately, then end the turn.

## On completion

- Read the completion notification and saved result. Treat the summary as claims. Verify cheaply what matters: `git fetch`,
  `rev-parse`, `merge-base --is-ancestor`, `diff --stat`, the cited lines, a `--porcelain` check.
  Before any merge read the review bot's body and inline comments on the head SHA; [critical]/[major] must be
  fixed or answered on the PR. Read the production diff yourself for risky changes; spot-check one key claim of every research
  report (they can be wrong — say so to the owner when they were).
- Odd endings: a result cut mid-sentence or «waiting on …» means the child stopped early or still
  runs outside the task. Check outputs/processes, read only the tail of the
  child thread. Never redispatch blindly; continue from the pushed branch with a new short brief.
- Decide and log: accept / continuation / review round / park. Measure optimizations and do not
  integrate a change without a measured gain (park the branch, note when to re-measure).
- Clean up what children leave behind (temp worktrees, scratch files) once evidence is saved.

## Decisions and the owner

- You decide technical details within the owner's rules and the contract (schema changes through the
  documented process, accepted residuals, ordering). Write each decision into the next brief.
- The owner decides scope, product/UX behaviour, cost and release. Ask briefly, with options and a
  recommendation; if an answer is ambiguous, take the literal reading, act, and state the assumption.
- Reports: terse, in the owner's language, results first. Keep merged / published / deployed /
  verified in production separate. Correct your own earlier mistakes explicitly.
- Default: the coordinator's quota must last ~5 hours of continuous work. Per child
  result, reply in 1–3 lines (what changed, what's next, any decision needed); no explanations,
  tables or option essays unless the owner asks. Details stay in reports/logs — the owner asks a
  separate thread for status. Ask only when a decision is truly the owner's.
- Releases, tags, deploys and other outward actions need the owner's explicit OK in this conversation.

## Self-economy

Measured 2026-10-31: re-reading the context on every tool call was ~83% of a coordinator session's cost,
visible text under 3%. Every tool call you make re-reads the whole context, so fewer calls and less
output kept in context matter most.

- Read narrowly (`grep -n`, `sed -n a,bp`, `--stat`); delegate wide exploration.
- Do not re-derive what the log or a report already states.
- Do trivial one-command work yourself instead of delegating it. More than ~3 calls of your own on one
  question, or any diagnosis, log reading or diff review: delegate it to a child, which returns the conclusion.
- Pack related checks into one Bash call and cap its output (`| tail -n 40`, `--stat`, `--jq`).
  Never paste whole logs, diffs or `application-one` JSON into your context.
- `t3_thread_read`: always `limit` ≤ 10 and `maxCharsPerItem` ≤ 500, tail only. Need more: delegate.
- Before going idle (no running children, waiting for the owner): log the state to `tasks.txt` and end the turn.
- After compaction: read the tail of `tasks.txt` and the running taskIds, then continue.
