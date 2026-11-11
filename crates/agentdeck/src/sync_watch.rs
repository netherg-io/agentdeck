//! Background sync watcher for `agentdeck daemon`: notify/debouncer-mini file
//! watch (2 s) over the managed config parents plus a 10-minute rescan.
//! Additive drift auto-applies only when `[sync] auto_apply` is on (OFF by
//! default); new DECISION sets send one deduped desktop notification.
//! Watcher failures are logged, never fatal to quota polling.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agentdeck_core::{Paths, Registry, sync};

/// Debounce quiet period after the last fs event before a scan.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// Periodic rescan covering edits the watcher missed (and new instances).
pub const RESCAN: Duration = Duration::from_secs(10 * 60);

/// Roots whose direct children matter: registry file parent, Claude profile
/// and `.claude.json` parents, Codex main/shadow homes. Non-recursive: link-
/// level drift lives at the top level; the rescan covers deeper entries
/// (`.hook-trust` records, sessions churn would otherwise rescan constantly).
fn watch_roots(paths: &Paths, home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![paths.config_dir.clone(), home.to_path_buf()];
    roots.extend(
        agentdeck_core::shared::instruction_targets(home)
            .into_iter()
            .filter_map(|(path, _)| path.parent().map(Path::to_path_buf)),
    );
    let Ok(reg) = Registry::load(paths) else {
        return roots;
    };
    let primary = home.join(".claude");
    if primary.is_dir() {
        roots.push(primary);
    }
    roots.push(home.join(".codex"));
    for inst in &reg.instances {
        let dir = sync_expand(&inst.home, home);
        if inst.harness == agentdeck_core::Harness::Claude
            || inst.harness == agentdeck_core::Harness::Codex
        {
            roots.push(dir);
        }
    }
    roots.sort();
    roots.dedup();
    roots.into_iter().filter(|p| p.is_dir()).collect()
}

fn sync_expand(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(value)
    }
}

/// One pass: gather drift, auto-apply additive items when enabled, then
/// notify (via `notify`, injectable in tests) when the DECISION set changed.
/// Never writes decision content. Returns the decision count.
pub fn scan_once(
    paths: &Paths,
    home: &Path,
    t3_root: Option<&Path>,
    notify: &dyn Fn(String, String),
) -> usize {
    let items = match sync::gather(paths, home, t3_root) {
        Ok(items) => items,
        Err(e) => {
            eprintln!("agentdeck sync: gather failed: {e:#}");
            return 0;
        }
    };
    if sync::auto_apply_enabled(paths) {
        match sync::apply_additive(paths, home, t3_root) {
            Ok(applied) => {
                for c in &applied {
                    // Value-safe: .claude.json writes show only redacted diffs.
                    eprintln!(
                        "agentdeck sync: {}",
                        agentdeck_core::claude::describe_change(c)
                    );
                }
            }
            Err(e) => eprintln!("agentdeck sync: additive apply failed: {e:#}"),
        }
    } else {
        let n = items
            .iter()
            .filter(|i| i.kind == sync::ItemKind::Additive)
            .count();
        if n > 0 {
            eprintln!(
                "agentdeck sync: auto-apply is off: {n} additive item(s) reported, nothing written"
            );
        }
    }
    // Re-gather after a possible apply so the notified set is current.
    let items = sync::gather(paths, home, t3_root).unwrap_or(items);
    let ids = sync::decision_instances(&items);
    if ids.is_empty() {
        // Drift resolved: clear so the next set notifies again.
        if sync::last_notified(&paths.state_dir).is_some() {
            let _ = sync::store_notified(&paths.state_dir, None);
        }
        return 0;
    }
    let hash = sync::decision_hash(&items);
    if sync::last_notified(&paths.state_dir) == Some(hash) {
        return ids.len(); // already notified, no repeat
    }
    let title = "agentdeck drift".to_string();
    let body = format!("drift in {}: Open agentdeck -> Drift", ids.join(", "));
    notify(title, body);
    if let Err(e) = sync::store_notified(&paths.state_dir, Some(hash)) {
        eprintln!("agentdeck sync: cannot remember notification: {e:#}");
    }
    ids.len()
}

/// Production notifier: `notify-send` with the agentdeck app name. Text names
/// instances only, never values. A missing notify-send is a log line.
pub fn notify_send(title: String, body: String) {
    let out = std::process::Command::new("notify-send")
        .arg("-a")
        .arg("agentdeck")
        .arg(&title)
        .arg(&body)
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!(
            "agentdeck sync: notify-send failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("agentdeck sync: notify-send not found, skipping desktop notification");
        }
        Err(e) => eprintln!("agentdeck sync: notify-send failed: {e:#}"),
    }
}

/// Run until cancelled. Watcher errors are logged; this future only ends on
/// error or cancellation and must never take quota polling down with it.
pub async fn run(paths: Paths) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home: HOME is not set"))?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);
    let mut debouncer = notify_debouncer_mini::new_debouncer(
        DEBOUNCE,
        move |res: notify_debouncer_mini::DebounceEventResult| {
            if res.is_ok() {
                let _ = tx.try_send(());
            }
        },
    )
    .map_err(|e| anyhow::anyhow!("sync watcher failed to start: {e}"))?;
    for root in watch_roots(&paths, &home) {
        if let Err(e) = debouncer
            .watcher()
            .watch(&root, notify::RecursiveMode::NonRecursive)
        {
            eprintln!("agentdeck sync: cannot watch {}: {e:#}", root.display());
        }
    }
    let mut rescan = tokio::time::interval(RESCAN);
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = rx.recv() => {
                // Debouncer already coalesced the burst; drain the backlog.
                while rx.try_recv().is_ok() {}
            }
            _ = rescan.tick() => {}
        }
        let t3_root = agentdeck_core::t3::t3_root().ok();
        scan_once(&paths, &home, t3_root.as_deref(), &notify_send);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Guard, Paths, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-watch-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let dot = home.join(".claude");
        for name in ["skills", "agents", "plugins"] {
            fs::create_dir_all(dot.join(name)).unwrap();
        }
        fs::write(home.join("AGENTS.md"), "me").unwrap();
        std::os::unix::fs::symlink(home.join("AGENTS.md"), dot.join("CLAUDE.md")).unwrap();
        fs::write(dot.join("settings.json"), "{\"theme\":\"dark\"}\n").unwrap();
        fs::write(
            home.join(".claude.json"),
            "{\"mcpServers\":{\"srv\":{\"command\":\"x\"}}}",
        )
        .unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(home.join(".codex/config.toml"), "x = 1\n").unwrap();
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        agentdeck_core::Registry::update(&paths, |r| {
            r.add(agentdeck_core::registry::AddSpec {
                id: "cl-second".into(),
                harness: agentdeck_core::Harness::Claude,
                display_name: None,
                home: home.join("claude-second").to_string_lossy().into_owned(),
                binary: None,
                no_t3: true,
            })?;
            Ok(())
        })
        .unwrap();
        (Guard(dir), paths, home)
    }

    type Log = Arc<Mutex<Vec<(String, String)>>>;

    fn recorder() -> (Log, impl Fn(String, String) + Clone) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let inner = log.clone();
        let f = move |t: String, b: String| inner.lock().unwrap().push((t, b));
        (log, f)
    }

    #[test]
    fn global_instruction_drift_notifies_and_watches_every_harness() {
        let (_g, paths, home) = fixture();
        sync::set_auto_apply(&paths, true, false).unwrap();
        let (log, notify) = recorder();
        scan_once(&paths, &home, None, &notify);
        assert!(log.lock().unwrap().is_empty());
        for (path, _) in agentdeck_core::shared::instruction_targets(&home) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let _ = fs::remove_file(&path);
            fs::write(&path, "private client rules\n").unwrap();
        }
        let roots = watch_roots(&paths, &home);
        for (path, _) in agentdeck_core::shared::instruction_targets(&home) {
            assert!(roots.contains(&path.parent().unwrap().to_path_buf()));
        }
        assert!(roots.contains(&home));
        assert_eq!(scan_once(&paths, &home, None, &notify), 1);
        assert_eq!(log.lock().unwrap().len(), 1);
        let body = log.lock().unwrap()[0].1.clone();
        assert!(body.contains("global-instructions"), "{body}");
        assert!(!body.contains("private client rules"));
        scan_once(&paths, &home, None, &notify);
        assert_eq!(log.lock().unwrap().len(), 1);
        for (path, _) in agentdeck_core::shared::instruction_targets(&home) {
            assert_eq!(fs::read_to_string(path).unwrap(), "private client rules\n");
        }
    }

    #[test]
    fn notify_dedupe_with_injected_notifier() {
        let (_g, paths, home) = fixture();
        let (log, notify) = recorder();
        // Materialize the secondary first (additive only, no notification).
        sync::set_auto_apply(&paths, true, false).unwrap();
        assert_eq!(scan_once(&paths, &home, None, &notify), 0);
        assert!(log.lock().unwrap().is_empty());
        // Decision drift: exactly one notification, no repeat.
        let cl = home.join("claude-second");
        fs::write(cl.join("settings.json"), "{\"theme\":\"light\"}\n").unwrap();
        assert_eq!(scan_once(&paths, &home, None, &notify), 1);
        assert_eq!(log.lock().unwrap().len(), 1);
        assert_eq!(scan_once(&paths, &home, None, &notify), 1);
        assert_eq!(log.lock().unwrap().len(), 1);
        // Notification names the instance and carries no values.
        let body = log.lock().unwrap()[0].1.clone();
        assert!(body.contains("cl-second"), "{body}");
        assert!(body.contains("Open agentdeck -> Drift"), "{body}");
        assert!(!body.contains("light"), "{body}");
        // A changed set (a second decision item) notifies again.
        fs::write(
            cl.join("settings.json"),
            "{\"theme\":\"light\",\"mine\":1}\n",
        )
        .unwrap();
        assert_eq!(scan_once(&paths, &home, None, &notify), 1);
        assert_eq!(log.lock().unwrap().len(), 2);
        // Resolved drift clears the memory: the next set notifies again.
        fs::write(cl.join("settings.json"), "{\"theme\":\"dark\"}\n").unwrap();
        assert_eq!(scan_once(&paths, &home, None, &notify), 0);
        fs::write(cl.join("settings.json"), "{\"theme\":\"light\"}\n").unwrap();
        assert_eq!(scan_once(&paths, &home, None, &notify), 1);
        assert_eq!(log.lock().unwrap().len(), 3);
    }

    #[test]
    fn auto_apply_gate_defaults_off() {
        let (_g, paths, home) = fixture();
        let (log, notify) = recorder();
        // Additive drift with the flag off: reported, nothing written.
        scan_once(&paths, &home, None, &notify);
        assert!(log.lock().unwrap().is_empty());
        assert!(
            fs::symlink_metadata(home.join("claude-second")).is_err(),
            "flag off must not create the secondary home"
        );
        // Flag on: the same pass applies additive drift.
        sync::set_auto_apply(&paths, true, false).unwrap();
        scan_once(&paths, &home, None, &notify);
        let skills_link = home.join("claude-second/skills");
        assert_eq!(
            fs::read_link(&skills_link).unwrap(),
            home.join(".claude/skills")
        );
        assert!(
            log.lock().unwrap().is_empty(),
            "additive apply must not notify"
        );
    }

    /// Coalescing primitive behind the watcher: a burst of triggers inside
    /// one quiet window produces a single scan.
    pub async fn coalesce_until_quiet(
        rx: &mut tokio::sync::mpsc::Receiver<()>,
        quiet: Duration,
        scans: &Arc<Mutex<u32>>,
    ) {
        tokio::time::sleep(quiet).await;
        while rx.try_recv().is_ok() {}
        *scans.lock().unwrap() += 1;
    }

    #[tokio::test]
    async fn debounce_coalesces_a_burst() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(32);
        let scans: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        // A burst of 20 triggers: one quiet window, one scan.
        for _ in 0..20 {
            tx.try_send(()).unwrap();
        }
        coalesce_until_quiet(&mut rx, Duration::from_millis(50), &scans).await;
        assert_eq!(*scans.lock().unwrap(), 1);
        assert!(rx.try_recv().is_err(), "burst must be drained");
    }
}
