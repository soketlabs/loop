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
    /// Skills.
    pub skills: Vec<Skill>,
    /// Prompt templates.
    pub prompts: Vec<PromptTemplate>,
    /// Extension script paths.
    pub extension_paths: Vec<PathBuf>,
    /// Hook JSON paths.
    pub hook_paths: Vec<PathBuf>,
    /// Theme search dirs.
    pub theme_dirs: Vec<PathBuf>,
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
            if seen_skills.insert(skill.name.clone()) {
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
