---
status: accepted
---

# Local Approver Broker with Policy Judge in `agentdeck`

Privileged agent operations (`agent-sudo`, `dokploy-mcp` writes, and remote host actions) move from the standalone Vercel/Upstash `agent-approver` bot into a built-in `approver` plugin hosted by the local `agentdeck daemon` (using Telegram long-polling and SSH reverse-sockets for remote hosts like `homeserver`). During an active **Autonomous Run**, a configurable **Policy Judge** (Jev or an equivalent decision model) evaluates each request against the structured **Run Mandate** under a code-enforced **Hard-Deny Perimeter**; anything outside the perimeter or below the confidence threshold escalates to a short Telegram approval window (120 s default vs 600 s interactive) before parking the branch in the **Owner Inbox**.

## Considered Options

- **Keep Vercel + Upstash webhook bot and always wait on the owner in Telegram**: Fails 3-day unattended runs whenever the owner is offline, adds external hosting dependencies even though the broker is only needed while this machine is running, and previously suffered from 20-second client timeouts leaving dangling Telegram buttons.
- **Let the Coordinator self-approve inside its own prompt**: Violates least privilege and is vulnerable to prompt injection or runaway loops without an independent enforcement boundary.

## Consequences

- **Opaque Approval**: The broker returns only `APPROVED`, `DENIED`, or `NEEDS_OWNER`, never revealing whether the Policy Judge or the owner granted approval, so the Coordinator cannot exploit owner presence.
- **Strict pre-execution timeout and disconnect cancel**: The approval timeout governs only the decision wait *before* command execution begins; once approved, execution runs to completion and returns immediately, while a caller disconnect before approval cancels the pending Telegram buttons at once (eliminating ghost runs and dangling buttons).
- **Hard-Deny Perimeter in code**: Secret extraction, approver/security/OS-layer modifications, unbacked production data destruction, and out-of-scope hosts can never be auto-approved by the Policy Judge regardless of Run Mandate, and always require human Telegram confirmation.
- **Remote hosts need the local daemon**: Remote relays (`homeserver`) reach the broker over SSH reverse-sockets instead of public HTTPS; this is intentional because remote approvals are only required while the local workstation runs agents.
- **Calibration**: Policy Judge confidence thresholds are configurable in `agentdeck` and calibrated with an offline golden + adversarial evaluation suite (and logged decision outcomes).
