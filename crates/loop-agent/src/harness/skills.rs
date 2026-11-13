//! SKILL.md loading.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Deserialize;

use crate::harness::types::{FileError, FileSystem, Skill};

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

/// Load skills recursively from a directory on the Runtime host (a global
/// skill folder). Skills of a Workspace load through [`load_skills_in`].
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

/// Ignore rules of one folder, in the order the `ignore` crate applies them.
struct IgnoreLevel {
    ignore: Option<Gitignore>,
    gitignore: Option<Gitignore>,
    exclude: Option<Gitignore>,
}

type IgnoreLevels = Arc<Vec<Arc<IgnoreLevel>>>;

/// The rules in the file `name` of `dir`, read through `fs`.
async fn read_rules(fs: &dyn FileSystem, dir: &Path, name: &Path) -> Option<Gitignore> {
    let text = fs.read_text_file(&dir.join(name)).await.ok()?;
    let mut builder = GitignoreBuilder::new(dir);
    for line in text.strip_prefix('\u{feff}').unwrap_or(&text).lines() {
        let _ = builder.add_line(None, line);
    }
    builder.build().ok()
}

/// The rules of `dir`. As in the host walker, `.ignore` always counts;
/// `.gitignore` counts only inside a git repository (`in_repo`, set here
/// when `dir` has `.git`), and `.git/info/exclude` where `.git` is.
async fn ignore_level(fs: &dyn FileSystem, dir: &Path, in_repo: &mut bool) -> IgnoreLevel {
    let has_git = fs.exists(&dir.join(".git")).await.unwrap_or(false);
    *in_repo |= has_git;
    IgnoreLevel {
        ignore: read_rules(fs, dir, Path::new(".ignore")).await,
        gitignore: if *in_repo {
            read_rules(fs, dir, Path::new(".gitignore")).await
        } else {
            None
        },
        exclude: if has_git {
            read_rules(fs, dir, Path::new(".git/info/exclude")).await
        } else {
            None
        },
    }
}

/// Whether the rules hide `path`. The deepest folder with a matching rule
/// decides; inside one folder `.ignore` goes before `.gitignore`.
fn hidden_by_rules(levels: &[Arc<IgnoreLevel>], path: &Path, is_dir: bool) -> bool {
    for level in levels.iter().rev() {
        for rules in [&level.ignore, &level.gitignore, &level.exclude]
            .into_iter()
            .flatten()
        {
            let found = rules.matched(path, is_dir);
            if !found.is_none() {
                return found.is_ignore();
            }
        }
    }
    false
}

/// Load skills recursively from a directory of a Workspace, through its
/// [`FileSystem`]. It finds the same files as [`load_skills`]: hidden folders
/// count, linked folders are not entered, and ignore files apply as the
/// host walker applies them, in the order the folder lists its entries.
/// The git-wide exclude file of the user is not read (it is a host file).
pub async fn load_skills_in(
    fs: &dyn FileSystem,
    root: &Path,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diags = Vec::new();
    let Ok(root) = fs.absolute_path(root) else {
        return (skills, diags);
    };
    // Ignore files of the parents of the root count too.
    let mut chain: Vec<&Path> = root.ancestors().skip(1).collect();
    chain.reverse();
    let mut above = Vec::new();
    let mut in_repo = false;
    for dir in chain {
        above.push(Arc::new(ignore_level(fs, dir, &mut in_repo).await));
    }

    /// An entry still to visit, with the rules of its parent folders.
    struct Entry {
        path: PathBuf,
        is_dir: bool,
        is_root: bool,
        parent: IgnoreLevels,
        in_repo: bool,
    }
    let mut visited = HashSet::new();
    let mut stack = vec![Entry {
        path: root,
        is_dir: true,
        is_root: true,
        parent: Arc::new(above),
        in_repo,
    }];
    while let Some(entry) = stack.pop() {
        if !entry.is_root && entry.path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
            let loaded = fs
                .read_text_file(&entry.path)
                .await
                .map_err(|e| FileError::new(crate::harness::types::FileErrorCode::Io, e.message));
            match loaded {
                Ok(text) => skills.push(parse_skill(&entry.path, &text)),
                Err(e) => diags.push(SkillDiagnostic {
                    path: entry.path.clone(),
                    message: e.to_string(),
                }),
            }
        }
        if !entry.is_dir {
            continue;
        }
        // A linked folder may list as a folder; like the host walker, do
        // not enter it (the root is entered even when it is a link).
        if !entry.is_root
            && matches!(
                fs.symlink_metadata(&entry.path).await,
                Ok(meta) if meta.kind == crate::harness::types::PathKind::Symlink
            )
        {
            continue;
        }
        let key = fs
            .canonical_path(&entry.path)
            .await
            .unwrap_or_else(|_| entry.path.clone());
        if !visited.insert(key) {
            continue;
        }
        let mut in_repo = entry.in_repo;
        let level = ignore_level(fs, &entry.path, &mut in_repo).await;
        let mut levels: Vec<_> = entry.parent.iter().cloned().collect();
        levels.push(Arc::new(level));
        let levels: IgnoreLevels = Arc::new(levels);
        let Ok(listed) = fs.list_dir(&entry.path).await else {
            continue;
        };
        let children: Vec<Entry> = listed
            .into_iter()
            .filter(|child| !hidden_by_rules(&levels, &child.path, child.is_dir))
            .map(|child| Entry {
                path: child.path,
                is_dir: child.is_dir,
                is_root: false,
                parent: Arc::clone(&levels),
                in_repo,
            })
            .collect();
        stack.extend(children.into_iter().rev());
    }
    (skills, diags)
}

fn load_skill_file(path: &Path) -> Result<Skill, FileError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| FileError::new(crate::harness::types::FileErrorCode::Io, e.to_string()))?;
    Ok(parse_skill(path, &text))
}

/// The skill in the text of the `SKILL.md` at `path`. Shared by the host
/// loader and the one that reads through a workspace [`FileSystem`].
fn parse_skill(path: &Path, text: &str) -> Skill {
    let (fm, body) = split_frontmatter(text);
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
    Skill {
        name,
        description: fm.description.unwrap_or_default(),
        body: body.trim().to_string(),
        path: path.to_path_buf(),
        disable_model_invocation: fm.disable_model_invocation,
    }
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
