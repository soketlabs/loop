//! Which model a run starts with.
//!
//! Loop never picks a model on the user's behalf: a run starts with the model given on
//! the command line, else the one saved by `/model`, else none (the UI asks for one).
//! A saved model that has disappeared from the catalog is reported, not swapped out.

use loop_ai::{Model, Models};

/// Outcome of [`resolve_startup_model`].
#[derive(Debug, Clone, PartialEq)]
pub enum StartupModel {
    /// Ready to prompt.
    Selected(Box<Model>),
    /// No usable model; `reason` explains a saved choice that no longer resolves.
    NotSelected {
        /// Why the saved model can't be used, if there was one.
        reason: Option<String>,
    },
}

/// Resolve the starting model.
///
/// * `explicit_provider` / `explicit_model` come from `--provider` / `--model`; if given,
///   they must resolve or this returns an error. `--model` accepts `provider/id` or a bare
///   id that is unique across providers.
/// * `saved` is the `(provider, id)` remembered in settings.
pub fn resolve_startup_model(
    models: &Models,
    saved: Option<(&str, &str)>,
    explicit_provider: Option<&str>,
    explicit_model: Option<&str>,
) -> anyhow::Result<StartupModel> {
    match (explicit_provider, explicit_model) {
        (Some(provider), Some(id)) => models
            .get_model(provider, id)
            .map(|m| StartupModel::Selected(Box::new(m)))
            .ok_or_else(|| unknown(&format!("{provider}/{id}"))),
        (None, Some(spec)) => resolve_model_spec(models, spec).map(|m| StartupModel::Selected(Box::new(m))),
        (Some(_), None) => anyhow::bail!("--provider needs --model"),
        (None, None) => Ok(match saved {
            Some((provider, id)) => match models.get_model(provider, id) {
                Some(model) => StartupModel::Selected(Box::new(model)),
                None => StartupModel::NotSelected {
                    reason: Some(format!("saved model {provider}/{id} is not available")),
                },
            },
            None => StartupModel::NotSelected { reason: None },
        }),
    }
}

/// `provider/id` (the id may itself contain `/`, e.g. `openrouter/anthropic/claude`),
/// or a bare id that exactly one provider serves. Used by `--model` and `/model <spec>`.
pub fn resolve_model_spec(models: &Models, spec: &str) -> anyhow::Result<Model> {
    if let Some((provider, id)) = spec.split_once('/') {
        if let Some(model) = models.get_model(provider, id) {
            return Ok(model);
        }
    }
    let mut matches: Vec<Model> = models
        .get_models(None)
        .into_iter()
        .filter(|m| m.id == spec)
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(unknown(spec)),
        _ => {
            let mut providers: Vec<String> = matches.into_iter().map(|m| m.provider).collect();
            providers.sort();
            anyhow::bail!(
                "model {spec} is offered by several providers ({}); use provider/{spec}",
                providers.join(", ")
            )
        }
    }
}

fn unknown(spec: &str) -> anyhow::Error {
    anyhow::anyhow!("unknown model {spec} — run `loop` and use /model to see available models")
}

#[cfg(test)]
mod tests {
    use loop_ai::providers::{custom_provider, CustomModelSpec, CustomProviderConfig};

    use super::*;

    fn provider(id: &str, model_ids: &[&str]) -> loop_ai::Provider {
        custom_provider(CustomProviderConfig {
            id: id.into(),
            name: None,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key_env: vec![],
            models: model_ids.iter().map(|m| CustomModelSpec::new(*m)).collect(),
            headers: None,
        })
    }

    fn catalog() -> Models {
        let models = Models::new();
        models.set_provider(provider("alpha", &["shared", "a-only", "vendor/nested"]));
        models.set_provider(provider("beta", &["shared"]));
        models
    }

    fn selected(result: StartupModel) -> String {
        match result {
            StartupModel::Selected(m) => format!("{}/{}", m.provider, m.id),
            other => panic!("expected a model, got {other:?}"),
        }
    }

    #[test]
    fn settings_keep_old_selections_and_default_to_none() {
        use crate::config::Settings;

        let fresh: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(fresh.selected_model(), None);
        assert!(!serde_json::to_string(&fresh)
            .unwrap()
            .contains("defaultModel"));

        let old: Settings =
            serde_json::from_str(r#"{"defaultProvider":"soket","defaultModel":"qwen3-8-27b"}"#)
                .unwrap();
        assert_eq!(old.selected_model(), Some(("soket", "qwen3-8-27b")));

        let mut global = old.clone();
        global.merge_project(fresh);
        assert_eq!(
            global.selected_model(),
            Some(("soket", "qwen3-8-27b")),
            "a project without a model must not clear the global one"
        );
        let mut project = Settings::default();
        project.set_selected_model("openrouter", "anthropic/claude-sonnet-5");
        global.merge_project(project);
        assert_eq!(
            global.selected_model_spec().as_deref(),
            Some("openrouter/anthropic/claude-sonnet-5")
        );
        global.clear_selected_model();
        assert_eq!(global.selected_model(), None);
    }

    #[test]
    fn nothing_saved_means_nothing_selected() {
        let result = resolve_startup_model(&catalog(), None, None, None).unwrap();
        assert_eq!(result, StartupModel::NotSelected { reason: None });
    }

    #[test]
    fn saved_model_is_used_when_available() {
        let result = resolve_startup_model(&catalog(), Some(("beta", "shared")), None, None);
        assert_eq!(selected(result.unwrap()), "beta/shared");
    }

    #[test]
    fn unavailable_saved_model_is_reported_not_replaced() {
        let result =
            resolve_startup_model(&catalog(), Some(("soket", "qwen3-30b")), None, None).unwrap();
        assert_eq!(
            result,
            StartupModel::NotSelected {
                reason: Some("saved model soket/qwen3-30b is not available".into())
            }
        );
    }

    #[test]
    fn explicit_flags_win_over_saved_and_must_resolve() {
        let models = catalog();
        let saved = Some(("beta", "shared"));
        assert_eq!(
            selected(resolve_startup_model(&models, saved, Some("alpha"), Some("a-only")).unwrap()),
            "alpha/a-only"
        );
        assert!(resolve_startup_model(&models, saved, Some("alpha"), Some("nope")).is_err());
        assert!(resolve_startup_model(&models, saved, Some("alpha"), None).is_err());
    }

    #[test]
    fn model_spec_forms() {
        let models = catalog();
        let spec = |s| resolve_startup_model(&models, None, None, Some(s));
        assert_eq!(
            selected(spec("alpha/vendor/nested").unwrap()),
            "alpha/vendor/nested"
        );
        assert_eq!(selected(spec("a-only").unwrap()), "alpha/a-only");
        let ambiguous = spec("shared").unwrap_err().to_string();
        assert!(ambiguous.contains("alpha, beta"), "{ambiguous}");
        assert!(spec("missing").unwrap_err().to_string().contains("/model"));
    }
}
