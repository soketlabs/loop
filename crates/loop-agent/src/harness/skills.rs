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

/// Ignore rules of one folder.
struct IgnoreLevel {
    ignore: Option<Gitignore>,
    gitignore: Option<Gitignore>,
    exclude: Option<Gitignore>,
    /// The folder has `.git` (a folder or a file) or `.jj`.
    has_git: bool,
}

/// The rules of the folders from the top of the file system to the folder
/// being listed, and what a match needs to know about them.
struct Rules<'a> {
    /// The first `above` levels are folders above the walk root.
    above: usize,
    /// The walk root as the listing spells it, and as the file system
    /// resolves it. The folders above the root are the parents of the
    /// resolved root, and their rules see the path under the resolved root.
    root: &'a Path,
    resolved_root: Option<PathBuf>,
    /// The user's global ignore file (`core.excludesFile`).
    global: &'a Gitignore,
}

type IgnoreLevels = Arc<Vec<Arc<IgnoreLevel>>>;

/// The rules in `file` (under `dir`), read through `fs`; patterns are
/// relative to `dir`.
async fn read_rules(fs: &dyn FileSystem, dir: &Path, file: &Path) -> Option<Gitignore> {
    let text = fs.read_text_file(file).await.ok()?;
    let mut builder = GitignoreBuilder::new(dir);
    for line in text.strip_prefix('\u{feff}').unwrap_or(&text).lines() {
        let _ = builder.add_line(None, line);
    }
    builder.build().ok()
}

/// The git dir that holds `info/exclude` for the repository at `dir`: the
/// `.git` folder, or, when `.git` is a file (a worktree), the common git
/// dir named by `gitdir:` and its `commondir` file. `None` when the file
/// does not say (the exclude file is then empty, as in the host walker).
async fn git_common_dir(fs: &dyn FileSystem, dir: &Path, git_is_file: bool) -> Option<PathBuf> {
    let git = dir.join(".git");
    if !git_is_file {
        return Some(git);
    }
    let text = fs.read_text_file(&git).await.ok()?;
    let line = text.lines().next()?;
    let real = PathBuf::from(line.strip_prefix("gitdir: ")?);
    let common = fs.read_text_file(&real.join("commondir")).await.ok()?;
    let line = common.lines().next()?;
    Some(if line.starts_with('.') {
        real.join(line)
    } else {
        PathBuf::from(line)
    })
}

/// The rules of `dir`. Every ignore file is read when it is there; whether a
/// rule counts is decided at match time ([`hidden_by_rules`]).
async fn ignore_level(fs: &dyn FileSystem, dir: &Path) -> IgnoreLevel {
    let git = fs.file_info(&dir.join(".git")).await.ok();
    let has_jj = fs.exists(&dir.join(".jj")).await.unwrap_or(false);
    let exclude = match &git {
        Some(info) => match git_common_dir(fs, dir, info.is_file).await {
            Some(git_dir) => read_rules(fs, dir, &git_dir.join("info/exclude")).await,
            None => None,
        },
        None => None,
    };
    IgnoreLevel {
        ignore: read_rules(fs, dir, &dir.join(".ignore")).await,
        gitignore: read_rules(fs, dir, &dir.join(".gitignore")).await,
        exclude,
        has_git: git.is_some() || has_jj,
    }
}

/// What `rules` say about `path`: `Some(true)` ignore, `Some(false)` whitelist.
fn verdict(rules: &Option<Gitignore>, path: &Path, is_dir: bool) -> Option<bool> {
    let found = rules.as_ref()?.matched(path, is_dir);
    (!found.is_none()).then(|| found.is_ignore())
}

/// Whether the rules hide `path`, with the precedence of the `ignore` crate
/// (`Ignore::matched_ignore`):
/// 1. the first folder, deepest first, with a matching `.ignore` rule decides
///    over everything else (a whitelist counts as a match);
/// 2. then the first matching `.gitignore` rule, then the first matching
///    `.git/info/exclude` rule, then the global file; but these three count
///    only in a repository (some folder in the chain has `.git` or `.jj`)
///    and only down from the nearest folder with `.git`, so a repository
///    above a nested one does not apply;
/// 3. whatever the first verdict says is final: ignore hides, whitelist shows.
fn hidden_by_rules(
    rules: &Rules<'_>,
    levels: &[Arc<IgnoreLevel>],
    path: &Path,
    is_dir: bool,
) -> bool {
    let any_git = levels.iter().any(|level| level.has_git);
    // The folders above the root see the path under the resolved root.
    let below_resolved = rules
        .resolved_root
        .as_ref()
        .map(|resolved| resolved.join(path.strip_prefix(rules.root).unwrap_or(path)));
    let (mut ignore, mut gitignore, mut exclude) = (None, None, None);
    let mut saw_git = false;
    for (depth, level) in levels.iter().enumerate().rev() {
        let seen = match (&below_resolved, depth < rules.above) {
            (Some(resolved), true) => resolved.as_path(),
            _ => path,
        };
        if ignore.is_none() {
            ignore = verdict(&level.ignore, seen, is_dir);
        }
        if any_git && !saw_git {
            if gitignore.is_none() {
                gitignore = verdict(&level.gitignore, seen, is_dir);
            }
            if exclude.is_none() {
                exclude = verdict(&level.exclude, seen, is_dir);
            }
        }
        saw_git = saw_git || level.has_git;
    }
    let global = if any_git {
        let found = rules.global.matched(path, is_dir);
        (!found.is_none()).then(|| found.is_ignore())
    } else {
        None
    };
    ignore.or(gitignore).or(exclude).or(global).unwrap_or(false)
}

/// The user's global ignore file (`core.excludesFile`, else
/// `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore`), compiled the
/// way the host walker does. It belongs to the user of the Runtime, not to
/// a project, so it is a host read; read it once and pass it to
/// [`load_skills_in`].
pub fn host_global_gitignore() -> Gitignore {
    let Ok(cwd) = std::env::current_dir() else {
        return Gitignore::empty();
    };
    GitignoreBuilder::new(cwd).build_global().0
}

/// Load skills recursively from a directory of a Workspace, through its
/// [`FileSystem`]. It finds the same files as [`load_skills`], in the same
/// order: hidden folders count, linked folders are not entered, and the
/// ignore files (`.ignore`, `.gitignore`, `.git/info/exclude`, those of the
/// folders above the root, and `global`) apply with the same precedence.
pub async fn load_skills_in(
    fs: &dyn FileSystem,
    root: &Path,
    global: &Gitignore,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diags = Vec::new();
    let Ok(root) = fs.absolute_path(root) else {
        return (skills, diags);
    };
    // The folders above the root are the parents of the resolved root. When
    // it cannot be resolved there are none, as in the host walker.
    let resolved_root = fs.canonical_path(&root).await.ok();
    let mut above = Vec::new();
    if let Some(resolved) = &resolved_root {
        let mut chain: Vec<&Path> = resolved.ancestors().skip(1).collect();
        chain.reverse();
        for dir in chain {
            above.push(Arc::new(ignore_level(fs, dir).await));
        }
    }
    let rules = Rules {
        above: above.len(),
        root: &root,
        resolved_root,
        global,
    };

    /// An entry still to visit, with the rules of its parent folders.
    struct Entry {
        path: PathBuf,
        is_dir: bool,
        is_root: bool,
        parent: IgnoreLevels,
    }
    let mut visited = HashSet::new();
    let mut stack = vec![Entry {
        path: root.clone(),
        is_dir: true,
        is_root: true,
        parent: Arc::new(above),
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
        let mut levels: Vec<_> = entry.parent.iter().cloned().collect();
        levels.push(Arc::new(ignore_level(fs, &entry.path).await));
        let levels: IgnoreLevels = Arc::new(levels);
        let Ok(listed) = fs.list_dir(&entry.path).await else {
            continue;
        };
        let children: Vec<Entry> = listed
            .into_iter()
            .filter(|child| !hidden_by_rules(&rules, &levels, &child.path, child.is_dir))
            .map(|child| Entry {
                path: child.path,
                is_dir: child.is_dir,
                is_root: false,
                parent: Arc::clone(&levels),
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
