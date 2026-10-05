//! Resource discovery: skills, prompts, context, extensions, hooks, themes.

use std::path::{Path, PathBuf};

use loop_agent::harness::prompt_templates::load_prompt_templates;
use loop_agent::harness::skills::load_skills;
use loop_agent::harness::types::{PromptTemplate, Skill};

use crate::config::paths::get_project_dir;
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

/// Load skills, prompts, extensions, hooks for agent + optional trusted project.
pub fn load_resources(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
    settings: &Settings,
) -> LoadedResources {
    let mut out = LoadedResources::default();
    let mut skill_dirs = Vec::new();

    skill_dirs.push(agent_dir.join("skills"));
    // Cross-harness user skills
    if let Some(home) = dirs::home_dir() {
        skill_dirs.push(home.join(".agents").join("skills"));
    }
    if project_trusted {
        skill_dirs.push(get_project_dir(cwd).join("skills"));
        // Walk ancestors for .agents/skills
        let mut dir = cwd.to_path_buf();
        loop {
            skill_dirs.push(dir.join(".agents").join("skills"));
            if !dir.pop() {
                break;
            }
        }
    }

    // Settings skill paths (supports ~/.claude/skills opt-in)
    for entry in &settings.skills {
        let path = expand_path(entry, agent_dir);
        if path.is_dir() {
            skill_dirs.push(path);
        }
    }

    let mut seen_skills = std::collections::HashSet::new();
    for dir in skill_dirs {
        if !dir.is_dir() {
            continue;
        }
        let (skills, _) = load_skills(&dir);
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

    // Prompts
    let mut prompt_dirs = vec![agent_dir.join("prompts")];
    if project_trusted {
        prompt_dirs.push(get_project_dir(cwd).join("prompts"));
    }
    for entry in &settings.prompts {
        prompt_dirs.push(expand_path(entry, agent_dir));
    }
    let mut seen_prompts = std::collections::HashSet::new();
    for dir in prompt_dirs {
        if !dir.is_dir() {
            continue;
        }
        let (templates, _) = load_prompt_templates(&dir);
        for tmpl in templates {
            if seen_prompts.insert(tmpl.name.clone()) {
                out.prompts.push(tmpl);
            }
        }
    }

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

    out
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

    fn names(skills: &[Skill]) -> Vec<&str> {
        skills.iter().map(|s| s.name.as_str()).collect()
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

        let all: Vec<_> = res.all_skills().into_iter().map(|(s, on)| (s.name.as_str(), on)).collect();
        assert_eq!(all, [("alpha", false), ("beta", true)]);
    }

    #[test]
    fn disabled_skills_serialize_only_when_set() {
        let mut settings = Settings::default();
        assert!(!serde_json::to_string(&settings).unwrap().contains("disabledSkills"));
        crate::config::settings::set_skill_disabled(&mut settings.disabled_skills, "a", false);
        crate::config::settings::set_skill_disabled(&mut settings.disabled_skills, "a", false);
        assert_eq!(settings.disabled_skills, ["a"]);
        assert!(serde_json::to_string(&settings).unwrap().contains("\"disabledSkills\":[\"a\"]"));
        crate::config::settings::set_skill_disabled(&mut settings.disabled_skills, "a", true);
        assert!(settings.disabled_skills.is_empty());
    }
}
