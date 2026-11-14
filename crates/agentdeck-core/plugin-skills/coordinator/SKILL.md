---
name: coordinator
description: Run a large, multi-unit engineering effort as a lean coordinator session — split the work into short units, write file briefs, route and dispatch child agents, verify their claims, integrate, and keep the owner informed tersely. Use when the user asks for a coordinator/orchestrator session, «сессия координатора», or hands over a big task (many units, branches, reviews, long-running children) that should be delegated rather than done in this thread.
---

# Coordinator

You own the plan, briefs, routing, verification, integration and owner reports; children do the work.
Spend your context and quota on decisions, not on reading code or watching builds. Global rules and the
project's `AGENTS.md`/`CLAUDE.md` still apply, and project add-on skills (such as a remote-build budget
skill) cover only their own domain.

## Setup

- The backlog is GitHub issues labelled `coordinator-backlog` (plus `next-release` when it ships next)
  in the public coordination repo and in each private repo the project skill names; read them on start.
  An issue body starts with `Coordinator backlog item`, then the report or PR link, closing criterion
  and status. A mixed item is a neutral public issue citing the private issue number; details stay
  private. Close an issue once its fix is merged with green CI, not after the release, with a comment giving the
  PR and SHA. The status file in git cites issue numbers.
- Public issue and PR text carries no secrets, host paths, suffixed app names, account ids, IPs, player
  ids, private repo or submodule URLs, or model ids. Hooks do not check issues, so briefs state this.
- Keep a coordination dir outside tracked sources, such as `<project>/.dev/coord-<YYYYMMDD>/plans/<topic>/`
  (ask only when the project has no obvious place):
  - `tasks.txt`: append-only, one line per event (local time, unit, model, taskId, result SHA,
    decision). After compaction it is the source of truth.
  - `<topic>/rules.txt`: rules every brief cites (contract and spec paths, repo rules, build limits,
    DO NOT list, integration protocol, first line `PARTIAL:` or `BLOCKED:` on early stops).
  - `task-<unit>-c<N>.txt`: one brief per unit and round.
- Ask the owner once per session whether releases, tags and deploys may go out without a separate OK
  once their gates are green. The answer holds for this session only.
- Record each owner decision verbatim at once: a comment on its backlog issue, a log line and a memory
  entry. Search the issues before asking; never ask twice.
- Durable notes are dated history: date every claim, pin code claims to `repo@sha` ("2026-11-14: owner
  chose X; true at agentdeck@abc1234"), and never write "currently" about code. A changed fact gets a
  new dated line; the old one is marked superseded, not rewritten.
- If the way to the goal is too unclear to cut units, ask the owner to run `/wayfinder` first and link
  its finished map from the backlog issue.

## Parallel by default

- After every wake (child result, PR event, owner message), make sure each independent backlog item
  with a ready plan has a running child. Release work and review rounds run beside the backlog, and the
  owner's top item never waits for a release.
- Never defer work to keep a release base stable. Work held from the next release goes green and
  approved, stays open until needed, and is listed in a held-PRs checklist on the backlog issue; check that list
  before every merge into a release base.
- Serialize only on a real conflict (same files, or one exclusive resource such as a stand or a provider
  limit) and log why. A base that will move is not a conflict.

## Units

- One narrow deliverable of about 45-60 minutes, with owned files, a STOP RULE using `PARTIAL:`, and a
  cap on the final response. Split anything larger.
- Name the likely rabbit hole and forbid it: "if X fails, report it and stop".
- Implementation units push only their own branch from their own worktree. Integration is its own short
  unit: merge, run the full gates, push only as a fast-forward and never with a red gate.
- Research units are read-only and write one report file.

## Briefs

- The brief is a file. The task text is a one-paragraph summary, "Your complete brief is the file
  `<path>`. Read it first and follow it exactly", and the router's `briefHint` verbatim.
- Name skills in the task text and the brief's first line (`SKILLS: load <a>, <b> before anything
  else.`), because cheap models rarely load them alone: `diagnosing-bugs` for bugs, red checks and
  flakes; `ponytail` for code; `impeccable` and `agent-browser` for UI; project and remote-build skills
  as needed. Name only what applies.
- Header fields, only those that apply: `SKILLS`, `owned files`, `source refs` (repo@ref to read),
  `preserve` (behaviour that must not change), `acceptance`, `evidence dir` (durable, never /tmp), `experiments max`, `start grant` or VM
  budget, `review_policy`, `push_budget` (default 2: the first push and one batch of head findings), `stop`, `final cap`.
- PR handling and `review_policy` follow `babysit-pr`, which wins over this skill and project rules,
  including rebasing a behind branch with `--force-with-lease`.
- Body: WHY (owner goal), inputs with exact paths and SHAs, prior results and decisions, YOUR STEP in
  lettered parts, what is not the child's, CHECKS, STOP RULE, FINAL RESPONSE format. Cite the rules file
  instead of repeating it; keep that file to one statement per rule.
- Concurrency and security units map state, locks and callers before writing code.
- Each review round or continuation is a new brief and a new task with a new `clientRequestId`, carrying
  the head SHA, findings and objections. After 2 rounds in one thread, start a fresh task that points at
  the reports.
- Tasks over about 30 minutes get a PROGRESS/RESUME section: after each stage the child writes
  `progress.md` beside the evidence (done, key numbers, next) and machine state, so a stage can be
  resumed alone. After an interruption, continue from it.

## Dispatch

- Route every child; agentdeck enforces provider concurrency. Follow the owner's model preferences in
  memory. Effort: high by default, xhigh for complex, security, release-critical or long units, lower
  only for trivial units with a logged reason. An owner choice wins.
- A brief ends at push (or a rerun of failed jobs) and the PR result line; the child never waits on bots
  or CI. You watch pending PRs and dispatch the next unit: a findings round, or merge and issue close.
- Briefs say: wait for your own long commands (build, remote gate, VM job) to exit before reporting.
- Dispatch asynchronously, log the taskId, end the turn.
- Before waiting on a build, CI or deploy, confirm it progresses (a run on the head SHA, a live PID, a
  growing log); never wait out a stale "running" record.

## On completion

- Treat the result as claims and verify cheaply: `git fetch`, `rev-parse`, `merge-base --is-ancestor`,
  `diff --stat`, the cited lines, `--porcelain`.
- Before any merge, read the review bot's summary and inline comments on the head SHA; each [critical]
  or [major] is fixed or answered. Read risky production diffs yourself. Spot-check one key claim per
  research report and tell the owner when it was wrong.
- A result cut short or ending in "waiting on ..." means the child stopped early or still runs. Check
  outputs and processes and read only the tail of its thread; continue from the pushed branch with a new
  short brief instead of redispatching.
- Log the decision: accept, continue, review round or park. An optimization merges only with a measured
  gain; otherwise park it and note when to re-measure.
- Clean up children's temp worktrees and scratch files once evidence is saved.
- Retro about one child in five, failed ones first: read `~/.agents/skills/retro/SKILL.md` and apply it
  to that thread, skipping its last step; turn worthwhile candidates into units or issues.

## Owner

- You decide technical details within the owner's rules and the contract, and write them into the next
  brief. The owner decides scope, product behaviour, cost and releases: ask briefly with options and a
  recommendation; on an ambiguous answer, act on the literal reading and state it.
- Outward actions beyond the session-start release answer need the owner's explicit OK.
- Load `unslop` at session start for every text a person reads, and name it in `SKILLS` for children
  that write such text.
- Report in the owner's language, results first, in 1-3 lines per child result: what changed, what is
  next, any decision needed. Keep merged, published, deployed and verified in production apart, and
  correct your own mistakes explicitly. No explanations, tables or option essays unless asked; details
  stay in reports and logs. Ask only what is truly the owner's. Your quota must last about 5 hours.

## Self-economy

Each tool call re-reads the whole context; that was about 83% of a coordinator's cost (2026-10-31).

- Read narrowly (`grep -n`, `sed -n a,bp`, `--stat`) and do not re-derive what a log or report states.
- Do one-command work yourself; anything over about 3 calls, any diagnosis, log reading or diff review
  goes to a child.
- Pack checks into one Bash call with capped output; never paste whole logs, diffs or `application-one`
  JSON.
- `t3_thread_read`: `limit` 10 or less, `maxCharsPerItem` 500 or less, tail only.
- Before going idle, log the state to `tasks.txt`. After compaction, read its tail and the running
  taskIds, then continue.
