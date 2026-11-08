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
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
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
