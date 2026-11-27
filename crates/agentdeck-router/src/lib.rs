//! Rust router for model delegation (`agentdeck route`, `agentdeck route
//! hook`, `agentdeck route workflow`, `agentdeck route claude-shell`,
//! `agentdeck route t3-mcp-call`). It reads the generated `router.json`,
//! talks to Jev the same way, and prints byte-compatible JSON.

pub mod admission;
pub mod hook;
pub mod jev_queue;
pub mod load;
pub mod mcp_call;
pub mod outcomes;
pub mod policy;
pub mod reasoning;
pub mod route;
pub mod shell;
pub mod workflow;
pub mod workflow_eval;

use serde_json::Value;

/// A JSON number formatted the way `JSON.stringify` formats it: integers
/// without a decimal point, other finite doubles in shortest round-trip form.
/// Non-finite values cannot exist in JSON and become null, like `stringify`.
pub fn js_num(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9e15 {
        Value::from(v as i64)
    } else if let Some(n) = serde_json::Number::from_f64(v) {
        Value::Number(n)
    } else {
        Value::Null
    }
}

/// `+v.toFixed(3)`: round half away from zero at three decimals.
pub fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// `+v.toFixed(2)`.
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub fn home_dir() -> anyhow::Result<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

pub fn runtime_dir() -> std::path::PathBuf {
    agentdeck_core::judge::default_runtime_base()
}

pub fn state_home() -> anyhow::Result<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(std::path::PathBuf::from(d));
    }
    Ok(home_dir()?.join(".local/state"))
}

/// Parse an ISO-8601 timestamp (`Date.parse` subset as written by Codex logs)
/// to milliseconds since the epoch; `None` when unparseable.
pub fn parse_time_ms(s: &str) -> Option<f64> {
    let (s, offset_min): (&str, f64) =
        if let Some(t) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
            (t, 0.0)
        } else if s.len() > 6 {
            let (head, tail) = s.split_at(s.len() - 6);
            let sign = match tail.as_bytes()[0] {
                b'+' => 1.0,
                b'-' => -1.0,
                _ => return None,
            };
            if tail.as_bytes()[3] != b':' {
                return None;
            }
            let h: f64 = tail[1..3].parse().ok()?;
            let m: f64 = tail[4..6].parse().ok()?;
            (head, sign * (h * 60.0 + m))
        } else {
            return None;
        };
    let (date, time) = s.split_once(['T', 't'])?;
    let mut d = date.split('-');
    let (y, mo, da): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    if d.next().is_some() {
        return None;
    }
    let (time, frac) = match time.split_once('.') {
        Some((t, f)) => (t, f),
        None => (time, "0"),
    };
    let frac_ms: f64 = format!("0.{frac}").parse().ok()?;
    if !(0.0..1.0).contains(&frac_ms) && frac != "0" {
        return None;
    }
    let mut t = time.split(':');
    let (h, mi, se): (f64, f64, f64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if t.next().is_some() {
        return None;
    }
    let days = days_from_civil(y, mo, da)?;
    Some(
        ((days as f64) * 86400.0 + h * 3600.0 + mi * 60.0 + se + frac_ms - offset_min * 60.0)
            * 1000.0,
    )
}

// Howard Hinnant's days-from-civil; `None` on out-of-range input.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

#[derive(Debug)]
pub(crate) enum ChildRunError {
    TimedOut,
    OutputTooLarge,
    Io(std::io::Error),
}

/// Spawn `cmd` with `input` on stdin, concurrently draining stdout and stderr
/// up to `max_bytes` so pipes cannot deadlock when output or input exceeds the
/// OS pipe buffer (which Linux shrinks to 4–8 KiB under pipe-user-pages-soft).
pub(crate) fn run_piped_with_timeout(
    cmd: &mut std::process::Command,
    input: &[u8],
    timeout: std::time::Duration,
    max_bytes: usize,
) -> Result<std::process::Output, ChildRunError> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ChildRunError::Io)?;

    let overflow = Arc::new(AtomicBool::new(false));
    let cap = (max_bytes as u64).saturating_add(1);
    let spawn_reader = |mut pipe: Box<dyn Read + Send>, overflow: Arc<AtomicBool>| {
        std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let mut buf = Vec::new();
            pipe.by_ref().take(cap).read_to_end(&mut buf)?;
            if buf.len() > max_bytes {
                overflow.store(true, Ordering::Relaxed);
            }
            Ok(buf)
        })
    };

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ChildRunError::Io(std::io::Error::other("no stdout")))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ChildRunError::Io(std::io::Error::other("no stderr")))?;
    let mut stdin_pipe = child
        .stdin
        .take()
        .ok_or_else(|| ChildRunError::Io(std::io::Error::other("no stdin")))?;

    let stdout_thread = spawn_reader(Box::new(stdout_pipe), Arc::clone(&overflow));
    let stderr_thread = spawn_reader(Box::new(stderr_pipe), Arc::clone(&overflow));
    let input_bytes = input.to_vec();
    let stdin_thread = std::thread::spawn(move || {
        let _ = stdin_pipe.write_all(&input_bytes);
    });

    let start = std::time::Instant::now();
    let status = loop {
        if overflow.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ChildRunError::OutputTooLarge);
        }
        match child.try_wait().map_err(ChildRunError::Io)? {
            Some(status) => break status,
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ChildRunError::TimedOut);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };

    let _ = stdin_thread.join();
    let stdout = stdout_thread
        .join()
        .map_err(|_| ChildRunError::Io(std::io::Error::other("stdout reader panicked")))?
        .map_err(ChildRunError::Io)?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| ChildRunError::Io(std::io::Error::other("stderr reader panicked")))?
        .map_err(ChildRunError::Io)?;

    if overflow.load(Ordering::Relaxed) || stdout.len().saturating_add(stderr.len()) > max_bytes {
        return Err(ChildRunError::OutputTooLarge);
    }

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod child_run_tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn drains_large_stdout_and_stderr_without_deadlock() {
        // 128 KiB on stdout + 128 KiB on stderr exceeds Linux's 64 KiB default
        // pipe capacity and its 4–8 KiB shrunk capacity under pipe pressure.
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "s=0123456789abcdef; i=0; while [ $i -lt 13 ]; do s=$s$s; i=$((i+1)); done; printf '%s' \"$s\"; printf '%s' \"$s\" >&2",
        ]);
        let out = run_piped_with_timeout(&mut cmd, b"", Duration::from_secs(5), 1024 * 1024)
            .expect("must drain concurrent large stdout and stderr");
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 131_072);
        assert_eq!(out.stderr.len(), 131_072);
    }

    #[test]
    fn drains_large_stdin_echoed_to_stdout_without_deadlock() {
        let payload = vec![b'x'; 262_144];
        let mut cmd = Command::new("cat");
        let out = run_piped_with_timeout(&mut cmd, &payload, Duration::from_secs(5), 1024 * 1024)
            .expect("must write >64KiB stdin while draining stdout");
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), payload.len());
    }

    #[test]
    fn enforces_timeout_and_output_cap() {
        let start = Instant::now();
        let mut slow = Command::new("sleep");
        slow.arg("10");
        assert!(matches!(
            run_piped_with_timeout(&mut slow, b"", Duration::from_millis(150), 1024),
            Err(ChildRunError::TimedOut)
        ));
        assert!(start.elapsed() < Duration::from_secs(2));

        let mut noisy = Command::new("sh");
        noisy.args([
            "-c",
            "s=0123456789abcdef; i=0; while [ $i -lt 10 ]; do s=$s$s; i=$((i+1)); done; printf '%s' \"$s\"",
        ]);
        assert!(matches!(
            run_piped_with_timeout(&mut noisy, b"", Duration::from_secs(5), 4096),
            Err(ChildRunError::OutputTooLarge)
        ));
    }
}
