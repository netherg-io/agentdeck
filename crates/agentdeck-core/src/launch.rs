//! Launch profile CLIs directly or in an external terminal.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::registry::Harness;
use crate::{Paths, Registry};

/// Codex file credential store, same literal arg as login and wrappers use.
const CODEX_FILE_STORE: &str = r#"cli_auth_credentials_store="file""#;

// COSMIC's command separator is parsed in pop-os/cosmic-term/src/main.rs,
// although its --help output does not document it.
const TERMINALS: &[(&str, &[&str])] = &[
    ("xdg-terminal-exec", &[]),
    ("cosmic-term", &["-e"]),
    ("gnome-terminal", &["--"]),
    ("konsole", &["-e"]),
    ("xterm", &["-e"]),
];

/// Resolved program + exact args + env pinning for one profile CLI run.
#[derive(Debug, Clone)]
struct LaunchSpec {
    program: String,
    args: Vec<OsString>,
    env_set: Vec<(String, String)>,
    env_remove: Vec<String>,
}

/// Expand a leading `~/` against `home`; anything else passes through.
fn expand_home(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(h) = home
    {
        return h.join(rest);
    }
    PathBuf::from(raw)
}

fn unknown_id(reg: &Registry, id: &str) -> anyhow::Error {
    let mut known: Vec<&str> = reg.instances.iter().map(|i| i.id.as_str()).collect();
    known.sort_unstable();
    let hint = if known.is_empty() {
        "the registry has no instances".to_string()
    } else {
        format!("known instances: {}", known.join(", "))
    };
    anyhow::anyhow!("unknown instance id {id:?} ({hint})")
}

/// Pure command building for [`run_cli`]; `home` is the caller's `$HOME`.
/// Never touches processes, so tests inspect it with fake scripts only.
fn launch_spec(
    paths: &Paths,
    id: &str,
    args: &[OsString],
    home: Option<&Path>,
) -> anyhow::Result<LaunchSpec> {
    let reg = Registry::load(paths)?;
    let inst = reg.get(id).ok_or_else(|| unknown_id(&reg, id))?;
    let home_dir = expand_home(&inst.home, home);
    let binary = inst
        .binary
        .clone()
        .unwrap_or_else(|| inst.harness.as_str().to_string());
    let mut env_set = Vec::new();
    let mut env_remove = Vec::new();
    // Values stay single argv entries throughout: no shell is ever involved.
    let mut argv: Vec<OsString> = Vec::with_capacity(args.len() + 2);
    match inst.harness {
        Harness::Codex => {
            env_set.push(("CODEX_HOME".to_string(), home_dir.display().to_string()));
            argv.push(OsString::from("-c"));
            argv.push(OsString::from(CODEX_FILE_STORE));
        }
        Harness::Claude => {
            // The primary profile keeps its default location; drop a foreign
            // override so another profile's dir never leaks in.
            if home.is_some_and(|h| h.join(".claude") == home_dir) {
                env_remove.push("CLAUDE_CONFIG_DIR".to_string());
            } else {
                env_set.push((
                    "CLAUDE_CONFIG_DIR".to_string(),
                    home_dir.display().to_string(),
                ));
            }
        }
        Harness::Opencode => {
            // Reuse the login adapter: data dir -> XDG_DATA_HOME=<parent>.
            // Real HOME is never touched.
            env_set.extend(crate::opencode::command_env(&home_dir)?);
        }
        Harness::Agy if !matches!(inst.id.as_str(), "agy" | "antigravity") => {
            env_set.push(("HOME".to_string(), home_dir.display().to_string()));
        }
        Harness::Agy => {}
    }
    argv.extend(args.iter().cloned());
    Ok(LaunchSpec {
        program: binary,
        args: argv,
        env_set,
        env_remove,
    })
}

/// Replace this process with the profile CLI. Call before any tokio runtime
/// or secret loading; on success this never returns.
pub fn run_cli(paths: &crate::Paths, id: &str, args: &[std::ffi::OsString]) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME");
    let spec = launch_spec(paths, id, args, home.as_deref().map(Path::new))?;
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.env_set {
        cmd.env(key, value);
    }
    use std::os::unix::process::CommandExt as _;
    let err = cmd.exec();
    anyhow::bail!("cannot exec {:?}: {err}", spec.program)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn find_on_path(name: &str, path_env: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// First usable terminal on `PATH`: binary plus its exec-separator prefix.
fn find_terminal(path_env: &OsStr) -> Option<(PathBuf, &'static [&'static str])> {
    TERMINALS
        .iter()
        .find_map(|(name, prefix)| find_on_path(name, path_env).map(|bin| (bin, *prefix)))
}

/// Full terminal argv: `terminal <prefix> <agentdeck> instance run <id>`.
/// Every element stays one argv entry; never rendered through a shell.
fn terminal_command(
    terminal: &Path,
    prefix: &[&str],
    exe: &Path,
    id: &str,
    paths: &Paths,
) -> Vec<OsString> {
    let mut argv = Vec::new();
    argv.push(terminal.as_os_str().to_owned());
    argv.extend(prefix.iter().map(OsString::from));
    // A terminal server may discard the client's environment; pass registry
    // roots as arguments to env so the inner Agentdeck uses the same profile.
    argv.push(OsString::from("/usr/bin/env"));
    for (key, path) in [
        ("AGENTDECK_CONFIG_DIR", &paths.config_dir),
        ("AGENTDECK_STATE_DIR", &paths.state_dir),
    ] {
        let mut value = OsString::from(format!("{key}="));
        value.push(path);
        argv.push(value);
    }
    argv.push(exe.as_os_str().to_owned());
    argv.push(OsString::from("instance"));
    argv.push(OsString::from("run"));
    argv.push(OsString::from(id));
    argv
}

fn check_binary(binary: &str, inst: &crate::registry::Instance) -> anyhow::Result<()> {
    if binary.contains('/') {
        let path = Path::new(binary);
        if is_executable(path) {
            return Ok(());
        }
        anyhow::bail!(
            "cannot launch {:?}: executable {binary:?} not found or not executable; fix the instance's `binary` or install the {} CLI",
            inst.id,
            inst.harness.as_str(),
        );
    }
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    if find_on_path(binary, &path_env).is_some() {
        return Ok(());
    }
    anyhow::bail!(
        "cannot launch {:?}: executable {binary:?} not found on PATH; install the {} CLI first",
        inst.id,
        inst.harness.as_str(),
    )
}

/// Open an external terminal running this executable as
/// `agentdeck instance run <id>`. Returns once the terminal is spawned; a
/// detached thread reaps the child so the GUI never blocks. Never
/// stops or signals any process.
pub fn launch_cli(paths: &crate::Paths, id: &str) -> anyhow::Result<()> {
    let reg = Registry::load(paths)?;
    let inst = reg.get(id).ok_or_else(|| unknown_id(&reg, id))?;
    let binary = inst
        .binary
        .clone()
        .unwrap_or_else(|| inst.harness.as_str().to_string());
    check_binary(&binary, inst)?;
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let (terminal, prefix) = find_terminal(&path_env).ok_or_else(|| {
        let names: Vec<&str> = TERMINALS.iter().map(|(n, _)| *n).collect();
        anyhow::anyhow!(
            "cannot launch {id:?}: no terminal emulator found (looked for {}); install one to open profile CLIs from the GUI",
            names.join(", "),
        )
    })?;
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot find the running agentdeck executable: {e}"))?;
    let argv = terminal_command(&terminal, prefix, &exe, &inst.id, paths);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot open terminal {}: {e}", terminal.display()))?;
    // Reap without blocking the caller; the child is never signalled.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Paths, Guard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-launch-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (
            Paths::new(dir.join("config"), dir.join("state")),
            Guard(dir),
        )
    }

    fn add(paths: &Paths, id: &str, harness: Harness, home: &str, binary: Option<&str>) {
        Registry::update(paths, |r| {
            r.add(crate::registry::AddSpec {
                id: id.to_string(),
                harness,
                display_name: None,
                home: home.to_string(),
                binary: binary.map(str::to_string),
                no_t3: false,
            })?;
            Ok(())
        })
        .unwrap();
    }

    fn home() -> PathBuf {
        PathBuf::from("/home/u")
    }

    #[test]
    fn codex_env_and_argv_passthrough() {
        let (paths, _g) = fixture();
        add(&paths, "cx", Harness::Codex, "/tmp/o'brien dir/x", None);
        let args: Vec<OsString> = vec![
            "chat".into(),
            "it's --weird".into(),
            "$(rm -rf ~)".into(),
            "--flag=va l".into(),
        ];
        let spec = launch_spec(&paths, "cx", &args, Some(home().as_path())).unwrap();
        assert_eq!(spec.program, "codex");
        // File-store prefix first, user args verbatim after: no shell parsing.
        assert_eq!(
            spec.args,
            vec![
                OsString::from("-c"),
                OsString::from(CODEX_FILE_STORE),
                OsString::from("chat"),
                OsString::from("it's --weird"),
                OsString::from("$(rm -rf ~)"),
                OsString::from("--flag=va l"),
            ]
        );
        assert!(spec.args.iter().all(|a| !a.is_empty()));
        assert_eq!(
            spec.env_set,
            vec![("CODEX_HOME".to_string(), "/tmp/o'brien dir/x".to_string())]
        );
        assert!(spec.env_remove.is_empty());
    }

    #[test]
    fn claude_primary_gets_no_override() {
        let (paths, _g) = fixture();
        add(&paths, "cl", Harness::Claude, "~/.claude", None);
        let spec = launch_spec(&paths, "cl", &[], Some(home().as_path())).unwrap();
        assert!(spec.env_set.is_empty());
        assert_eq!(spec.env_remove, vec!["CLAUDE_CONFIG_DIR".to_string()]);
        assert_eq!(spec.args, Vec::<OsString>::new());
    }

    #[test]
    fn claude_secondary_pins_dir() {
        let (paths, _g) = fixture();
        add(&paths, "cl2", Harness::Claude, "~/.claude-second", None);
        let spec = launch_spec(&paths, "cl2", &[], Some(home().as_path())).unwrap();
        assert_eq!(
            spec.env_set,
            vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/home/u/.claude-second".to_string()
            )]
        );
        assert!(spec.env_remove.is_empty());
    }

    #[test]
    fn opencode_points_at_parent_and_keeps_home() {
        let (paths, _g) = fixture();
        add(&paths, "op", Harness::Opencode, "/d/share/opencode", None);
        let spec = launch_spec(&paths, "op", &[], Some(home().as_path())).unwrap();
        assert_eq!(
            spec.env_set,
            vec![("XDG_DATA_HOME".to_string(), "/d/share".to_string())]
        );
        assert!(spec.env_remove.is_empty());
    }

    #[test]
    fn opencode_root_home_errors() {
        let (paths, _g) = fixture();
        add(&paths, "op", Harness::Opencode, "/", None);
        assert!(launch_spec(&paths, "op", &[], Some(home().as_path())).is_err());
    }

    #[test]
    fn agy_relocates_home() {
        let (paths, _g) = fixture();
        add(&paths, "ag", Harness::Agy, "/d/agy-home", None);
        let spec = launch_spec(&paths, "ag", &[], Some(home().as_path())).unwrap();
        assert_eq!(
            spec.env_set,
            vec![("HOME".to_string(), "/d/agy-home".to_string())]
        );
    }

    #[test]
    fn primary_agy_keeps_real_home() {
        let (paths, _g) = fixture();
        add(&paths, "antigravity", Harness::Agy, "~/.gemini", None);
        let spec = launch_spec(&paths, "antigravity", &[], Some(home().as_path())).unwrap();
        assert!(spec.env_set.is_empty());
    }

    #[test]
    fn unknown_id_names_known_instances() {
        let (paths, _g) = fixture();
        add(&paths, "cx", Harness::Codex, "/x", None);
        let err = launch_spec(&paths, "ghost", &[], Some(home().as_path())).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"ghost\""), "{msg}");
        assert!(msg.contains("cx"), "{msg}");
    }

    #[test]
    fn malicious_binary_stays_a_single_value() {
        let (paths, _g) = fixture();
        add(
            &paths,
            "cx",
            Harness::Codex,
            "/x",
            Some("/tmp/evil dir/'; rm -rf ~"),
        );
        let spec = launch_spec(&paths, "cx", &[], Some(home().as_path())).unwrap();
        assert_eq!(spec.program, "/tmp/evil dir/'; rm -rf ~");
    }

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn terminal_prefers_standard_helper_then_cosmic() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-launch-term-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        fake_bin(&dir, "cosmic-term");
        let exe = PathBuf::from("/opt/agentdeck/agentdeck");
        let paths = Paths::new(dir.join("config with spaces"), dir.join("state"));

        let (term, prefix) =
            find_terminal(dir.as_os_str()).expect("cosmic-term fallback must be found");
        assert_eq!(term, dir.join("cosmic-term"));
        assert_eq!(
            terminal_command(&term, prefix, &exe, "cx", &paths),
            vec![
                dir.join("cosmic-term").into_os_string(),
                OsString::from("-e"),
                OsString::from("/usr/bin/env"),
                OsString::from(format!(
                    "AGENTDECK_CONFIG_DIR={}",
                    paths.config_dir.display()
                )),
                OsString::from(format!("AGENTDECK_STATE_DIR={}", paths.state_dir.display())),
                OsString::from("/opt/agentdeck/agentdeck"),
                OsString::from("instance"),
                OsString::from("run"),
                OsString::from("cx"),
            ]
        );

        fake_bin(&dir, "xdg-terminal-exec");
        let (term, prefix) = find_terminal(dir.as_os_str()).unwrap();
        assert_eq!(term, dir.join("xdg-terminal-exec"));
        assert!(prefix.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn terminal_missing_is_actionable() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-launch-empty-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(find_terminal(dir.as_os_str()).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
