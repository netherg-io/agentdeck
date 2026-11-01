app-title = Agentdeck

nav-instances = Instances
nav-drift = Drift
nav-plugins = Plugins
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
plugins-title = Plugins
processes-title = Processes
processes-empty = No agent processes running.
processes-stop = Stop
processes-kind-agent = session
processes-kind-t3 = T3 scope
processes-kind-harness = harness
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

plugins-empty = No plugins available.
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
pool-col-on = On
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

pool-search = Search models or instances
pool-all-instances = All instances
pool-sort-instance = By instance
pool-sort-model = By model
pool-sort-enabled = Enabled first
pool-no-match = No models match.
instances-rename = Rename
rename-save = Save
