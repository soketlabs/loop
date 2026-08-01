//! Session repository facade.

use std::sync::Arc;

use crate::harness::session::search::SessionSearch;
use crate::harness::session::types::{Session, SessionMetadata, SessionStore};
use crate::harness::types::SessionError;

/// Repository composing store + optional search.
pub struct SessionRepository {
    store: Arc<dyn SessionStore>,
    search: Option<Arc<dyn SessionSearch>>,
}

/// Create a repository.
pub fn create_session_repository(
    store: Arc<dyn SessionStore>,
    search: Option<Arc<dyn SessionSearch>>,
) -> SessionRepository {
    SessionRepository { store, search }
}

impl SessionRepository {
    /// Create a session.
    pub async fn create(
        &self,
        cwd: Option<String>,
        name: Option<String>,
    ) -> Result<Session, SessionError> {
        let reader = self.store.create(cwd, name).await?;
        Ok(Session::new(Arc::clone(&self.store), reader))
    }

    /// Open a session.
    pub async fn open(&self, id: &str) -> Result<Session, SessionError> {
        let reader = self.store.load(id).await?;
        Ok(Session::new(Arc::clone(&self.store), reader))
    }

    /// List sessions.
    pub async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionMetadata>, SessionError> {
        self.store.list(cwd).await
    }

    /// Delete.
    pub async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.store.delete(id).await
    }

    /// Search if backend configured.
    pub async fn search(
        &self,
        query: &str,
        cwd: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::harness::session::search::SessionSearchHit>, SessionError> {
        let Some(search) = &self.search else {
            return Err(SessionError::Invalid("no search backend".into()));
        };
        search.search(query, cwd, limit).await
    }

    /// Store handle.
    pub fn store(&self) -> Arc<dyn SessionStore> {
        Arc::clone(&self.store)
    }
}
