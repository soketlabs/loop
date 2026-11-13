//! A Session's caller environment layered over its confined filesystem and shell.
use super::super::types::*;
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Runtime token variables. No other variable carries a Runtime token.
pub const RUNTIME_TOKEN_ENVS: &[&str] = &["LOOP_RUNTIME_TOKEN"];

/// The exact variable names Loop removes from tool environments: every
/// built-in provider key variable, the Runtime token variables, and the
/// `apiKeyEnv` names of configured providers (`custom_key_envs`). This is a
/// list of names, not a pattern, so `GITHUB_TOKEN`, `NPM_TOKEN`, `AWS_*` and
/// the like stay. The Runtime and a standalone Surface both call this function.
pub fn removed_tool_env_names(custom_key_envs: &[String]) -> BTreeSet<String> {
    loop_ai::providers::PROVIDER_PRESETS
        .iter()
        .flat_map(|preset| preset.api_key_envs.iter().map(|name| (*name).to_owned()))
        .chain(RUNTIME_TOKEN_ENVS.iter().map(|name| (*name).to_owned()))
        .chain(custom_key_envs.iter().cloned())
        .collect()
}

/// Which variables tool commands do not receive, and which removals were
/// already reported this turn. Clones share one state.
#[derive(Clone)]
pub struct ToolEnvPolicy(Arc<PolicyState>);

struct PolicyState {
    removed: BTreeSet<String>,
    noted: Mutex<BTreeSet<String>>,
}

impl ToolEnvPolicy {
    /// [`removed_tool_env_names`] without the names in `pass` (`tools.passEnv`).
    pub fn new(custom_key_envs: &[String], pass: &[String]) -> Self {
        let mut removed = removed_tool_env_names(custom_key_envs);
        for name in pass {
            removed.remove(name);
        }
        Self(Arc::new(PolicyState {
            removed,
            noted: Mutex::new(BTreeSet::new()),
        }))
    }

    /// Whether a command does not receive `name`.
    pub fn removes(&self, name: &str) -> bool {
        self.0.removed.contains(name)
    }

    /// Allow the removal notes again: call when a turn starts.
    pub fn begin_turn(&self) {
        self.0
            .noted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

impl Default for ToolEnvPolicy {
    fn default() -> Self {
        Self::new(&[], &[])
    }
}

/// Whether `name` appears in `text` as a whole variable name.
fn mentions_name(text: &str, name: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(word)
            && !text[at + name.len()..].chars().next().is_some_and(word)
    })
}

/// A Session's tool environment: the caller's variables (or, with none given,
/// this process's) minus the variables its [`ToolEnvPolicy`] removes. It is
/// layered over the confined filesystem and shell.
pub struct SessionExecutionEnv {
    inner: Arc<dyn ExecutionEnv>,
    /// `None` reads this process's environment each time a command starts.
    env: Option<BTreeMap<String, String>>,
    policy: ToolEnvPolicy,
}
impl SessionExecutionEnv {
    /// The environment tools and `!shell` use. A caller environment always
    /// applies. With none, a sandboxed `inner` keeps its own minimal
    /// environment, and an unsandboxed one gets this process's environment
    /// minus the removed names.
    pub fn for_tools(
        inner: Arc<dyn ExecutionEnv>,
        env: Option<&BTreeMap<String, String>>,
        policy: &ToolEnvPolicy,
    ) -> Arc<dyn ExecutionEnv> {
        if env.is_none() && inner.sandboxed() {
            return inner;
        }
        Arc::new(Self {
            inner,
            env: env.cloned(),
            policy: policy.clone(),
        })
    }

    /// Variables a command receives, and the removed names that were set.
    fn split(&self) -> (BTreeMap<String, String>, Vec<String>) {
        let mut kept = BTreeMap::new();
        let mut removed = Vec::new();
        let mut visit = |key: String, value: String| {
            if self.policy.removes(&key) {
                removed.push(key);
            } else {
                kept.insert(key, value);
            }
        };
        match &self.env {
            Some(values) => values
                .iter()
                .for_each(|(key, value)| visit(key.clone(), value.clone())),
            None => std::env::vars_os().for_each(|(key, value)| {
                if let (Ok(key), Ok(value)) = (key.into_string(), value.into_string()) {
                    visit(key, value);
                }
            }),
        }
        (kept, removed)
    }
}
#[async_trait]
impl Shell for SessionExecutionEnv {
    fn sandboxed(&self) -> bool {
        self.inner.sandboxed()
    }

    fn removed_env_note(&self, output: &str, exit_code: i32) -> Option<String> {
        if exit_code == 0 || self.inner.sandboxed() {
            return None;
        }
        let (_, removed) = self.split();
        let mut noted = self
            .policy
            .0
            .noted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut names: Vec<String> = removed
            .into_iter()
            .filter(|name| mentions_name(output, name) && !noted.contains(name))
            .collect();
        names.sort();
        names.dedup();
        if names.is_empty() {
            return None;
        }
        noted.extend(names.iter().cloned());
        let (verb, pronoun) = if names.len() == 1 {
            ("was", "it")
        } else {
            ("were", "them")
        };
        Some(format!(
            "Note: {} {verb} removed from the command environment by Loop; allow {pronoun} with \
             tools.passEnv in global settings.",
            names.join(", ")
        ))
    }

    async fn exec(
        &self,
        command: &str,
        mut options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let mut env: std::collections::HashMap<_, _> = if options.inherit_env {
            self.split().0.into_iter().collect()
        } else {
            Default::default()
        };
        if let Some(overrides) = options.env.take() {
            env.extend(overrides);
        }
        options.env = Some(env);
        options.inherit_env = false;
        self.inner.exec(command, options).await
    }
}
#[async_trait]
impl FileSystem for SessionExecutionEnv {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.inner.absolute_path(path)
    }
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        self.inner.join_path(base, child)
    }
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        self.inner.read_text_file(path).await
    }
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        self.inner.read_binary_file(path).await
    }
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.inner.write_file(path, data).await
    }
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.inner.append_file(path, data).await
    }
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.inner.file_info(path).await
    }
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.inner.list_dir(path).await
    }
    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        self.inner.exists(path).await
    }
    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        self.inner.create_dir(path).await
    }
    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        self.inner.remove(path).await
    }
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.inner.canonical_path(path).await
    }
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        self.inner.create_temp_dir(prefix).await
    }
    async fn file_identity(
        &self,
        path: &Path,
    ) -> Result<Option<crate::harness::types::FileIdentity>, FileError> {
        self.inner.file_identity(path).await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::harness::HostExecutionEnv;

    fn host() -> Arc<dyn ExecutionEnv> {
        Arc::new(HostExecutionEnv::new(std::env::temp_dir()))
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    async fn printed(env: &Arc<dyn ExecutionEnv>, names: &[&str]) -> String {
        let command = names
            .iter()
            .map(|name| format!("printf '%s=%s;' {name} \"${{{name}-unset}}\""))
            .collect::<Vec<_>>()
            .join("; ");
        env.exec(&command, Default::default()).await.unwrap().stdout
    }

    #[test]
    fn the_removed_list_is_the_exact_names_and_nothing_else() {
        let names: Vec<_> = removed_tool_env_names(&["CUSTOM_ONE".into()])
            .into_iter()
            .collect();
        assert_eq!(
            names,
            [
                "ANTHROPIC_API_KEY",
                "CUSTOM_ONE",
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
                "LOOP_API_KEY",
                "LOOP_RUNTIME_TOKEN",
                "OPENAI_API_KEY",
                "OPENROUTER_API_KEY",
                "SOKET_API_KEY",
                "TENSORSTUDIO_API_KEY",
            ]
        );
        for kept in [
            "GITHUB_TOKEN",
            "NPM_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "GH_TOKEN",
        ] {
            assert!(!removed_tool_env_names(&[]).contains(kept), "{kept}");
        }
    }

    #[test]
    fn pass_env_keeps_names_and_can_keep_a_token() {
        let policy = ToolEnvPolicy::new(
            &["CUSTOM_ONE".into()],
            &["OPENAI_API_KEY".into(), "LOOP_RUNTIME_TOKEN".into()],
        );
        assert!(!policy.removes("OPENAI_API_KEY"));
        assert!(!policy.removes("LOOP_RUNTIME_TOKEN"));
        assert!(policy.removes("ANTHROPIC_API_KEY"));
        assert!(policy.removes("CUSTOM_ONE"));
        assert!(!policy.removes("GITHUB_TOKEN"));
    }

    #[tokio::test]
    async fn a_caller_environment_loses_the_removed_names_only() {
        let env = SessionExecutionEnv::for_tools(
            host(),
            Some(&map(&[
                ("OPENAI_API_KEY", "k"),
                ("LOOP_RUNTIME_TOKEN", "t"),
                ("CUSTOM_ONE", "c"),
                ("GITHUB_TOKEN", "g"),
                ("TASK_VALUE", "v"),
            ])),
            &ToolEnvPolicy::new(&["CUSTOM_ONE".into()], &[]),
        );
        assert_eq!(
            printed(
                &env,
                &["OPENAI_API_KEY", "LOOP_RUNTIME_TOKEN", "CUSTOM_ONE", "GITHUB_TOKEN", "TASK_VALUE"]
            )
            .await,
            "OPENAI_API_KEY=unset;LOOP_RUNTIME_TOKEN=unset;CUSTOM_ONE=unset;GITHUB_TOKEN=g;TASK_VALUE=v;"
        );
    }

    #[test]
    fn a_name_matches_only_as_a_whole_word() {
        assert!(mentions_name(
            "error: OPENAI_API_KEY not set",
            "OPENAI_API_KEY"
        ));
        assert!(mentions_name("OPENAI_API_KEY", "OPENAI_API_KEY"));
        assert!(mentions_name("$OPENAI_API_KEY.", "OPENAI_API_KEY"));
        assert!(!mentions_name("MY_OPENAI_API_KEY", "OPENAI_API_KEY"));
        assert!(!mentions_name("OPENAI_API_KEY_2", "OPENAI_API_KEY"));
    }

    #[tokio::test]
    async fn the_note_is_for_failures_of_set_and_removed_names_once_per_turn() {
        let policy = ToolEnvPolicy::default();
        let env = SessionExecutionEnv::for_tools(
            host(),
            Some(&map(&[("OPENAI_API_KEY", "k"), ("GITHUB_TOKEN", "g")])),
            &policy,
        );
        // A success, a kept name, and a removed name that was never set: no note.
        assert_eq!(env.removed_env_note("OPENAI_API_KEY", 0), None);
        assert_eq!(env.removed_env_note("GITHUB_TOKEN missing", 1), None);
        assert_eq!(env.removed_env_note("ANTHROPIC_API_KEY missing", 1), None);
        let note = env.removed_env_note("OPENAI_API_KEY missing", 1).unwrap();
        assert_eq!(
            note,
            "Note: OPENAI_API_KEY was removed from the command environment by Loop; allow it with tools.passEnv in global settings."
        );
        assert_eq!(env.removed_env_note("OPENAI_API_KEY missing", 1), None);
        policy.begin_turn();
        assert!(env.removed_env_note("OPENAI_API_KEY missing", 1).is_some());
    }
}
