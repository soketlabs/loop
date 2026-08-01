//! SKILL.md loading.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::harness::types::{FileError, Skill};

/// Skill load diagnostic.
#[derive(Debug, Clone)]
pub struct SkillDiagnostic {
    /// Path.
    pub path: PathBuf,
    /// Message.
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

/// Load skills recursively from a directory.
pub fn load_skills(root: &Path) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diags = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()) != Some("SKILL.md") {
            continue;
        }
        match load_skill_file(path) {
            Ok(skill) => skills.push(skill),
            Err(e) => diags.push(SkillDiagnostic {
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
        }
    }
    (skills, diags)
}

fn load_skill_file(path: &Path) -> Result<Skill, FileError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| FileError::new(crate::harness::types::FileErrorCode::Io, e.to_string()))?;
    let (fm, body) = split_frontmatter(&text);
    let fm: SkillFrontmatter = serde_yaml::from_str(&fm).unwrap_or(SkillFrontmatter {
        name: None,
        description: None,
        disable_model_invocation: false,
    });
    let name = fm.name.unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string()
    });
    Ok(Skill {
        name,
        description: fm.description.unwrap_or_default(),
        body: body.trim().to_string(),
        path: path.to_path_buf(),
        disable_model_invocation: fm.disable_model_invocation,
    })
}

fn split_frontmatter(text: &str) -> (String, String) {
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = rest[..end].trim().to_string();
            let body = rest[end + 4..].to_string();
            return (fm, body);
        }
    }
    (String::new(), text.to_string())
}

/// Format skill invocation block.
pub fn format_skill_invocation(skill: &Skill, args: &str) -> String {
    format!(
        "<skill name=\"{}\">\n{}\n\nArgs: {}\n</skill>",
        skill.name, skill.body, args
    )
}
