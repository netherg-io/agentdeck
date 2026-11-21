---
status: accepted
---

# External Watchdog, Coordinator Pool Rotation, and Jev-Gated Checkpoints

Long-horizon **Autonomous Run** liveness, quota survival, and context rotation are enforced outside the Coordinator's context window by a two-layer **Watchdog** (proactive in-turn rotation plus a reactive **Watchdog** in `agentdeck daemon` that drives dormant or limit-failed T3 threads directly). This deliberately narrows the earlier `PLAN.md` boundary ("T3 itself is not managed"): `agentdeck` still does not start or stop T3 itself, but its daemon may configure and wake active Coordinator threads through T3's local interface when unblocked work remains.

## Considered Options

- **In-context LLM self-scheduling (`schedule_task`) on a single pinned account**: Empirically failed in the 4.5-day Blockfield run (11.4 h lost to 5h/weekly quota lockouts across 18 failed runs, 4.5 h silent stall when the LLM forgot to schedule a wake-up, a scheduled wake-up that died on a 5h limit without rescheduling, and a 2-day stale `state` snapshot).
- **Daemon alerts only (no external T3 thread control)**: Still leaves the Coordinator dead overnight whenever a sudden quota limit or provider error interrupts a turn before it can switch accounts.

## Consequences

- **Dedicated `Coordinator Pool`**: Quota rotation selects only from a dedicated pool of strong instances (never cheap worker models), picking the candidate with the fullest 5-hour window while guarding weekly quota reserves.
- **Clean sleep on full-pool exhaustion**: When every instance in `Coordinator Pool` is exhausted, the Watchdog reads the earliest `resets_at` across the pool, sets run phase to `sleeping_for_quota` (zero failed-run spam), and wakes/switches the thread at `resets_at + 2 min`.
- **Jev-gated `Checkpoint` + state freshness invariant**: Context/cache resets happen only above a context threshold or at milestone boundaries when the Checkpoint Judge confirms a clean boundary (preserving prompt cache on short turns) and `agentdeck` verifies that `state` is fresh relative to recent checklist changes.
- **Structured run state**: `thread_state` expands with `phase`, `mandate`, `owner_inbox`, and `assumptions`, shared by the Coordinator MCP tools, Watchdog, Policy Judge, and `agentdeck` GUI/applet.
