//! Which variables tool commands do not receive.

use super::{paths::models_json_path, Settings};
use loop_agent::harness::ToolEnvPolicy;
use std::path::Path;

/// The `apiKeyEnv` names of the custom providers in `models.json` and of
/// `webSearch`. A missing or unreadable file adds none. Only global settings
/// can set `webSearch.apiKeyEnv`.
pub fn custom_key_envs(agent_dir: &Path, settings: &Settings) -> Vec<String> {
    let mut names: Vec<String> = settings
        .web_search
        .iter()
        .flat_map(|search| search.api_key_env.iter().cloned())
        .collect();
    let providers = std::fs::read_to_string(models_json_path(agent_dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| value.get("providers").cloned());
    for provider in providers
        .iter()
        .filter_map(|value| value.as_array())
        .flatten()
    {
        let list = provider
            .get("apiKeyEnv")
            .or_else(|| provider.get("api_key_env"));
        names.extend(
            list.and_then(|value| value.as_array())
                .into_iter()
                .flatten()
                .filter_map(|name| name.as_str().map(str::to_owned)),
        );
    }
    names
}

/// The policy for a Session's tool environment: the built-in provider key and
/// Runtime token names, the configured `apiKeyEnv` names, minus `tools.passEnv`.
pub fn tool_env_policy(agent_dir: &Path, settings: &Settings) -> ToolEnvPolicy {
    ToolEnvPolicy::new(
        &custom_key_envs(agent_dir, settings),
        &settings.tools.pass_env,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_secret_list_matches_the_names_loop_reads() {
        use crate::config::tracing::{ENV_LANGFUSE_PUBLIC_KEY, ENV_LANGFUSE_SECRET_KEY};
        let names = loop_agent::harness::removed_tool_env_names(&[]);
        assert!(names.contains(ENV_LANGFUSE_PUBLIC_KEY));
        assert!(names.contains(ENV_LANGFUSE_SECRET_KEY));
    }
}
