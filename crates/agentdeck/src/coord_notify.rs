//! Telegram progress messages for coordinator checklists (`agentdeck daemon`,
//! coordinator plugin on). Once a minute it compares every thread's checklist
//! statuses with what it last sent and posts one message per changed thread,
//! so a burst of updates collapses into one message. The last sent statuses
//! live in the state dir, so a daemon restart does not resend.
//! Config: `<config>/telegram.env` (0600) with `TELEGRAM_BOT_TOKEN` and
//! `TELEGRAM_CHAT_ID`; without it nothing is sent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentdeck_core::thread_state::{ThreadState, list};
use agentdeck_core::{Paths, Registry, plugins};

pub const INTERVAL: Duration = Duration::from_secs(60);

/// thread id -> item id -> status, as last sent.
type Sent = BTreeMap<String, BTreeMap<String, String>>;

fn statuses(s: &ThreadState) -> BTreeMap<String, String> {
    s.checklist
        .iter()
        .map(|i| (i.id.clone(), i.status.clone()))
        .collect()
}

fn titles<'a>(s: &'a ThreadState, status: &str) -> Vec<&'a str> {
    s.checklist
        .iter()
        .filter(|i| i.status == status)
        .map(|i| i.title.as_str())
        .collect()
}

/// One message for a thread whose statuses changed: goal, progress, what runs,
/// what is blocked, and what just finished.
pub fn message(s: &ThreadState, before: Option<&BTreeMap<String, String>>) -> String {
    let goal: String = s
        .state
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(160)
        .collect();
    let (done, total) = s.progress();
    let mut out = format!("{goal}\n{done}/{total}");
    let running = titles(s, "running");
    if !running.is_empty() {
        out.push_str(&format!("\nИдёт: {}", running.join("; ")));
    }
    let blocked = titles(s, "blocked");
    if !blocked.is_empty() {
        out.push_str(&format!("\nБлок: {}", blocked.join("; ")));
    }
    let finished: Vec<&str> = s
        .checklist
        .iter()
        .filter(|i| {
            i.status == "done"
                && before
                    .and_then(|b| b.get(&i.id))
                    .is_some_and(|st| st != "done")
        })
        .map(|i| i.title.as_str())
        .collect();
    if !finished.is_empty() {
        out.push_str(&format!("\nГотово: {}", finished.join("; ")));
    }
    out
}

/// Messages to send now and the new sent-map. A thread seen for the first time
/// is recorded without a message (no flood when the feature turns on).
pub fn diff(
    sent: &Sent,
    threads: &[(String, ThreadState)],
    first_run: bool,
) -> (Vec<String>, Sent) {
    let mut next = Sent::new();
    let mut out = Vec::new();
    for (id, s) in threads {
        if s.checklist.is_empty() {
            continue;
        }
        let now = statuses(s);
        match sent.get(id) {
            Some(before) if *before != now => out.push(message(s, Some(before))),
            None if !first_run => out.push(message(s, None)),
            _ => {}
        }
        next.insert(id.clone(), now);
    }
    (out, next)
}

fn sent_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("coordinator/telegram-sent.json")
}

fn read_env(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let get = |k: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(key, _)| key.trim() == k)
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    };
    Some((get("TELEGRAM_BOT_TOKEN")?, get("TELEGRAM_CHAT_ID")?))
}

async fn send(client: &reqwest::Client, token: &str, chat: &str, text: &str) -> Result<(), String> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let resp = client
        .post(url)
        .json(
            &serde_json::json!({ "chat_id": chat, "text": text, "disable_web_page_preview": true }),
        )
        .send()
        .await
        // reqwest errors print the URL, which carries the bot token.
        .map_err(|e| {
            format!(
                "telegram unreachable ({})",
                if e.is_timeout() { "timeout" } else { "connect" }
            )
        })?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("telegram answered HTTP {}", resp.status().as_u16()))
    }
}

pub async fn run(paths: Paths) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => return eprintln!("coordinator notify: no HTTP client: {e}"),
    };
    let file = sent_file(&paths);
    let mut sent: Option<Sent> = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    loop {
        tokio::time::sleep(INTERVAL).await;
        let on = Registry::load(&paths)
            .is_ok_and(|r| plugins::plugin_enabled(&r, plugins::PLUGIN_COORDINATOR));
        let Some((token, chat)) = read_env(&paths.config_dir.join("telegram.env")).filter(|_| on)
        else {
            continue;
        };
        let first_run = sent.is_none();
        let (messages, next) = diff(
            sent.as_ref().unwrap_or(&Sent::new()),
            &list(&paths),
            first_run,
        );
        let mut ok = true;
        for m in &messages {
            if let Err(e) = send(&client, &token, &chat, m).await {
                eprintln!("coordinator notify: {e}");
                ok = false;
                break;
            }
        }
        // Keep the old map after a failed send so the change goes out next minute.
        if ok {
            if let Ok(b) = serde_json::to_vec(&next) {
                let _ = std::fs::create_dir_all(file.parent().expect("coordinator dir"));
                let _ = std::fs::write(&file, b);
            }
            sent = Some(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentdeck_core::thread_state::Item;

    fn state(items: &[(&str, &str)]) -> ThreadState {
        ThreadState {
            state: "Goal: ship #71\nmore".into(),
            checklist: items
                .iter()
                .map(|(id, st)| Item {
                    id: id.to_string(),
                    title: format!("Item {id}"),
                    status: st.to_string(),
                    ..Item::default()
                })
                .collect(),
            ..ThreadState::default()
        }
    }

    #[test]
    fn first_run_records_then_changes_send_once() {
        let t = |items: &[(&str, &str)]| vec![("t1".to_string(), state(items))];
        let (m, sent) = diff(&Sent::new(), &t(&[("a", "running"), ("b", "todo")]), true);
        assert!(m.is_empty());
        let (m, sent) = diff(&sent, &t(&[("a", "running"), ("b", "todo")]), false);
        assert!(m.is_empty());
        let (m, _) = diff(&sent, &t(&[("a", "done"), ("b", "blocked")]), false);
        assert_eq!(
            m,
            vec!["Goal: ship #71\n1/2\nБлок: Item b\nГотово: Item a".to_string()]
        );
    }

    #[test]
    fn a_new_thread_after_the_first_run_is_announced() {
        let (m, _) = diff(
            &Sent::new(),
            &[("t2".into(), state(&[("a", "running")]))],
            false,
        );
        assert_eq!(m, vec!["Goal: ship #71\n0/1\nИдёт: Item a".to_string()]);
    }

    #[test]
    fn env_needs_both_keys() {
        let dir = std::env::temp_dir().join(format!("agentdeck-tg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("telegram.env");
        std::fs::write(&f, "TELEGRAM_BOT_TOKEN=1:abc\n").unwrap();
        assert!(read_env(&f).is_none());
        std::fs::write(&f, "TELEGRAM_BOT_TOKEN=1:abc\nTELEGRAM_CHAT_ID=42\n").unwrap();
        assert_eq!(read_env(&f), Some(("1:abc".into(), "42".into())));
        std::fs::remove_dir_all(&dir).ok();
    }
}
