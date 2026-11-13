//! Resource discovery: skills, prompts, context, extensions, hooks, themes.

use std::path::{Path, PathBuf};

use loop_agent::harness::prompt_templates::{load_prompt_templates, load_prompt_templates_in};
use loop_agent::harness::skills::{host_global_gitignore, load_skills, load_skills_in};
use loop_agent::harness::types::{FileSystem, PromptTemplate, Skill};

use crate::config::paths::{get_project_dir, settings_path};
use crate::config::Settings;

/// Loaded resources for a session.
#[derive(Debug, Clone, Default)]
pub struct LoadedResources {
    /// Enabled skills (offered to the model and as `/skill:` commands).
    pub skills: Vec<Skill>,
    /// Discovered skills turned off via `disabledSkills` in settings.
    pub disabled_skills: Vec<Skill>,
    /// Prompt templates.
    pub prompts: Vec<PromptTemplate>,
    /// Extension script paths.
    pub extension_paths: Vec<PathBuf>,
    /// Hook JSON paths.
    pub hook_paths: Vec<PathBuf>,
    /// Theme search dirs.
    pub theme_dirs: Vec<PathBuf>,
}

impl LoadedResources {
    /// Move a discovered skill between the enabled and disabled lists.
    ///
    /// Returns `false` when no skill has that name.
    pub fn set_skill_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let (from, to) = if enabled {
            (&mut self.disabled_skills, &mut self.skills)
        } else {
            (&mut self.skills, &mut self.disabled_skills)
        };
        if let Some(i) = from.iter().position(|s| s.name == name) {
            to.push(from.remove(i));
            return true;
        }
        to.iter().any(|s| s.name == name)
    }

    /// All discovered skills with their enabled flag, sorted by name.
    pub fn all_skills(&self) -> Vec<(&Skill, bool)> {
        let mut all: Vec<_> = self
            .skills
            .iter()
            .map(|s| (s, true))
            .chain(self.disabled_skills.iter().map(|s| (s, false)))
            .collect();
        all.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        all
    }
}

/// Where a resource folder lives. A global folder belongs to the user of the
/// Runtime and is a Runtime host path. A project folder belongs to the
/// Workspace and is a path in the namespace of its execution environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Scope {
    Global,
    Project,
}

/// The skill folders in precedence order (the first skill of a name wins).
///
/// `listed_in` says which settings file the `skills` list of `settings` came
/// from ([`settings_layers`]).
fn skill_dirs(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    settings: &Settings,
    listed_in: Scope,
) -> Vec<(PathBuf, Scope)> {
    let mut skill_dirs = Vec::new();

    skill_dirs.push((agent_dir.join("skills"), Scope::Global));
    // Cross-harness user skills (shared with Claude Code)
    if let Some(home) = dirs::home_dir() {
        skill_dirs.push((home.join(".agents").join("skills"), Scope::Global));
        skill_dirs.push((home.join(".claude").join("skills"), Scope::Global));
    }
    if project_trusted {
        skill_dirs.push((get_project_dir(cwd).join("skills"), Scope::Project));
        // Walk ancestors for .agents/skills and .claude/skills
        let mut dir = cwd.to_path_buf();
        loop {
            skill_dirs.push((dir.join(".agents").join("skills"), Scope::Project));
            skill_dirs.push((dir.join(".claude").join("skills"), Scope::Project));
            if !dir.pop() {
                break;
            }
        }
    }

    // Extra skill paths from settings. The folder check is the loader's:
    // a folder that is not there loads nothing.
    for entry in &settings.skills {
        skill_dirs.push((expand_path(entry, agent_dir), listed_in));
    }
    skill_dirs
}

/// Which settings file the `skills` and the `prompts` list of the merged
/// `settings` came from. A trusted project `settings.json` replaces a list
/// when it names one (`project_overlay`), so a list that differs from the
/// global file's own list came from the project: its entries are project
/// paths and are read through the Workspace. The merged settings do not keep
/// the layer, so this loads the global file again and compares. A list equal
/// to the global one names the user's own folders and stays a host read.
fn settings_layers(agent_dir: &Path, project_trusted: bool, settings: &Settings) -> (Scope, Scope) {
    if !project_trusted {
        return (Scope::Global, Scope::Global);
    }
    let global = Settings::load_file(&settings_path(agent_dir)).unwrap_or_default();
    let layer = |merged: &[String], own: &[String]| {
        if merged == own {
            Scope::Global
        } else {
            Scope::Project
        }
    };
    (
        layer(&settings.skills, &global.skills),
        layer(&settings.prompts, &global.prompts),
    )
}

/// The prompt template folders in precedence order.
fn prompt_dirs(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    settings: &Settings,
    listed_in: Scope,
) -> Vec<(PathBuf, Scope)> {
    let mut prompt_dirs = vec![(agent_dir.join("prompts"), Scope::Global)];
    if project_trusted {
        prompt_dirs.push((get_project_dir(cwd).join("prompts"), Scope::Project));
    }
    for entry in &settings.prompts {
        prompt_dirs.push((expand_path(entry, agent_dir), listed_in));
    }
    prompt_dirs
}

/// Load skills, prompts, extensions, hooks for agent + optional trusted project.
///
/// Every folder is read on the Runtime host. A Session on a Workspace loads
/// through [`load_resources_in`], which reads the project folders through the
/// Workspace.
pub fn load_resources(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    settings: &Settings,
) -> LoadedResources {
    let mut out = LoadedResources::default();
    let mut seen_dirs = std::collections::HashSet::new();
    let mut seen_skills = std::collections::HashSet::new();
    for (dir, _) in skill_dirs(agent_dir, cwd, project_trusted, settings, Scope::Global) {
        if !dir.is_dir() {
            continue;
        }
        if !seen_dirs.insert(dir.canonicalize().unwrap_or_else(|_| dir.clone())) {
            continue;
        }
        let (skills, _) = load_skills(&dir);
        add_skills(&mut out, &mut seen_skills, settings, skills);
    }

    let mut seen_prompts = std::collections::HashSet::new();
    for (dir, _) in prompt_dirs(agent_dir, cwd, project_trusted, settings, Scope::Global) {
        if !dir.is_dir() {
            continue;
        }
        let (templates, _) = load_prompt_templates(&dir);
        add_prompts(&mut out, &mut seen_prompts, templates);
    }

    collect_rest(&mut out, agent_dir, cwd, project_trusted, settings);
    out
}

/// [`load_resources`] for a Session on a Workspace: the project folders
/// (the project's `skills` and `prompts`, and the `.agents/skills` and
/// `.claude/skills` of the working directory and its parents) are read
/// through `workspace`, the execution environment of that Workspace. The
/// global folders (the agent directory, the user's `~/.agents` and
/// `~/.claude`, and folders named in settings) stay Runtime host reads.
/// Trust works as before: with `project_trusted` false no project folder
/// is read, and a Runtime under a tool lock passes `false`.
pub async fn load_resources_in(
    workspace: &dyn FileSystem,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    settings: &Settings,
) -> LoadedResources {
    let mut out = LoadedResources::default();
    // The user's global ignore file is a host read, made once.
    let global_ignore = host_global_gitignore();
    let (skills_from, prompts_from) = settings_layers(agent_dir, project_trusted, settings);
    let mut seen_dirs = std::collections::HashSet::new();
    let mut seen_skills = std::collections::HashSet::new();
    for (dir, scope) in skill_dirs(agent_dir, cwd, project_trusted, settings, skills_from) {
        let (key, skills) = match scope {
            Scope::Global => {
                if !dir.is_dir() {
                    continue;
                }
                let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
                if seen_dirs.contains(&key) {
                    continue;
                }
                (key, load_skills(&dir).0)
            }
            Scope::Project => {
                if !workspace_is_dir(workspace, &dir).await {
                    continue;
                }
                let key = workspace
                    .canonical_path(&dir)
                    .await
                    .unwrap_or_else(|_| dir.clone());
                if seen_dirs.contains(&key) {
                    continue;
                }
                (key, load_skills_in(workspace, &dir, &global_ignore).await.0)
            }
        };
        seen_dirs.insert(key);
        add_skills(&mut out, &mut seen_skills, settings, skills);
    }

    let mut seen_prompts = std::collections::HashSet::new();
    for (dir, scope) in prompt_dirs(agent_dir, cwd, project_trusted, settings, prompts_from) {
        let templates = match scope {
            Scope::Global => {
                if !dir.is_dir() {
                    continue;
                }
                load_prompt_templates(&dir).0
            }
            Scope::Project => {
                if !workspace_is_dir(workspace, &dir).await {
                    continue;
                }
                load_prompt_templates_in(workspace, &dir).await.0
            }
        };
        add_prompts(&mut out, &mut seen_prompts, templates);
    }

    collect_rest(&mut out, agent_dir, cwd, project_trusted, settings);
    out
}

/// Whether `dir` is a folder in the Workspace (a link to one counts).
async fn workspace_is_dir(workspace: &dyn FileSystem, dir: &Path) -> bool {
    workspace.file_info(dir).await.is_ok_and(|info| info.is_dir)
}

fn add_skills(
    out: &mut LoadedResources,
    seen_skills: &mut std::collections::HashSet<String>,
    settings: &Settings,
    skills: Vec<Skill>,
) {
    for skill in skills {
        if !seen_skills.insert(skill.name.clone()) {
            continue;
        }
        if settings.disabled_skills.contains(&skill.name) {
            out.disabled_skills.push(skill);
        } else {
            out.skills.push(skill);
        }
    }
}

fn add_prompts(
    out: &mut LoadedResources,
    seen_prompts: &mut std::collections::HashSet<String>,
    templates: Vec<PromptTemplate>,
) {
    for tmpl in templates {
        if seen_prompts.insert(tmpl.name.clone()) {
            out.prompts.push(tmpl);
        }
    }
}

/// Extensions, hooks and themes. These are found on the Runtime host: the
/// Rhai extensions run in the Runtime, the hook files belong to the hook
/// loader (`hooks_load`, which reads them from these paths), and themes
/// are a Surface matter.
fn collect_rest(
    out: &mut LoadedResources,
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    settings: &Settings,
) {
    // Extensions
    collect_rhai(&agent_dir.join("extensions"), &mut out.extension_paths);
    if project_trusted {
        collect_rhai(
            &get_project_dir(cwd).join("extensions"),
            &mut out.extension_paths,
        );
    }
    for entry in &settings.extensions {
        let p = expand_path(entry, agent_dir);
        if p.is_file() {
            out.extension_paths.push(p);
        } else if p.is_dir() {
            collect_rhai(&p, &mut out.extension_paths);
        }
    }

    // Hooks
    collect_json(&agent_dir.join("hooks"), &mut out.hook_paths);
    if project_trusted {
        collect_json(&get_project_dir(cwd).join("hooks"), &mut out.hook_paths);
    }

    out.theme_dirs.push(agent_dir.join("themes"));
    if project_trusted {
        out.theme_dirs.push(get_project_dir(cwd).join("themes"));
    }
    for entry in &settings.themes {
        let p = expand_path(entry, agent_dir);
        if p.is_dir() {
            out.theme_dirs.push(p);
        }
    }
}

fn expand_path(entry: &str, agent_dir: &Path) -> PathBuf {
    let s = if let Some(rest) = entry.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else if entry.starts_with('/') {
        PathBuf::from(entry)
    } else {
        agent_dir.join(entry)
    };
    s
}

fn collect_rhai(dir: &Path, out: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("rhai") {
                out.push(path);
            } else if path.is_dir() {
                let main = path.join("main.rhai");
                if main.is_file() {
                    out.push(main);
                }
            }
        }
    }
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(agent_dir: &Path, name: &str) {
        let dir = agent_dir.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\nbody"),
        )
        .unwrap();
    }

    /// Names of test skills only (the user's real `~/.claude/skills` also load).
    fn names(skills: &[Skill]) -> Vec<&str> {
        skills
            .iter()
            .map(|s| s.name.as_str())
            .filter(|n| ["alpha", "beta"].contains(n))
            .collect()
    }

    #[test]
    fn disabled_skills_are_loaded_separately_and_toggle() {
        let agent = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_skill(agent.path(), "alpha");
        write_skill(agent.path(), "beta");
        let settings = Settings {
            disabled_skills: vec!["beta".into()],
            ..Settings::default()
        };

        let mut res = load_resources(agent.path(), cwd.path(), false, &settings);
        assert_eq!(names(&res.skills), ["alpha"]);
        assert_eq!(names(&res.disabled_skills), ["beta"]);

        assert!(res.set_skill_enabled("beta", true));
        assert!(res.set_skill_enabled("alpha", false));
        assert_eq!(names(&res.skills), ["beta"]);
        assert_eq!(names(&res.disabled_skills), ["alpha"]);
        assert!(res.set_skill_enabled("beta", true));
        assert!(!res.set_skill_enabled("missing", true));

        let all: Vec<_> = res
            .all_skills()
            .into_iter()
            .map(|(s, on)| (s.name.as_str(), on))
            .filter(|(n, _)| ["alpha", "beta"].contains(n))
            .collect();
        assert_eq!(all, [("alpha", false), ("beta", true)]);
    }

    #[test]
    fn disabled_skills_serialize_only_when_set() {
        let mut settings = Settings::default();
        assert!(!serde_json::to_string(&settings)
            .unwrap()
            .contains("disabledSkills"));
        crate::config::settings::set_skill_disabled(&mut settings.disabled_skills, "a", false);
        crate::config::settings::set_skill_disabled(&mut settings.disabled_skills, "a", false);
        assert_eq!(settings.disabled_skills, ["a"]);
        assert!(serde_json::to_string(&settings)
            .unwrap()
            .contains("\"disabledSkills\":[\"a\"]"));
        crate::config::settings::set_skill_disabled(&mut settings.disabled_skills, "a", true);
        assert!(settings.disabled_skills.is_empty());
    }
}
