//! Browser OAuth for remote MCP servers (authorization-code + PKCE).
//!
//! Tokens are stored per server under the manager's oauth directory, mode `0600`.
//! A later connect loads that file and attaches the SDK [`AuthClient`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde::Deserialize;
use tokio::sync::oneshot;

/// How long `/mcp login` waits for the browser to hit the localhost callback.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);

const CALLBACK_OK: &str = "<!doctype html><html><body><p>Loop authorization complete. You can close this tab and return to the terminal.</p></body></html>";
const CALLBACK_DENIED: &str = "<!doctype html><html><body><p>Loop authorization was not completed. You can close this tab and return to the terminal.</p></body></html>";

/// Result of an interactive OAuth login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthLoginOutcome {
    /// The browser flow finished and tokens were saved.
    Authorized,
    /// The server accepted the session without OAuth.
    NotRequired,
}

/// File path for one server's OAuth credentials.
pub(crate) fn credential_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    if !valid_server_name(name) {
        return Err(format!("invalid mcp server name '{name}'"));
    }
    Ok(dir.join(format!("{name}.json")))
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Run the authorization-code flow for `url` and save tokens at `store_path`.
///
/// `notify_url` is called with the authorization URL once the server has
/// advertised how to log in. The caller opens it in a browser.
pub(crate) async fn login<F>(
    url: &str,
    store_path: &Path,
    notify_url: F,
) -> Result<OAuthLoginOutcome, String>
where
    F: FnOnce(&str) + Send,
{
    let challenge = match auth_challenge(url).await? {
        Some(challenge) => challenge,
        None => return Ok(OAuthLoginOutcome::NotRequired),
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("failed to bind oauth callback: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed to read oauth callback port: {e}"))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/callback");

    let (tx, rx) = oneshot::channel();
    let state = CallbackState {
        tx: Arc::new(std::sync::Mutex::new(Some(tx))),
    };
    let app = Router::new()
        .route("/callback", get(callback))
        .with_state(state);
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::debug!("oauth callback server ended: {err}");
        }
    });

    let session = match authorization_session(url, store_path, &redirect, &challenge).await {
        Ok(session) => session,
        Err(err) => {
            server.abort();
            return Err(err);
        }
    };
    let auth_url = session.get_authorization_url().to_string();
    notify_url(&auth_url);

    let query = match tokio::time::timeout(LOGIN_TIMEOUT, rx).await {
        Ok(Ok(query)) => query,
        Ok(Err(_)) => {
            server.abort();
            return Err("authorization callback was dropped".into());
        }
        Err(_) => {
            server.abort();
            return Err("timed out waiting for browser authorization (3 minutes)".into());
        }
    };

    let result = finish_callback(session, query).await;
    // Let the callback response finish writing before the listener is dropped.
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.abort();
    result
}

async fn auth_challenge(url: &str) -> Result<Option<String>, String> {
    let transport = StreamableHttpClientTransport::from_uri(url);
    match ().serve(transport).await {
        Ok(client) => {
            let _ = client.cancel().await;
            Ok(None)
        }
        Err(err) if err.is_authorization_required() => {
            Ok(Some(err.auth_challenge().unwrap_or("").to_string()))
        }
        Err(err) => Err(format!("failed to reach mcp server at {url}: {err}")),
    }
}

async fn authorization_session(
    url: &str,
    store_path: &Path,
    redirect: &str,
    challenge: &str,
) -> Result<AuthorizationSession, String> {
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| format!("oauth setup failed: {e}"))?;
    manager.set_credential_store(FileOAuthStore::new(store_path));
    let challenge = if challenge.is_empty() {
        None
    } else {
        Some(challenge)
    };
    let resolution = manager
        .resolve_metadata_from_challenge(challenge)
        .await
        .map_err(|e| format!("oauth discovery failed: {e}"))?;
    manager.set_metadata(resolution.metadata);

    let mut request = AuthorizationRequest::new(redirect).with_client_name("Loop");
    if let Some(challenge) = challenge {
        request = request.with_challenge(challenge);
    }
    AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, err)| format!("oauth authorization failed: {err}"))
}

async fn finish_callback(
    session: AuthorizationSession,
    query: CallbackQuery,
) -> Result<OAuthLoginOutcome, String> {
    if let Some(err) = query.error {
        let detail = query.error_description.unwrap_or(err);
        return Err(format!("authorization denied: {detail}"));
    }
    let code = query
        .code
        .ok_or_else(|| "authorization callback missing code".to_string())?;
    let csrf = query
        .state
        .ok_or_else(|| "authorization callback missing state".to_string())?;
    session
        .handle_callback_with_issuer(&code, &csrf, query.iss.as_deref())
        .await
        .map_err(|e| format!("oauth token exchange failed: {e}"))?;
    Ok(OAuthLoginOutcome::Authorized)
}

#[derive(Clone)]
struct CallbackState {
    tx: Arc<std::sync::Mutex<Option<oneshot::Sender<CallbackQuery>>>>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(
    State(state): State<CallbackState>,
    Query(query): Query<CallbackQuery>,
) -> Html<&'static str> {
    let denied = query.error.is_some();
    if let Ok(mut slot) = state.tx.lock() {
        if let Some(tx) = slot.take() {
            let _ = tx.send(query);
        }
    }
    Html(if denied { CALLBACK_DENIED } else { CALLBACK_OK })
}

/// Build an authorized HTTP transport when `store_path` holds a token.
///
/// `Ok(None)` means there is nothing to restore and the caller should connect
/// without a bearer token.
pub(crate) async fn authorized_manager(
    url: &str,
    store_path: &Path,
) -> Result<Option<AuthorizationManager>, String> {
    if !store_path.exists() {
        return Ok(None);
    }
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| format!("oauth setup failed: {e}"))?;
    manager.set_credential_store(FileOAuthStore::new(store_path));
    match manager.initialize_from_store().await {
        Ok(true) => Ok(Some(manager)),
        Ok(false) => Ok(None),
        Err(err) => Err(format!("failed to load oauth credentials: {err}")),
    }
}

pub(crate) async fn clear_credentials(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    tokio::fs::remove_file(path)
        .await
        .map_err(|e| format!("failed to remove oauth credentials: {e}"))?;
    Ok(true)
}

struct FileOAuthStore {
    path: PathBuf,
}

impl FileOAuthStore {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl CredentialStore for FileOAuthStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let raw = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        if raw.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&raw).map_err(|e| AuthError::InternalError(e.to_string()))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AuthError::InternalError(e.to_string()))?;
        }
        let json = serde_json::to_string_pretty(&credentials)
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        let tmp = self.path.with_extension("tmp");
        tokio::fs::write(&tmp, json)
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&self.path, perms)
                .await
                .map_err(|e| AuthError::InternalError(e.to_string()))?;
        }
        Ok(())
    }

    async fn clear(&self) -> Result<(), AuthError> {
        if self.path.exists() {
            tokio::fs::remove_file(&self.path)
                .await
                .map_err(|e| AuthError::InternalError(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_path_rejects_traversal() {
        let dir = Path::new("/tmp/mcp-auth");
        assert!(credential_path(dir, "../secret").is_err());
        assert!(credential_path(dir, "a/b").is_err());
        assert!(credential_path(dir, ".hidden").is_err());
        assert_eq!(
            credential_path(dir, "notion").unwrap(),
            PathBuf::from("/tmp/mcp-auth/notion.json")
        );
    }

    #[tokio::test]
    async fn file_store_round_trips_a_token() {
        let dir = std::env::temp_dir().join(format!(
            "loop-mcp-oauth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notion.json");
        let raw = r#"{
            "client_id": "client",
            "token_response": {
                "access_token": "access-token",
                "token_type": "Bearer",
                "expires_in": 3600
            },
            "granted_scopes": ["read"],
            "token_received_at": 1,
            "issuer": "https://auth.example"
        }"#;
        let parsed: StoredCredentials = serde_json::from_str(raw).unwrap();
        let store = FileOAuthStore::new(&path);
        store.save(parsed).await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client");
        assert!(loaded.token_response.is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
