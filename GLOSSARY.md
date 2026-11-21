# Agentdeck Coordinator

Long-horizon autonomous orchestration of multi-unit software work across agent harnesses and accounts without waiting for owner interaction during execution.

## Language

**Coordinator**:
An autonomous orchestrator that plans, delegates, verifies, integrates, and ships work across a project backlog until all unblocked work is complete.
_Avoid_: Manager, interactive coordinator, assistant

**Intake**:
The single upfront alignment phase before an Autonomous Run starts, where the Coordinator clarifies requirements (via `grill-with-docs`) and establishes the Run Mandate.
_Avoid_: Onboarding, setup chat, initial Q&A

**Autonomous Run**:
The unattended execution phase following Intake in which the Coordinator drains unblocked work to completion without pausing for owner confirmation.
_Avoid_: Session, sprint, loop

**Run Mandate**:
The explicit scope of authority granted by the owner at Intake for an Autonomous Run, defining what operations (merges, releases, deployments, privileged actions) are permitted without owner intervention.
_Avoid_: Permission, approval flag, auto-ok

**Owner Inbox**:
The isolated queue of work items parked during an Autonomous Run because they physically require the owner (credentials, manual external actions) or exceed the Run Mandate.
_Avoid_: Blocked list, questions file, todo for user

**Assumption**:
A reversible product or technical decision made autonomously by the Coordinator when an unforeseen detail arises during an Autonomous Run, recorded so the owner can review or override it afterward.
_Avoid_: Guess, workaround, silent choice

**Unit**:
A single bounded deliverable delegated to a child agent with a clear scope, acceptance criteria, and stop condition.
_Avoid_: Subtask, job, ticket

**Checkpoint**:
A clean boundary in an Autonomous Run (such as a completed milestone or context threshold) where the Coordinator persists authoritative state and starts a fresh context window without losing progress.
_Avoid_: Compaction, handoff, restart

**Watchdog**:
An external liveness mechanism outside the Coordinator's own context that ensures an active Autonomous Run never stalls silently when unblocked work remains.
_Avoid_: Alarm, manual timer, reminder

**Policy Judge**:
An automated decision evaluator inside the approval broker that checks whether a privileged operation falls within the active Run Mandate.
_Avoid_: Auto-approver, LLM bypass, smart sudo

**Hard-Deny Perimeter**:
The non-overridable, code-enforced set of sensitive action classes (secret extraction, approver/security modification, unbacked production data destruction, out-of-scope hosts) that the Policy Judge may never auto-approve regardless of Run Mandate.
_Avoid_: Blocklist, safety prompt, soft guard

**Opaque Approval**:
The rule that the approval broker returns only the verdict (approved, denied, or parked) without disclosing whether the Policy Judge or the owner granted approval.
_Avoid_: Silent approval, hidden source

**Coordinator Pool**:
The dedicated set of strong model instances eligible to host the Coordinator itself when rotating off a quota-exhausted account.
_Avoid_: Model pool, fallback list, worker pool

**Run Summary**:
The single structured final report produced when an Autonomous Run finishes all unblocked work, containing deliverables, Assumptions, Owner Inbox items, and incident/metric highlights.
_Avoid_: Status update, chat log, changelog
