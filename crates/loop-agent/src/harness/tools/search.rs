//! Read-only search tools: `grep`, `glob` and `ls`.
//!
//! They walk the host filesystem under a path the [`ExecutionEnv`] resolves,
//! so a jailed env still refuses paths outside its root. `grep` and `glob`
//! respect `.gitignore` (in a git repo or not) and skip hidden files unless
//! asked; `.git/` is never searched.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{arg_str, schema_object};
use crate::harness::types::ExecutionEnv;
use crate::harness::utils::{truncate_head, DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH};
use crate::types::{AgentTool, AgentToolResult};
use loop_ai::{TextContent, ToolResultContent};

/// The most lines `grep`, `glob` or `ls` returns.
pub const SEARCH_MAX_RESULTS: usize = 500;

/// Bytes read to decide whether a file is binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Create a `grep` tool bound to `env`.
pub fn create_grep_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "grep",
        "Grep",
        "Search file contents with a regular expression. Respects .gitignore and skips \
         hidden and binary files. Prefer this to running grep or rg through bash.",
        schema_object(
            json!({
                "pattern": {
                    "type": "string",
                    "description": "Regular expression (Rust regex syntax) to find in each line"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search, relative to the working \
                                    directory (default: the working directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files matching this .gitignore-style glob, \
                                    e.g. `*.rs` or `src/**/*.ts`"
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Match case-insensitively (default false)"
                },
                "outputMode": {
                    "type": "string",
                    "enum": ["content", "files", "count"],
                    "description": "`content` (default): `path:line:text` for each matching \
                                    line; `files`: each matching file once; `count`: \
                                    `path:count` per file"
                },
                "hidden": {
                    "type": "boolean",
                    "description": "Also search hidden files and directories (default false)"
                }
            }),
            &["pattern"],
        ),
        move |_id, args, cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let pattern = arg_str(&args, "pattern")?;
                let ignore_case = args
                    .get("ignoreCase")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let regex = RegexBuilder::new(&pattern)
                    .case_insensitive(ignore_case)
                    .build()
                    .map_err(|e| format!("invalid regex `{pattern}`: {e}"))?;
                let mode = match args.get("outputMode").and_then(|v| v.as_str()) {
                    None | Some("content") => GrepMode::Content,
                    Some("files") => GrepMode::Files,
                    Some("count") => GrepMode::Count,
                    Some(other) => {
                        return Err(format!(
                            "unknown outputMode `{other}`; use content, files or count"
                        ))
                    }
                };
                let walk = Walk::from_args(env.as_ref(), &args).await?;
                let glob = match args.get("glob").and_then(|v| v.as_str()) {
                    Some(glob) => Some(glob_matcher(&walk.root, glob)?),
                    None => None,
                };
                let search = tokio::task::spawn_blocking(move || {
                    grep_files(&walk, &regex, mode, cancel.as_ref(), glob.as_ref())
                })
                .await
                .map_err(|e| e.to_string())?;
                let matches = search.matches;
                Ok(text_result(
                    search.lines,
                    search.truncated,
                    "No matches.",
                    json!({"matches": matches}),
                ))
            }
        },
    )
}

/// Create a `glob` tool bound to `env`.
pub fn create_glob_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "glob",
        "Glob",
        "Find files by name with a .gitignore-style glob. Respects .gitignore and skips \
         hidden files. Prefer this to running find through bash.",
        schema_object(
            json!({
                "pattern": {
                    "type": "string",
                    "description": "Glob to match, e.g. `*.rs` (at any depth), \
                                    `src/**/*.ts` or `Cargo.toml`; a pattern with a `/` is \
                                    relative to `path`"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search, relative to the working directory \
                                    (default: the working directory)"
                },
                "hidden": {
                    "type": "boolean",
                    "description": "Also match hidden files and directories (default false)"
                }
            }),
            &["pattern"],
        ),
        move |_id, args, cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let pattern = arg_str(&args, "pattern")?;
                let walk = Walk::from_args(env.as_ref(), &args).await?;
                let matcher = glob_matcher(&walk.root, &pattern)?;
                let (lines, truncated) = tokio::task::spawn_blocking(move || {
                    let mut lines = Vec::new();
                    let mut truncated = false;
                    for path in walk.files(cancel.as_ref()) {
                        if !matches_glob(&matcher, &walk.root, &path) {
                            continue;
                        }
                        if lines.len() == SEARCH_MAX_RESULTS {
                            truncated = true;
                            break;
                        }
                        lines.push(walk.display(&path));
                    }
                    lines.sort();
                    (lines, truncated)
                })
                .await
                .map_err(|e| e.to_string())?;
                let files = lines.len();
                Ok(text_result(
                    lines,
                    truncated,
                    "No files found.",
                    json!({"files": files}),
                ))
            }
        },
    )
}

/// Create an `ls` tool bound to `env`.
pub fn create_ls_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "ls",
        "List",
        "List one directory's entries, hidden ones included; directories end in `/`.",
        schema_object(
            json!({
                "path": {
                    "type": "string",
                    "description": "Directory to list, relative to the working directory \
                                    (default: the working directory)"
                }
            }),
            &[],
        ),
        move |_id, args, _cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let abs = env
                    .absolute_path(Path::new(path))
                    .map_err(|e| e.to_string())?;
                let info = env.file_info(&abs).await.map_err(|e| e.to_string())?;
                if !info.is_dir {
                    return Err(format!("not a directory: {}", abs.display()));
                }
                let mut lines: Vec<String> = env
                    .list_dir(&abs)
                    .await
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|entry| {
                        let name = entry
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if entry.is_dir {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                    .collect();
                lines.sort();
                let truncated = lines.len() > SEARCH_MAX_RESULTS;
                lines.truncate(SEARCH_MAX_RESULTS);
                Ok(text_result(
                    lines,
                    truncated,
                    "Empty directory.",
                    json!({"path": abs}),
                ))
            }
        },
    )
}

/// What `grep` prints for each match.
#[derive(Debug, Clone, Copy)]
enum GrepMode {
    Content,
    Files,
    Count,
}

/// Where `grep` and `glob` look, from a call's `path` and `hidden`.
struct Walk {
    /// The resolved file or directory to search.
    root: PathBuf,
    /// The working directory, which shown paths are relative to.
    cwd: PathBuf,
    hidden: bool,
}

impl Walk {
    async fn from_args(env: &dyn ExecutionEnv, args: &serde_json::Value) -> Result<Self, String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let root = env
            .absolute_path(Path::new(path))
            .map_err(|e| e.to_string())?;
        if !env.exists(&root).await.map_err(|e| e.to_string())? {
            return Err(format!("no such file or directory: {path}"));
        }
        Ok(Self {
            root,
            cwd: env.cwd().to_path_buf(),
            hidden: args
                .get("hidden")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    /// The files under the root, in walk order, until `cancel` fires.
    fn files<'a>(
        &'a self,
        cancel: Option<&'a CancellationToken>,
    ) -> impl Iterator<Item = PathBuf> + 'a {
        WalkBuilder::new(&self.root)
            .hidden(!self.hidden)
            .require_git(false)
            .sort_by_file_path(|a, b| a.cmp(b))
            .filter_entry(|entry| entry.file_name() != ".git")
            .build()
            .take_while(move |_| !cancel.is_some_and(|c| c.is_cancelled()))
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
            .map(ignore::DirEntry::into_path)
    }

    /// `path` relative to the working directory when inside it, with `/`
    /// separators.
    fn display(&self, path: &Path) -> String {
        let shown = path.strip_prefix(&self.cwd).unwrap_or(path);
        shown.to_string_lossy().replace('\\', "/")
    }
}

/// A matcher for one glob, relative to `root`.
fn glob_matcher(root: &Path, pattern: &str) -> Result<Gitignore, String> {
    let mut builder = GitignoreBuilder::new(root);
    builder
        .add_line(None, pattern)
        .map_err(|e| format!("invalid glob `{pattern}`: {e}"))?;
    builder
        .build()
        .map_err(|e| format!("invalid glob `{pattern}`: {e}"))
}

fn matches_glob(matcher: &Gitignore, root: &Path, path: &Path) -> bool {
    // A file root (grep on one file) always matches.
    path == root || matcher.matched_path_or_any_parents(path, false).is_ignore()
}

struct GrepOutput {
    lines: Vec<String>,
    matches: usize,
    truncated: bool,
}

fn grep_files(
    walk: &Walk,
    regex: &regex::Regex,
    mode: GrepMode,
    cancel: Option<&CancellationToken>,
    glob: Option<&Gitignore>,
) -> GrepOutput {
    let mut out = GrepOutput {
        lines: Vec::new(),
        matches: 0,
        truncated: false,
    };
    'files: for path in walk.files(cancel) {
        if glob.is_some_and(|g| !matches_glob(g, &walk.root, &path)) {
            continue;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        let shown = walk.display(&path);
        let mut count = 0;
        for (index, line) in text.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            count += 1;
            out.matches += 1;
            match mode {
                GrepMode::Content => {
                    if out.lines.len() == SEARCH_MAX_RESULTS {
                        out.truncated = true;
                        break 'files;
                    }
                    let (line, _) = truncate_head(line, GREP_MAX_LINE_LENGTH);
                    out.lines.push(format!("{shown}:{}:{line}", index + 1));
                }
                GrepMode::Files => break,
                GrepMode::Count => {}
            }
        }
        if count > 0 && !matches!(mode, GrepMode::Content) {
            if out.lines.len() == SEARCH_MAX_RESULTS {
                out.truncated = true;
                break;
            }
            out.lines.push(match mode {
                GrepMode::Count => format!("{shown}:{count}"),
                _ => shown,
            });
        }
    }
    out
}

/// A file's text, or `None` for binary files and files that cannot be read.
fn read_text(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0; BINARY_SNIFF_BYTES];
    let read = file.read(&mut head).ok()?;
    if head[..read].contains(&0) {
        return None;
    }
    head.truncate(read);
    file.read_to_end(&mut head).ok()?;
    Some(String::from_utf8_lossy(&head).into_owned())
}

/// The tool result for `lines`, with a note when they were cut short.
fn text_result(
    lines: Vec<String>,
    truncated: bool,
    empty: &str,
    mut details: serde_json::Value,
) -> AgentToolResult {
    let text = if lines.is_empty() {
        empty.to_owned()
    } else {
        lines.join("\n")
    };
    let (mut text, cut) = truncate_head(&text, DEFAULT_MAX_BYTES);
    let truncated = truncated || cut;
    if truncated {
        text.push_str(&format!(
            "\n[truncated: showing the first {} lines; narrow the pattern, path or glob to \
             see the rest]",
            text.lines().count()
        ));
    }
    details["truncated"] = json!(truncated);
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text,
            text_signature: None,
        })],
        details,
        usage: None,
        added_tool_names: None,
        terminate: None,
    }
}
