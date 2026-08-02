//! JSONL file-backed session store.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::harness::session::fork::{entries_for_fork_selection, SessionForkSelection};
use crate::harness::session::keyed_operation_queue::KeyedOperationQueue;
use crate::harness::session::types::{
    create_session_id, PendingSessionWrite, SessionMetadata, SessionReader, SessionStore,
    SessionTreeEntry,
};
use crate::harness::types::{ExecutionEnv, SessionError};
use loop_ai::now_ms;

const DEFAULT_MAX_CONCURRENT_OPERATIONS: usize = 4;

/// JSONL session file header (version 3).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionHeader {
    #[serde(rename = "type")]
    kind: String,
    version: u32,
    id: String,
    timestamp: String,
    cwd: String,
    #[serde(rename = "parentSession", skip_serializing_if = "Option::is_none")]
    parent_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Map<String, Value>>,
}

struct SessionDocument {
    meta: SessionMetadata,
    entries: Vec<SessionTreeEntry>,
}

#[derive(Clone)]
struct CachedSession {
    meta: SessionMetadata,
    entries: Vec<SessionTreeEntry>,
    entry_ids: HashSet<String>,
    leaf_id: Option<String>,
    operation_key: String,
}

struct JsonlSessionStoreInner {
    fs: Arc<dyn ExecutionEnv>,
    sessions_root: PathBuf,
    sessions_root_resolved: Mutex<Option<PathBuf>>,
    cache: Mutex<HashMap<String, CachedSession>>,
    id_to_path: Mutex<HashMap<String, String>>,
    operations: KeyedOperationQueue,
    disposed: AtomicBool,
}

struct JsonlSessionStore {
    inner: Arc<JsonlSessionStoreInner>,
}

struct JsonlReader {
    meta: SessionMetadata,
    inner: Arc<JsonlSessionStoreInner>,
}

/// Create a JSONL session store rooted at `sessions_root`.
pub fn create_jsonl_session_store(
    fs: Arc<dyn ExecutionEnv>,
    sessions_root: PathBuf,
) -> Arc<dyn SessionStore> {
    Arc::new(JsonlSessionStore {
        inner: Arc::new(JsonlSessionStoreInner {
            fs,
            sessions_root,
            sessions_root_resolved: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
            id_to_path: Mutex::new(HashMap::new()),
            operations: KeyedOperationQueue::new(DEFAULT_MAX_CONCURRENT_OPERATIONS),
            disposed: AtomicBool::new(false),
        }),
    })
}

impl JsonlSessionStoreInner {
    fn assert_open(&self) -> Result<(), SessionError> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(SessionError::Storage(
                "JSONL session store is disposed".into(),
            ));
        }
        Ok(())
    }

    async fn sessions_root(&self) -> Result<PathBuf, SessionError> {
        if let Some(root) = self.sessions_root_resolved.lock().clone() {
            return Ok(root);
        }
        let root = self
            .fs
            .absolute_path(&self.sessions_root)
            .map_err(|e| SessionError::Io(e.message))?;
        *self.sessions_root_resolved.lock() = Some(root.clone());
        Ok(root)
    }

    async fn resolve_path_for_id(&self, id: &str) -> Result<String, SessionError> {
        if let Some(path) = self.id_to_path.lock().get(id).cloned() {
            return Ok(path);
        }
        let root = self.sessions_root().await?;
        if !self
            .fs
            .exists(&root)
            .await
            .map_err(|e| SessionError::Io(e.message))?
        {
            return Err(SessionError::NotFound(id.into()));
        }
        let dirs = self
            .fs
            .list_dir(&root)
            .await
            .map_err(|e| SessionError::Io(e.message))?;
        for dir in dirs {
            if !dir.is_dir {
                continue;
            }
            let files = self
                .fs
                .list_dir(&dir.path)
                .await
                .map_err(|e| SessionError::Io(e.message))?;
            for file in files {
                if file.is_dir || !file.path.extension().is_some_and(|e| e == "jsonl") {
                    continue;
                }
                let path = file.path.to_string_lossy().into_owned();
                if let Ok(meta) = self.load_metadata_from_path(&path).await {
                    if meta.id == id {
                        self.id_to_path.lock().insert(id.to_string(), path.clone());
                        return Ok(path);
                    }
                }
            }
        }
        Err(SessionError::NotFound(id.into()))
    }

    async fn get_session_dir(&self, cwd: &str) -> Result<PathBuf, SessionError> {
        let root = self.sessions_root().await?;
        let encoded = encode_cwd(cwd);
        Ok(self.fs.join_path(&root, Path::new(&encoded)))
    }

    async fn list_session_dirs(&self) -> Result<Vec<PathBuf>, SessionError> {
        let root = self.sessions_root().await?;
        if !self
            .fs
            .exists(&root)
            .await
            .map_err(|e| SessionError::Io(e.message))?
        {
            return Ok(vec![]);
        }
        Ok(self
            .fs
            .list_dir(&root)
            .await
            .map_err(|e| SessionError::Io(e.message))?
            .into_iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.path)
            .collect())
    }

    async fn load_metadata_from_path(&self, path: &str) -> Result<SessionMetadata, SessionError> {
        let content = self
            .fs
            .read_text_file(Path::new(path))
            .await
            .map_err(|e| SessionError::Io(e.message))?;
        let header_line = content
            .lines()
            .find(|line| !line.trim().is_empty())
            .ok_or_else(|| invalid_session(path, "missing session header"))?;
        let header = parse_header(header_line, path)?;
        Ok(metadata_from_header(header, path))
    }

    async fn load_document(&self, path: &str) -> Result<SessionDocument, SessionError> {
        if let Some(cached) = self.cache.lock().get(path) {
            return Ok(SessionDocument {
                meta: cached.meta.clone(),
                entries: cached.entries.clone(),
            });
        }

        let content = self
            .fs
            .read_text_file(Path::new(path))
            .await
            .map_err(|e| SessionError::Io(e.message))?;
        let lines: Vec<&str> = content.lines().filter(|line| !line.trim().is_empty()).collect();
        if lines.is_empty() {
            return Err(invalid_session(path, "missing session header"));
        }
        let header = parse_header(lines[0], path)?;
        let mut entries = Vec::new();
        let mut entry_ids = HashSet::new();
        for (index, line) in lines.iter().skip(1).enumerate() {
            let entry = parse_entry(line, path, index + 2)?;
            if !entry_ids.insert(entry.id().to_string()) {
                return Err(invalid_session(
                    path,
                    &format!("duplicate entry id {}", entry.id()),
                ));
            }
            entries.push(entry);
        }
        let meta = metadata_from_header(header.clone(), path);
        let leaf_id = compute_leaf_id(&entries);
        let operation_key = document_operation_key(&header.cwd, &path_file_name(path));
        self.cache.lock().insert(
            path.to_string(),
            CachedSession {
                meta: meta.clone(),
                entries: entries.clone(),
                entry_ids,
                leaf_id,
                operation_key,
            },
        );
        self.id_to_path
            .lock()
            .insert(meta.id.clone(), path.to_string());
        Ok(SessionDocument { meta, entries })
    }

    async fn create_document(
        &self,
        id: String,
        timestamp_iso: String,
        file_name: String,
        operation_key: String,
        cwd: String,
        cwd_meta: Option<String>,
        name: Option<String>,
        parent_session_path: Option<String>,
        entries: Vec<SessionTreeEntry>,
    ) -> Result<SessionMetadata, SessionError> {
        let dir = self.get_session_dir(&cwd).await?;
        self.fs
            .create_dir(&dir)
            .await
            .map_err(|e| SessionError::Io(e.message))?;
        let path = self
            .fs
            .join_path(&dir, Path::new(&file_name))
            .to_string_lossy()
            .into_owned();
        if self
            .fs
            .exists(Path::new(&path))
            .await
            .map_err(|e| SessionError::Io(e.message))?
        {
            return Err(SessionError::Invalid(format!(
                "Session already exists: {path}"
            )));
        }

        let mut metadata_map = Map::new();
        if let Some(name) = &name {
            metadata_map.insert("name".into(), Value::String(name.clone()));
        }
        let header = SessionHeader {
            kind: "session".into(),
            version: 3,
            id: id.clone(),
            timestamp: timestamp_iso,
            cwd: cwd.clone(),
            parent_session: parent_session_path,
            metadata: if metadata_map.is_empty() {
                None
            } else {
                Some(metadata_map)
            },
        };
        let created_at = parse_header_timestamp(&header.timestamp)?;
        let mut content = serde_json::to_string(&header)
            .map_err(|e| SessionError::Storage(e.to_string()))?;
        for entry in &entries {
            content.push('\n');
            content.push_str(
                &serde_json::to_string(entry).map_err(|e| SessionError::Storage(e.to_string()))?,
            );
        }
        content.push('\n');
        self.fs
            .write_file(Path::new(&path), content.as_bytes())
            .await
            .map_err(|e| SessionError::Io(e.message))?;

        let entry_ids = entries.iter().map(|e| e.id().to_string()).collect();
        let leaf_id = compute_leaf_id(&entries);
        let meta = SessionMetadata {
            id: id.clone(),
            cwd: cwd_meta,
            name,
            parent_session_id: header.parent_session.clone(),
            created_at,
            path: Some(path.clone()),
        };
        self.cache.lock().insert(
            path.clone(),
            CachedSession {
                meta: meta.clone(),
                entries: entries.clone(),
                entry_ids,
                leaf_id,
                operation_key,
            },
        );
        self.id_to_path.lock().insert(id, path);
        Ok(meta)
    }

    fn operation_key_for_path(&self, path: &str) -> String {
        self.cache
            .lock()
            .get(path)
            .map(|c| c.operation_key.clone())
            .unwrap_or_else(|| format!("document:{path}"))
    }
}

impl JsonlSessionStore {
    fn reader_for(&self, meta: SessionMetadata) -> Arc<dyn SessionReader> {
        Arc::new(JsonlReader {
            meta,
            inner: Arc::clone(&self.inner),
        })
    }

    /// Drain pending operations (optional cleanup).
    /// Drain in-flight JSONL operations (call before dropping long-lived stores).
    #[allow(dead_code)]
    pub async fn dispose(&self) {
        self.inner.disposed.store(true, Ordering::SeqCst);
        self.inner.operations.drain().await;
    }
}

#[async_trait]
impl SessionReader for JsonlReader {
    fn metadata(&self) -> &SessionMetadata {
        &self.meta
    }

    async fn read_head(&self) -> Result<Option<String>, SessionError> {
        let path = self
            .meta
            .path
            .clone()
            .ok_or_else(|| SessionError::NotFound(self.meta.id.clone()))?;
        let key = self.inner.operation_key_for_path(&path);
        self.inner
            .operations
            .enqueue(key, || async {
                let cached = self.inner.cache.lock().get(&path).cloned();
                if let Some(c) = cached {
                    if let Some(leaf) = &c.leaf_id {
                        if !c.entries.iter().any(|e| e.id() == leaf) {
                            return Err(SessionError::Invalid(format!(
                                "Entry {leaf} not found"
                            )));
                        }
                    }
                    return Ok(c.leaf_id.clone());
                }
                Err(SessionError::NotFound(path.clone()))
            })
            .await
    }

    async fn read_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
        let path = self
            .meta
            .path
            .clone()
            .ok_or_else(|| SessionError::NotFound(self.meta.id.clone()))?;
        let key = self.inner.operation_key_for_path(&path);
        self.inner
            .operations
            .enqueue(key, || async {
                Ok(self
                    .inner
                    .cache
                    .lock()
                    .get(&path)
                    .and_then(|c| c.entries.iter().find(|e| e.id() == id).cloned()))
            })
            .await
    }

    async fn read_entries(
        &self,
        _after_seq: Option<u64>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let path = self
            .meta
            .path
            .clone()
            .ok_or_else(|| SessionError::NotFound(self.meta.id.clone()))?;
        let key = self.inner.operation_key_for_path(&path);
        self.inner
            .operations
            .enqueue(key, || async {
                Ok(self
                    .inner
                    .cache
                    .lock()
                    .get(&path)
                    .map(|c| c.entries.clone())
                    .unwrap_or_default())
            })
            .await
    }

    async fn read_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, SessionError> {
        let path = self
            .meta
            .path
            .clone()
            .ok_or_else(|| SessionError::NotFound(self.meta.id.clone()))?;
        let key = self.inner.operation_key_for_path(&path);
        let leaf_id = leaf_id.map(|s| s.to_string());
        self.inner
            .operations
            .enqueue(key, || async {
                let cached = self
                    .inner
                    .cache
                    .lock()
                    .get(&path)
                    .cloned()
                    .ok_or_else(|| SessionError::NotFound(path.clone()))?;
                let leaf = leaf_id.or(cached.leaf_id.clone());
                let Some(leaf) = leaf else {
                    return Ok(vec![]);
                };
                let by_id: HashMap<_, _> = cached
                    .entries
                    .iter()
                    .map(|e| (e.id().to_string(), e.clone()))
                    .collect();
                let mut path_entries = Vec::new();
                let mut cur = Some(leaf);
                while let Some(id) = cur {
                    let Some(entry) = by_id.get(&id) else {
                        break;
                    };
                    path_entries.push(entry.clone());
                    if let SessionTreeEntry::Compaction {
                        first_kept_entry_id,
                        ..
                    } = entry
                    {
                        // Include the retained tail (compaction parent back to
                        // the first kept entry) so recent context survives.
                        if let Some(first_kept) = first_kept_entry_id.clone() {
                            let mut kept_cur = entry.parent_id().map(|s| s.to_string());
                            while let Some(kid) = kept_cur {
                                let Some(kept_entry) = by_id.get(&kid) else {
                                    break;
                                };
                                path_entries.push(kept_entry.clone());
                                if kid == first_kept {
                                    break;
                                }
                                kept_cur = kept_entry.parent_id().map(|s| s.to_string());
                            }
                        }
                        break;
                    }
                    cur = entry.parent_id().map(|s| s.to_string());
                }
                path_entries.reverse();
                Ok(path_entries)
            })
            .await
    }
}

#[async_trait]
impl SessionStore for JsonlSessionStore {
    async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.assert_open()?;
        let (cwd_encoded, cwd_meta) = cwd_for_storage(cwd);
        let id = create_session_id();
        let timestamp_iso = iso_timestamp(now_ms());
        let file_name = document_file_name(&timestamp_iso, &id);
        let operation_key = document_operation_key(&cwd_encoded, &file_name);
        self.inner
            .operations
            .enqueue(operation_key.clone(), || async {
                let meta = self
                    .inner
                    .create_document(
                        id,
                        timestamp_iso,
                        file_name,
                        operation_key,
                        cwd_encoded,
                        cwd_meta,
                        name,
                        None,
                        vec![],
                    )
                    .await?;
                Ok(self.reader_for(meta))
            })
            .await
    }

    async fn load(&self, id: &str) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.assert_open()?;
        let path = self.inner.resolve_path_for_id(id).await?;
        let operation_key = self.inner.operation_key_for_path(&path);
        self.inner
            .operations
            .enqueue(operation_key, || async {
                if !self
                    .inner
                    .fs
                    .exists(Path::new(&path))
                    .await
                    .map_err(|e| SessionError::Io(e.message))?
                {
                    return Err(SessionError::NotFound(path.clone()));
                }
                let doc = self.inner.load_document(&path).await?;
                Ok(self.reader_for(doc.meta))
            })
            .await
    }

    async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError> {
        self.inner.assert_open()?;
        self.inner
            .operations
            .enqueue_barrier(|| async {
                let dirs = if let Some(cwd) = cwd {
                    vec![self.inner.get_session_dir(cwd).await?]
                } else {
                    self.inner.list_session_dirs().await?
                };
                let mut sessions = Vec::new();
                for dir in dirs {
                    if !self
                        .inner
                        .fs
                        .exists(&dir)
                        .await
                        .map_err(|e| SessionError::Io(e.message))?
                    {
                        continue;
                    }
                    let files = self
                        .inner
                        .fs
                        .list_dir(&dir)
                        .await
                        .map_err(|e| SessionError::Io(e.message))?;
                    for file in files {
                        if file.is_dir || !file.path.extension().is_some_and(|e| e == "jsonl") {
                            continue;
                        }
                        let path = file.path.to_string_lossy().into_owned();
                        sessions.push(self.inner.load_metadata_from_path(&path).await?);
                    }
                }
                sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                Ok(sessions)
            })
            .await
    }

    async fn append_entry(
        &self,
        session_id: &str,
        pending: PendingSessionWrite,
    ) -> Result<SessionTreeEntry, SessionError> {
        self.inner.assert_open()?;
        let path = self.inner.resolve_path_for_id(session_id).await?;
        let operation_key = self.inner.operation_key_for_path(&path);
        self.inner
            .operations
            .enqueue(operation_key, || async {
                if !self
                    .inner
                    .fs
                    .exists(Path::new(&path))
                    .await
                    .map_err(|e| SessionError::Io(e.message))?
                {
                    return Err(SessionError::NotFound(path.clone()));
                }
                if !self.inner.cache.lock().contains_key(&path) {
                    self.inner.load_document(&path).await?;
                }
                let (entry, append_line) = {
                    let mut cache = self.inner.cache.lock();
                    let session = cache
                        .get_mut(&path)
                        .ok_or_else(|| SessionError::NotFound(path.clone()))?;
                    let parent_id = session.leaf_id.clone();
                    let id = loop_ai::new_id();
                    let ts = now_ms();
                    let entry = pending_to_entry(pending, id, parent_id, ts);
                    if session.entry_ids.contains(entry.id()) {
                        return Err(SessionError::Invalid(format!(
                            "Entry {} already exists",
                            entry.id()
                        )));
                    }
                    let append_line = format!(
                        "{}\n",
                        serde_json::to_string(&entry)
                            .map_err(|e| SessionError::Storage(e.to_string()))?
                    );
                    (entry, append_line)
                };
                self.inner
                    .fs
                    .append_file(Path::new(&path), append_line.as_bytes())
                    .await
                    .map_err(|e| SessionError::Io(e.message))?;
                {
                    let mut cache = self.inner.cache.lock();
                    let session = cache
                        .get_mut(&path)
                        .ok_or_else(|| SessionError::NotFound(path.clone()))?;
                    session.entry_ids.insert(entry.id().to_string());
                    if let SessionTreeEntry::Leaf { target_id, .. } = &entry {
                        session.leaf_id = target_id.clone();
                    } else {
                        session.leaf_id = Some(entry.id().to_string());
                    }
                    session.entries.push(entry.clone());
                }
                Ok(entry)
            })
            .await
    }

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.inner.assert_open()?;
        let path = self.inner.resolve_path_for_id(id).await?;
        let operation_key = self.inner.operation_key_for_path(&path);
        self.inner
            .operations
            .enqueue(operation_key, || async {
                self.inner
                    .fs
                    .remove(Path::new(&path))
                    .await
                    .map_err(|e| SessionError::Io(e.message))?;
                self.inner.cache.lock().remove(&path);
                self.inner.id_to_path.lock().remove(id);
                Ok(())
            })
            .await
    }

    async fn fork(
        &self,
        source_id: &str,
        selection: SessionForkSelection,
        name: Option<String>,
    ) -> Result<Arc<dyn SessionReader>, SessionError> {
        self.inner.assert_open()?;
        let source_path = self.inner.resolve_path_for_id(source_id).await?;
        let source_operation_key = self.inner.operation_key_for_path(&source_path);

        let (cwd_encoded, cwd_meta, parent_path, entries) = self
            .inner
            .operations
            .enqueue(source_operation_key, || async {
                if !self
                    .inner
                    .fs
                    .exists(Path::new(&source_path))
                    .await
                    .map_err(|e| SessionError::Io(e.message))?
                {
                    return Err(SessionError::NotFound(source_path.clone()));
                }
                let doc = self.inner.load_document(&source_path).await?;
                let leaf = self
                    .inner
                    .cache
                    .lock()
                    .get(&source_path)
                    .and_then(|c| c.leaf_id.clone());
                let selected =
                    entries_for_fork_selection(&doc.entries, leaf.as_deref(), selection)?;
                let cwd_encoded = doc.meta.cwd.clone().unwrap_or_else(|| ".".into());
                Ok((
                    cwd_encoded,
                    doc.meta.cwd.clone(),
                    source_path.clone(),
                    selected,
                ))
            })
            .await?;

        let id = create_session_id();
        let timestamp_iso = iso_timestamp(now_ms());
        let file_name = document_file_name(&timestamp_iso, &id);
        let operation_key = document_operation_key(&cwd_encoded, &file_name);
        self.inner
            .operations
            .enqueue(operation_key.clone(), || async {
                let meta = self
                    .inner
                    .create_document(
                        id,
                        timestamp_iso,
                        file_name,
                        operation_key,
                        cwd_encoded,
                        cwd_meta,
                        name,
                        Some(parent_path),
                        entries,
                    )
                    .await?;
                Ok(self.reader_for(meta))
            })
            .await
    }
}

fn path_file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

fn cwd_for_storage(cwd: Option<String>) -> (String, Option<String>) {
    match cwd {
        Some(c) if !c.is_empty() => (c.clone(), Some(c)),
        _ => (".".into(), None),
    }
}

fn encode_cwd(cwd: &str) -> String {
    let stripped = cwd.trim_start_matches(['/', '\\']);
    let encoded: String = stripped
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == ':' { '-' } else { c })
        .collect();
    format!("--{encoded}--")
}

fn url_encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn document_file_name(timestamp_iso: &str, id: &str) -> String {
    let safe_ts = timestamp_iso.replace([':', '.'], "-");
    format!("{}_{}.jsonl", safe_ts, url_encode_component(id))
}

fn document_operation_key(cwd: &str, file_name: &str) -> String {
    format!(
        "document:{}",
        serde_json::to_string(&[encode_cwd(cwd), file_name.to_string()])
            .unwrap_or_else(|_| format!("{}:{}", encode_cwd(cwd), file_name))
    )
}

fn iso_timestamp(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn parse_header_timestamp(s: &str) -> Result<i64, SessionError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis());
    }
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    Err(SessionError::Invalid(format!("invalid timestamp: {s}")))
}

fn invalid_session(path: &str, message: &str) -> SessionError {
    SessionError::Invalid(format!("Invalid JSONL session file {path}: {message}"))
}

fn invalid_entry(path: &str, line: usize, message: &str) -> SessionError {
    SessionError::Invalid(format!(
        "Invalid JSONL session file {path}: line {line} {message}"
    ))
}

fn parse_header(line: &str, path: &str) -> Result<SessionHeader, SessionError> {
    let header: SessionHeader = serde_json::from_str(line).map_err(|_| {
        invalid_session(path, "first line is not a valid session header")
    })?;
    if header.kind != "session" || header.version != 3 {
        return Err(invalid_session(
            path,
            if header.kind == "session" {
                "unsupported session version"
            } else {
                "first line is not a valid session header"
            },
        ));
    }
    if header.id.is_empty() {
        return Err(invalid_session(path, "session header is missing id"));
    }
    if header.timestamp.is_empty() {
        return Err(invalid_session(path, "session header is missing timestamp"));
    }
    if header.cwd.is_empty() {
        return Err(invalid_session(path, "session header is missing cwd"));
    }
    Ok(header)
}

fn parse_entry(line: &str, path: &str, line_number: usize) -> Result<SessionTreeEntry, SessionError> {
    serde_json::from_str(line)
        .map_err(|_| invalid_entry(path, line_number, "is not valid JSON"))
}

fn metadata_from_header(header: SessionHeader, path: &str) -> SessionMetadata {
    let name = header
        .metadata
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    SessionMetadata {
        id: header.id,
        cwd: if header.cwd == "." {
            None
        } else {
            Some(header.cwd)
        },
        name,
        parent_session_id: header.parent_session,
        created_at: parse_header_timestamp(&header.timestamp).unwrap_or_else(|_| now_ms()),
        path: Some(path.to_string()),
    }
}

fn compute_leaf_id(entries: &[SessionTreeEntry]) -> Option<String> {
    let mut leaf_id = None;
    for entry in entries {
        leaf_id = match entry {
            SessionTreeEntry::Leaf { target_id, .. } => target_id.clone(),
            _ => Some(entry.id().to_string()),
        };
    }
    leaf_id
}

fn pending_to_entry(
    pending: PendingSessionWrite,
    id: String,
    parent_id: Option<String>,
    timestamp: i64,
) -> SessionTreeEntry {
    match pending {
        PendingSessionWrite::Message { message } => SessionTreeEntry::Message {
            id,
            parent_id,
            timestamp,
            message,
        },
        PendingSessionWrite::ThinkingLevelChange { thinking_level } => {
            SessionTreeEntry::ThinkingLevelChange {
                id,
                parent_id,
                timestamp,
                thinking_level,
            }
        }
        PendingSessionWrite::ModelChange { provider, model_id } => SessionTreeEntry::ModelChange {
            id,
            parent_id,
            timestamp,
            provider,
            model_id,
        },
        PendingSessionWrite::ActiveToolsChange { tool_names } => {
            SessionTreeEntry::ActiveToolsChange {
                id,
                parent_id,
                timestamp,
                tool_names,
            }
        }
        PendingSessionWrite::Compaction {
            summary,
            first_kept_entry_id,
            details,
        } => SessionTreeEntry::Compaction {
            id,
            parent_id,
            timestamp,
            summary,
            first_kept_entry_id,
            details,
        },
        PendingSessionWrite::BranchSummary { summary } => SessionTreeEntry::BranchSummary {
            id,
            parent_id,
            timestamp,
            summary,
        },
        PendingSessionWrite::Leaf { target_id } => SessionTreeEntry::Leaf {
            id,
            parent_id,
            timestamp,
            target_id,
        },
        PendingSessionWrite::Label { label } => SessionTreeEntry::Label {
            id,
            parent_id,
            timestamp,
            label,
        },
        PendingSessionWrite::SessionInfo { name } => SessionTreeEntry::SessionInfo {
            id,
            parent_id,
            timestamp,
            name,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_strips_leading_slashes() {
        assert_eq!(encode_cwd("/home/user"), "--home-user--");
        assert_eq!(encode_cwd("C:\\proj"), "--C--proj--");
    }
}
