use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};

const CODEX_SESSION_FILE_CACHE_MAX: usize = 256;

static CODEX_SESSION_FILE_CACHE: OnceLock<StdMutex<BoundedCache<PathBuf>>> = OnceLock::new();

struct BoundedCache<T> {
    entries: HashMap<String, T>,
    order: VecDeque<String>,
    max_entries: usize,
}

impl<T: Clone> BoundedCache<T> {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max_entries,
        }
    }

    fn get(&mut self, key: &str) -> Option<T> {
        let value = self.entries.get(key).cloned()?;
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: String, value: T) {
        self.entries.insert(key.clone(), value);
        self.touch(&key);
        while self.entries.len() > self.max_entries {
            if let Some(expired) = self.order.pop_front() {
                self.entries.remove(&expired);
            }
        }
    }

    fn remove(&mut self, key: &str) {
        self.entries.remove(key);
        self.order.retain(|entry| entry != key);
    }

    fn touch(&mut self, key: &str) {
        self.order.retain(|entry| entry != key);
        self.order.push_back(key.to_string());
    }
}

/// Codex homes this service should read from.
///
/// A single user routinely runs several Codex profiles side by side, each with
/// its own `CODEX_HOME` (`~/.codex`, `~/.codex-personal`, `~/.codex-*`). The
/// service has no way to know which one a hook came from, so it must look at
/// all of them. `$CODEX_HOME` of the service process (if any) is consulted
/// first, then the default `~/.codex`, then any sibling `~/.codex-*` profile
/// directories that look like a Codex home.
pub(crate) fn codex_homes() -> Vec<PathBuf> {
    discover_codex_homes(
        std::env::var_os("CODEX_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn discover_codex_homes(env_home: Option<PathBuf>, user_home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let mut push = |home: PathBuf| {
        if !homes.contains(&home) {
            homes.push(home);
        }
    };

    if let Some(home) = env_home {
        push(home);
    }
    let Some(user_home) = user_home else {
        return homes;
    };
    push(user_home.join(".codex"));

    let Ok(entries) = std::fs::read_dir(&user_home) else {
        return homes;
    };
    let mut profiles: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".codex-"))
        })
        .filter(|path| path.join("sessions").is_dir() || path.join("config.toml").is_file())
        .collect();
    profiles.sort();
    for profile in profiles {
        push(profile);
    }

    homes
}

pub(crate) fn codex_log_files() -> Vec<PathBuf> {
    codex_homes()
        .into_iter()
        .map(|home| home.join("log/codex-tui.log"))
        .filter(|path| path.is_file())
        .collect()
}

pub(crate) fn codex_session_index_files() -> Vec<PathBuf> {
    codex_homes()
        .into_iter()
        .map(|home| home.join("session_index.jsonl"))
        .filter(|path| path.is_file())
        .collect()
}

pub(crate) fn codex_session_file(session_id: &str) -> Option<PathBuf> {
    let cache = CODEX_SESSION_FILE_CACHE
        .get_or_init(|| StdMutex::new(BoundedCache::new(CODEX_SESSION_FILE_CACHE_MAX)));
    if let Ok(mut cache) = cache.lock()
        && let Some(path) = cache.get(session_id)
    {
        if path.exists() {
            return Some(path);
        }
        cache.remove(session_id);
    }

    let mut matches = Vec::new();
    for home in codex_homes() {
        collect_matching_codex_sessions(&home.join("sessions"), session_id, &mut matches);
    }
    let path = matches.into_iter().max_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    })?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(session_id.to_string(), path.clone());
    }
    Some(path)
}

pub(crate) fn read_file_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    if start == 0 {
        return Some(contents);
    }

    let line_start = contents.find('\n').map(|index| index + 1)?;
    Some(contents[line_start..].to_string())
}

fn collect_matching_codex_sessions(dir: &Path, session_id: &str, matches: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            collect_matching_codex_sessions(&path, session_id, matches);
            continue;
        }

        if file_type.is_file()
            && path.extension().is_some_and(|ext| ext == "jsonl")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(session_id))
        {
            matches.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "agent-dbus-artifacts-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discovers_default_and_profile_homes() {
        let home = TempHome::new("homes");
        std::fs::create_dir_all(home.0.join(".codex/sessions")).unwrap();
        std::fs::create_dir_all(home.0.join(".codex-personal/sessions")).unwrap();
        std::fs::create_dir_all(home.0.join(".codex-databricks")).unwrap();
        std::fs::write(home.0.join(".codex-databricks/config.toml"), "").unwrap();
        // A plain dotfile/dir that is not a Codex home must be ignored.
        std::fs::create_dir_all(home.0.join(".codex-random")).unwrap();

        let homes = discover_codex_homes(None, Some(home.0.clone()));

        assert_eq!(
            homes,
            vec![
                home.0.join(".codex"),
                home.0.join(".codex-databricks"),
                home.0.join(".codex-personal"),
            ]
        );
    }

    #[test]
    fn env_home_is_preferred_and_deduplicated() {
        let home = TempHome::new("env");
        std::fs::create_dir_all(home.0.join(".codex/sessions")).unwrap();

        let homes = discover_codex_homes(Some(home.0.join(".codex")), Some(home.0.clone()));

        assert_eq!(homes, vec![home.0.join(".codex")]);
    }

    #[test]
    fn collects_session_files_across_homes() {
        let home = TempHome::new("session");
        let id = "01a1c739-a0fb-7d92-b797-19578d56a08b";
        let sessions = home.0.join(".codex-personal/sessions/2026/11/10");
        std::fs::create_dir_all(&sessions).unwrap();
        let file = sessions.join(format!("rollout-2026-11-10T12-50-05-{id}.jsonl"));
        std::fs::write(&file, "{}").unwrap();

        let mut matches = Vec::new();
        for codex_home in discover_codex_homes(None, Some(home.0.clone())) {
            collect_matching_codex_sessions(&codex_home.join("sessions"), id, &mut matches);
        }

        assert_eq!(matches, vec![file]);
    }
}
