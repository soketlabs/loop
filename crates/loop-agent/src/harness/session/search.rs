//! Session search backends.

use std::sync::Arc;

use async_trait::async_trait;

use crate::harness::session::types::{SessionMetadata, SessionStore, SessionTreeEntry};
use crate::harness::types::SessionError;

/// A search hit.
#[derive(Debug, Clone)]
pub struct SessionSearchHit {
    /// Session metadata.
    pub session: SessionMetadata,
    /// Matching entry id.
    pub entry_id: String,
    /// Snippet.
    pub snippet: String,
}

/// Search backend.
#[async_trait]
pub trait SessionSearch: Send + Sync {
    /// Search sessions.
    async fn search(
        &self,
        query: &str,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionSearchHit>, SessionError>;
}

/// Lexical match helper over an entry.
pub fn find_session_entry_matches(entry: &SessionTreeEntry, query: &str) -> Option<String> {
    let q = query.to_lowercase();
    let text = entry_text(entry)?;
    if text.to_lowercase().contains(&q) {
        Some(text.chars().take(200).collect())
    } else {
        None
    }
}

fn entry_text(entry: &SessionTreeEntry) -> Option<String> {
    match entry {
        SessionTreeEntry::Message { message, .. } => Some(format!("{:?}", message)),
        SessionTreeEntry::Compaction { summary, .. }
        | SessionTreeEntry::BranchSummary { summary, .. } => Some(summary.clone()),
        SessionTreeEntry::Label { label, .. } => Some(label.clone()),
        SessionTreeEntry::SessionInfo { name, .. } => Some(name.clone()),
        _ => None,
    }
}

/// Scanning search over a store (no FTS).
pub fn create_scanning_session_search(store: Arc<dyn SessionStore>) -> Arc<dyn SessionSearch> {
    Arc::new(ScanningSearch { store })
}

struct ScanningSearch {
    store: Arc<dyn SessionStore>,
}

#[async_trait]
impl SessionSearch for ScanningSearch {
    async fn search(
        &self,
        query: &str,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionSearchHit>, SessionError> {
        let sessions = self.store.list(cwd).await?;
        let mut hits = Vec::new();
        for meta in sessions {
            let reader = self.store.load(&meta.id).await?;
            let entries = reader.read_entries(None).await?;
            for entry in entries {
                if let Some(snippet) = find_session_entry_matches(&entry, query) {
                    hits.push(SessionSearchHit {
                        session: meta.clone(),
                        entry_id: entry.id().to_string(),
                        snippet,
                    });
                    if hits.len() >= limit {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
    }
}
