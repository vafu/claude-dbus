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

pub(crate) fn codex_log_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".codex/log/codex-tui.log"))
}

pub(crate) fn codex_session_index_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".codex/session_index.jsonl"))
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

    let home = std::env::var_os("HOME")?;
    let sessions_dir = Path::new(&home).join(".codex/sessions");
    let mut matches = Vec::new();
    collect_matching_codex_sessions(&sessions_dir, session_id, &mut matches);
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
