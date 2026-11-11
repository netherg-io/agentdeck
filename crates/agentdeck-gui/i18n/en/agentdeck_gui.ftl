app-title = Agentdeck

nav-instances = Instances
nav-drift = Drift
nav-processes = Processes

loading = Loading…
load-error = Could not load: { $what }

instances-title = Instances
instances-loading = Loading…
instances-empty = No instances in the registry yet.
instances-add = Add
instances-remove = Remove
instances-quotas = Quotas
instances-standalone = Other usage sources
quotas-no-data = No usage data yet.
status-enabled = enabled
status-disabled = disabled
t3-label = T3
t3-unavailable = T3 unavailable
threads-count =
    { $count ->
        [one] { $count } thread
       *[other] { $count } threads
    }

account-ok = ok
account-stale = stale
account-error = error
account-waiting = waiting
quota-in-days = resets in { $d }d { $h }h
quota-in-hours = resets in { $h }h { $m }m
quota-in-mins = resets in { $m }m

drift-title = Drift
drift-no-drift = No drift.
drift-apply = Apply
drift-import = Import
drift-import-server = Import { $server }
drift-fix = Fix (backup first)
drift-reenable = Re-enable
drift-confirm = Confirm
drift-confirm-replace = Confirm replace { $path }
drift-cancel = Cancel
drift-preview = Preview
drift-working = Working…
drift-group-t3 = T3 provider instances
drift-not-in-registry = Not in the registry.
processes-title = Processes
processes-empty = No agent processes running.
processes-stop = Stop
processes-kind-agent = session
processes-kind-t3 = T3 scope
processes-kind-harness = harness
processes-kind-container = podman
processes-confirm-title = Stop { $title }?
processes-confirm-will = These processes will be stopped:
processes-force = Force kill (SIGKILL) after a 5 s grace period
processes-confirm-ok = Stop now
processes-cancel = Cancel
processes-stop-terminated = stopped
processes-stop-killed = force-stopped
processes-stop-exited = already exited
processes-stop-running = still running after the grace period
processes-stop-result = { $title }: { $what }
coming-soon = Coming in a later unit.

plugins-confirm = Confirm
plugins-cancel = Cancel
plugins-enabled = enabled
plugins-disabled = disabled
plugins-unavailable = unavailable (needs T3)
plugins-preview-enable = Enabling { $name } will:
plugins-preview-disable = Disabling { $name } will remove its files (data is kept):
add-title = Add instance
add-back = Back to Instances
add-harness = Harness
add-id = Instance id
add-id-note = Ids are never renamed: T3 history binds to them.
add-display-name = Display name
add-home = Home directory
add-home-use-suggestion = Use suggestion
add-preview = Preview
add-apply = Apply
add-working = Working…
add-login-title = Login
add-login-start = Start login
add-login-device-auth = Use device auth (codex)
add-login-code = Pasted code
add-login-send = Send
add-login-cancel = Cancel
add-login-copy = Copy
add-login-open = Open
add-login-cancelled = Login cancelled.
add-login-done = Login process exited 0.
add-login-failed = Login failed: { $reason }.
add-login-retry-device = Port 1455 is busy; retry with device auth
add-login-device-code = Device code: { $code }
add-open-failed = Could not open the browser; copy the URL instead.
add-wrapper-title = Terminal wrapper (optional)
add-t3-title = Sync to T3 (optional)

remove-ask = Remove { $id }?
remove-confirm = Remove
remove-cancel = Keep
remove-keep-home = The login dir is kept.
remove-pool = Also removed from the pool: { $models }
remove-usage = Also removed from Quotas: { $sources }
remove-failed = Removal failed: { $what }
remove-threads =
    { $count ->
        [one] T3 keeps { $count } thread for this id; history stays but cannot launch.
       *[other] T3 keeps { $count } threads for this id; history stays but cannot launch.
    }

nav-pool = Pool
nav-shared = Shared
nav-harnesses = Harnesses
action-apply = Apply
action-cancel = Cancel
action-working = Working…
action-none = Nothing to change: already in sync.

add-login-agy = agy has no login CLI: sign in with your Google account in the Antigravity IDE. It is a single account per OS user.

pool-title = Pool
pool-empty = No pool entries yet.
pool-add-title = Add entry
pool-instance = Instance
pool-model = Model
pool-no-models = No cached models for this instance: type a model id.
pool-options = Options (k=v, comma separated)
pool-preview-add = Preview add
pool-preview-title = Preview pool change
pool-warning = Warning: { $what }

shared-title = Shared
shared-skills = Skills
shared-skills-sync = Sync skills
shared-instr = Instruction files
shared-instr-link = Link instruction files
shared-mcp = MCP servers
shared-mcp-sync = Sync MCP servers
shared-mcp-hidden = Server values are hidden.
shared-mcp-none = No canonical MCP servers found.
shared-preview-title = Preview changes

harness-title = Harnesses
harness-not-installed = not installed
harness-update = Update
harness-install = Install
harness-run-title = Run this command?
harness-result = { $name }: { $what }

nav-group-accounts = Accounts
nav-group-system = System
processes-filter = Filter by name or command
processes-no-match = No process group matches the filter.
processes-count = { $count ->
    [one] { $count } process
   *[other] { $count } processes
}
remove-done = Removed { $id }.

drift-loading = Checking for drift…
drift-all-clear = All clear: every instance matches.
pool-add = Add
pool-col-options = Options
pool-col-on = Routing
plugins-details = Technical status
plugins-no-details = No details.
shared-ok-count = { $count } ok
shared-todo-count = { $count } to sync
shared-skills-none = No skill directories found.
shared-instr-none = No instruction files found.
shared-mcp-servers = Canonical servers
shared-mcp-targets = Targets
add-step-details = Details
add-step-preview = Preview
add-step-login = Login
add-step-finish = Finish
add-next = Next
add-edit = Back
add-done = Done
add-added = Added { $id }.

pool-search = Search
pool-all-instances = All instances
pool-sort-instance = By instance
pool-sort-model = By model
pool-sort-enabled = Enabled first
pool-no-match = No models match.
instances-rename = Rename
rename-save = Save

instances-cli = CLI
cli-open-error = Could not open CLI: { $what }

parallel-label = Parallel tasks
parallel-unlimited = Unlimited
parallel-invalid = Enter a whole number from 1 to 4294967295, or leave blank for unlimited.
nav-group-usage = Quotas
nav-group-router = Router
nav-group-agents = Agents
nav-usage-sources = Sources
nav-usage-widget = Panel
nav-limits = Limits
usage-sources-title = Quota sources
usage-widget-title = Panel widget
limits-title = Parallel limits
usage-add = Add source
usage-add-title = New quota source
usage-preview-add = Preview add
usage-preview-title = Quota source changes
usage-kind = Provider
usage-kind-missing = Choose a provider.
usage-id = ID
usage-display = Display name
usage-config-dir = Claude config directory
usage-codex-home = CODEX_HOME directory
usage-api-key-env = API key variable
usage-token-file = Token file
usage-db-file = OpenCode database
usage-optional = { $what } (optional)
usage-instance = Instance
usage-instance-none = Not bound
usage-empty = No quota sources.
usage-widget-scope = Hidden providers leave the COSMIC panel only; polling and routing still use them.
usage-order-title = Panel order
usage-sources-count = { $count ->
        [one] { $count } source
       *[other] { $count } sources
    }
sync-auto-apply = Auto-apply

nav-usage-poll = Polling
nav-router-settings = Decisions
nav-modules = Modules
usage-poll-title = Quota polling
router-settings-title = Routing decisions
modules-title = Modules
settings-save = Save
settings-saved = Saved
settings-seconds-invalid = Enter a whole number of seconds, or leave blank for the default
settings-decision-model = Decision model
settings-decision-model-hint = Model that picks the target for each delegated task. Blank uses the default.
settings-key-placeholder = Paste a new key to replace
settings-key-set = Key is set. Leave blank to keep it.
settings-key-missing = No key: routing fails until one is set.
settings-refresh = Refresh interval, s
settings-refresh-hint = At least { $min } s. Applied on the next daemon cycle.
settings-stale = Stale after, s
settings-stale-hint = Cached quota data older than this is marked stale.
pool-remove-tip = Remove this custom entry
usage-api-key = API key
usage-api-key-placeholder = Leave blank if the variable is already set
usage-key-write = Set { $var } in { $path } (0600)
t3-ready = T3 ready
t3-disabled = T3 disabled
settings-decision-provider = Decision provider
settings-decision-model-vercel = Gateway model id, e.g. openai/gpt-5-mini. Required.
settings-model-required = Vercel AI Gateway needs a decision model
settings-key = { $provider } API key
pool-show-legacy = Legacy ({ $count })
settings-test = Check
settings-testing = Checking…
settings-test-ok = { $model } works: size { $size }, difficulty { $difficulty }, { $ms } ms{ $cost }
settings-rules = Model rules
settings-rules-source-default = Built-in defaults
settings-rules-source-user = Your saved rules
settings-rules-placeholder = regex, comma-separated
settings-no-review = No-review models
settings-no-review-hint = Regex list, comma-separated. Escape commas with \,. Blank disables the ban.
settings-deny = Denied models
settings-deny-hint = Regex list, comma-separated. Never picked automatically.
settings-rules-note = A model you name directly can still override these rules.
settings-eval-title = Assessment history
settings-eval-note = Observations only; routing does not use them.
settings-eval-missing = No assessment file yet; nothing has been recorded.
settings-eval-empty = No assessments recorded yet.
settings-eval-unreadable = Could not read assessments: { $what }
settings-eval-corrupt = Assessment file is damaged at line { $line }: { $what }. The file was left unchanged.
settings-eval-refresh = Refresh
settings-eval-self = self-check
settings-eval-self-unknown = self-check unknown
settings-eval-cost = cost ${ $cost }
settings-eval-cost-unknown = cost unreported
settings-eval-evidence-ids = { $count } evidence ids
settings-eval-judge = judge { $who }
settings-eval-confidence = confidence { $value }
settings-eval-workflow = workflow { $run }/{ $step }

settings-eval-minutes-ago = { $count } min ago
settings-eval-hours-ago = { $count } h ago
settings-eval-days-ago = { $count } d ago

drift-instr-use-shared = Use shared file
drift-instr-use-local = Make shared
drift-instr-link-consequence = { $path } will link to ~/AGENTS.md.
drift-instr-import-consequence = This version will replace ~/AGENTS.md and update every file linked to it.
drift-instr-backup = Backup will be saved to { $path }
drift-instr-resolved = Instruction file resolved.
drift-instr-compare = Compare instructions
