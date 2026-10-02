//! Event-driven rollout telemetry. Hooks own lifecycle; this never creates sessions.
use super::artifacts::{codex_home, codex_session_file};
use crate::dbus::SessionObject;
use agent_dbus_core::{
    path::session_path,
    telemetry::{COUNTERS, TokenUsage, UsageReport, reasoning_effort},
};
use notify::{EventKind, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::OnceLock,
};
use tokio::sync::Mutex;

#[derive(Default)]
struct Parser {
    model: String,
    effort: String,
    turn: String,
    totals: TokenUsage,
    last: TokenUsage,
    epoch: u64,
    revision: u64,
}

fn counters(value: &serde_json::Value) -> TokenUsage {
    COUNTERS
        .into_iter()
        .filter_map(|(key, native)| value[native].as_u64().map(|v| (key.to_owned(), v)))
        .collect()
}

impl Parser {
    fn observe(&mut self, entry: &serde_json::Value, baseline: bool) -> Option<UsageReport> {
        let p = &entry["payload"];
        if entry["type"] == "turn_context" {
            if let Some(model) = p["model"].as_str() {
                self.model = model.to_owned();
            }
            if let Some(turn) = p["turn_id"].as_str() {
                self.turn = turn.to_owned();
            }
            if p.get("effort").is_some() {
                self.effort = reasoning_effort(p["effort"].as_str());
            } else if let Some(settings) = p.pointer("/collaboration_mode/settings") {
                if settings.get("reasoning_effort").is_some() {
                    self.effort = reasoning_effort(settings["reasoning_effort"].as_str());
                }
            }
            return None;
        }
        if entry["type"] != "event_msg" || p["type"] != "token_count" {
            return None;
        }
        let next = counters(&p["info"]["total_token_usage"]);
        // Reject context-full sentinels, malformed/negative totals and partial info.
        let (Some(input), Some(output), Some(total)) =
            (next.get("input"), next.get("output"), next.get("total"))
        else {
            return None;
        };
        if input.checked_add(*output) != Some(*total) {
            return None;
        }
        if next == self.totals {
            return None;
        }
        let reset = next
            .iter()
            .any(|(k, v)| self.totals.get(k).is_some_and(|old| v < old));
        let mut delta = TokenUsage::new();
        if reset {
            self.epoch += 1;
        }
        if !baseline && !reset {
            for (key, value) in &next {
                // A newly appearing optional breakdown cannot be safely backfilled.
                if let Some(old) = self
                    .totals
                    .get(key)
                    .copied()
                    .or_else(|| self.totals.is_empty().then_some(0))
                {
                    delta.insert(key.clone(), value - old);
                }
            }
        }
        self.totals = next;
        self.last = counters(&p["info"]["last_token_usage"]);
        self.revision += 1;
        (!baseline).then(|| UsageReport {
            epoch: self.epoch,
            revision: self.revision,
            timestamp: entry["timestamp"].as_str().unwrap_or("").to_owned(),
            turn_id: self.turn.clone(),
            model: if self.model.is_empty() {
                "unknown".to_owned()
            } else {
                self.model.clone()
            },
            reasoning_effort: if self.effort.is_empty() {
                "unknown".to_owned()
            } else {
                self.effort.clone()
            },
            delta,
            totals: self.totals.clone(),
        })
    }
    fn apply(&self, session: &mut SessionObject) {
        if !self.model.is_empty() {
            session.model_name = self.model.clone();
        }
        if !self.effort.is_empty() {
            session.reasoning_effort = self.effort.clone();
        }
        session.token_usage = self.totals.clone();
        session.last_token_usage = self.last.clone();
        session.usage_epoch = self.epoch;
        session.usage_revision = self.revision;
    }
}

#[derive(Default)]
struct Reader {
    path: Option<PathBuf>,
    inode: u64,
    offset: u64,
    fragment: Vec<u8>,
    parser: Parser,
}
impl Reader {
    fn read(&mut self, id: &str) -> Vec<UsageReport> {
        let Some(path) = codex_session_file(id) else {
            return vec![];
        };
        self.read_path(path)
    }

    fn read_path(&mut self, path: PathBuf) -> Vec<UsageReport> {
        let Ok(mut file) = std::fs::File::open(&path) else {
            return vec![];
        };
        let Ok(meta) = file.metadata() else {
            return vec![];
        };
        let baseline = self.path.is_none()
            || self.path.as_ref() != Some(&path)
            || self.inode != meta.ino()
            || meta.len() < self.offset;
        if baseline {
            if self.path.is_some() {
                self.parser.epoch += 1;
                self.parser.totals.clear();
                self.parser.last.clear();
            }
            self.path = Some(path);
            self.inode = meta.ino();
            self.offset = 0;
            self.fragment.clear();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return vec![];
        }
        let mut chunk = Vec::new();
        if file.read_to_end(&mut chunk).is_err() {
            return vec![];
        }
        self.offset += chunk.len() as u64;
        self.fragment.extend(chunk);
        let mut reports = Vec::new();
        let mut consumed = 0;
        for (index, byte) in self.fragment.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if let Ok(entry) = serde_json::from_slice(&self.fragment[consumed..index]) {
                if let Some(report) = self.parser.observe(&entry, baseline) {
                    reports.push(report);
                }
            }
            consumed = index + 1;
        }
        self.fragment.drain(..consumed);
        reports
    }
}

fn readers() -> &'static Mutex<HashMap<String, Reader>> {
    static READERS: OnceLock<Mutex<HashMap<String, Reader>>> = OnceLock::new();
    READERS.get_or_init(Default::default)
}

async fn emit(conn: &zbus::Connection, id: &str, reports: Vec<UsageReport>) -> zbus::Result<()> {
    let path = session_path("codex", id);
    if let Ok(iface) = conn
        .object_server()
        .interface::<_, SessionObject>(&path)
        .await
    {
        for report in reports {
            SessionObject::token_usage_reported(iface.signal_emitter(), &report).await?;
        }
    }
    Ok(())
}

pub(crate) async fn update_hook(
    conn: &zbus::Connection,
    id: &str,
    f: impl FnOnce(&mut SessionObject),
) -> zbus::Result<()> {
    // Serialize reading, registration and publishing, including bootstrap.
    let mut entries = readers().lock().await;
    let reader = entries.entry(id.to_owned()).or_default();
    let reports = reader.read(id);
    crate::dbus::update_session(conn, "codex", id, |s| {
        let previous_model = s.model_name.clone();
        f(s);
        if s.model_name == "unknown" && !previous_model.is_empty() {
            s.model_name = previous_model;
        }
        reader.parser.apply(s);
    })
    .await?;
    emit(conn, id, reports).await
}

pub(crate) async fn drain(conn: &zbus::Connection, id: &str, remove: bool) -> zbus::Result<()> {
    let mut entries = readers().lock().await;
    if let Some(reader) = entries.get_mut(id) {
        let reports = reader.read(id);
        crate::dbus::update_existing_session(conn, "codex", id, |s| reader.parser.apply(s)).await?;
        emit(conn, id, reports).await?;
    }
    if remove {
        entries.remove(id);
    }
    Ok(())
}

pub(crate) fn start(conn: zbus::Connection) {
    let Some(home) = codex_home() else {
        return;
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                let _ = tx.send(());
            }
            Err(err) => {
                tracing::warn!(%err, "Codex telemetry watch error; reconciling tracked files");
                let _ = tx.send(());
            }
            _ => {}
        });
    let Ok(mut watcher) = watcher else {
        tracing::warn!("cannot start Codex telemetry watcher");
        return;
    };
    if let Err(err) = watcher.watch(&home, RecursiveMode::Recursive) {
        tracing::warn!(%err, "cannot watch Codex home");
        return;
    }
    tokio::spawn(async move {
        let _watcher = watcher;
        while rx.recv().await.is_some() {
            // Notification bursts are hints to reconcile bytes, not usage events.
            while rx.try_recv().is_ok() {}
            let ids: Vec<_> = readers().lock().await.keys().cloned().collect();
            for id in ids {
                if let Err(err) = drain(&conn, &id, false).await {
                    tracing::warn!(%err, %id, "Codex telemetry update failed");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn usage(input: u64, output: u64) -> serde_json::Value {
        json!({"type":"event_msg", "timestamp":"now", "payload":{"type":"token_count", "info":{
            "total_token_usage":{"input_tokens":input,"output_tokens":output,"total_tokens":input+output,"reasoning_output_tokens":output/2},
            "last_token_usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}})
    }
    #[test]
    fn baseline_duplicates_deltas_and_reset() {
        let mut p = Parser::default();
        assert!(p.observe(&usage(100, 20), true).is_none());
        assert!(p.observe(&usage(100, 20), false).is_none());
        let r = p.observe(&usage(110, 22), false).unwrap();
        assert_eq!(r.delta["input"], 10);
        assert_eq!(r.delta["reasoning_output"], 1);
        assert!(!r.delta.contains_key("cache_read_input"));
        let reset = p.observe(&usage(10, 2), false).unwrap();
        assert_eq!(reset.epoch, 1);
        assert!(reset.delta.is_empty());
    }
    #[test]
    fn metadata_tracks_the_request_and_explicit_unset() {
        let mut p = Parser::default();
        p.observe(&json!({"type":"turn_context","payload":{"model":"a","effort":"high","turn_id":"turn"}}),false);
        let a = p.observe(&usage(10, 2), false).unwrap();
        assert_eq!(a.model, "a");
        assert_eq!(a.reasoning_effort, "high");
        p.observe(
            &json!({"type":"turn_context","payload":{"model":"b","effort":null}}),
            false,
        );
        let b = p.observe(&usage(20, 4), false).unwrap();
        assert_eq!(b.model, "b");
        assert_eq!(b.reasoning_effort, "unknown");
    }
    #[test]
    fn rate_only_and_context_sentinels_do_not_fabricate_usage() {
        let mut p = Parser::default();
        p.observe(&usage(100, 20), true);
        let mut sentinel = usage(0, 0);
        sentinel["payload"]["info"]["total_token_usage"]["total_tokens"] = json!(258400);
        assert!(p.observe(&sentinel, false).is_none());
        assert!(
            p.observe(
                &json!({"type":"event_msg","payload":{"type":"token_count","info":null}}),
                false
            )
            .is_none()
        );
        assert_eq!(p.totals["input"], 100);
    }

    #[test]
    fn reader_handles_partial_lines_and_truncation_without_replay() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("agent-dbus-telemetry-{}.jsonl", std::process::id()));
        std::fs::write(&path, format!("{}\n", usage(100, 20))).unwrap();
        let mut reader = Reader::default();
        assert!(reader.read_path(path.clone()).is_empty());
        let line = format!("{}\n", usage(110, 22));
        let split = line.len() / 2;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&line.as_bytes()[..split]).unwrap();
        assert!(reader.read_path(path.clone()).is_empty());
        file.write_all(&line.as_bytes()[split..]).unwrap();
        let events = reader.read_path(path.clone());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].delta["input"], 10);
        assert!(reader.read_path(path.clone()).is_empty());
        std::fs::write(&path, format!("{}\n", usage(5, 1))).unwrap();
        assert!(reader.read_path(path.clone()).is_empty());
        assert_eq!(reader.parser.epoch, 1);
        assert_eq!(reader.parser.totals["input"], 5);
        std::fs::remove_file(path).unwrap();
    }
}
