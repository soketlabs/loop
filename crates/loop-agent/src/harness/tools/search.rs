//! Read-only search tools: `grep`, `glob` and `ls`.
//!
//! Every directory walk, ignore rule and file read goes through [`ExecutionEnv`],
//! so filesystem confinement also applies to search. `grep` and `glob`
//! respect `.gitignore` (in a git repo or not) and skip hidden files unless
//! asked; `.git/` is never searched. A [`SearchFilter`] hides the files the
//! host's permission rules would not let the tool read.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::{Stream, StreamExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
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

/// Whether `tool` may read or list the file at an absolute path.
pub type SearchFilter = Arc<dyn Fn(&str, &Path) -> bool + Send + Sync>;

/// `grep`, `glob` and `ls` bound to `env`, hiding what `filter` refuses.
pub fn search_tools_with_filter(
    env: Arc<dyn ExecutionEnv>,
    filter: SearchFilter,
) -> [AgentTool; 3] {
    [
        grep_tool(Arc::clone(&env), Some(Arc::clone(&filter))),
        glob_tool(Arc::clone(&env), Some(Arc::clone(&filter))),
        ls_tool(env, Some(filter)),
    ]
}

/// Create a `grep` tool bound to `env`.
pub fn create_grep_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    grep_tool(env, None)
}

/// Create a `glob` tool bound to `env`.
pub fn create_glob_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    glob_tool(env, None)
}

/// Create an `ls` tool bound to `env`.
pub fn create_ls_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    ls_tool(env, None)
}

fn grep_tool(env: Arc<dyn ExecutionEnv>, filter: Option<SearchFilter>) -> AgentTool {
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
            let filter = filter.clone();
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
                let walk = Walk::from_args(Arc::clone(&env), &args, "grep", filter).await?;
                let glob = match args.get("glob").and_then(|v| v.as_str()) {
                    Some(glob) => Some(glob_matcher(&walk.root, glob)?),
                    None => None,
                };
                let search = grep_files(&walk, &regex, mode, cancel.as_ref(), glob.as_ref()).await;
                let hidden = walk.hidden_by_rules.load(Ordering::Relaxed);
                let matches = search.matches;
                Ok(text_result(
                    search.lines,
                    search.truncated,
                    hidden,
                    "No matches.",
                    json!({"matches": matches}),
                ))
            }
        },
    )
}

fn glob_tool(env: Arc<dyn ExecutionEnv>, filter: Option<SearchFilter>) -> AgentTool {
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
            let filter = filter.clone();
            async move {
                let pattern = arg_str(&args, "pattern")?;
                let walk = Walk::from_args(Arc::clone(&env), &args, "glob", filter).await?;
                let matcher = glob_matcher(&walk.root, &pattern)?;
                let mut lines = Vec::new();
                let mut truncated = false;
                let files = walk.files(cancel.as_ref());
                futures::pin_mut!(files);
                while let Some(path) = files.next().await {
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
                let hidden = walk.hidden_by_rules.load(Ordering::Relaxed);
                let files = lines.len();
                Ok(text_result(
                    lines,
                    truncated,
                    hidden,
                    "No files found.",
                    json!({"files": files}),
                ))
            }
        },
    )
}

fn ls_tool(env: Arc<dyn ExecutionEnv>, filter: Option<SearchFilter>) -> AgentTool {
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
            let filter = filter.clone();
            async move {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let abs = env
                    .absolute_path(Path::new(path))
                    .map_err(|e| e.to_string())?;
                let info = env.file_info(&abs).await.map_err(|e| e.to_string())?;
                if !info.is_dir {
                    return Err(format!("not a directory: {}", abs.display()));
                }
                let entries = env.list_dir(&abs).await.map_err(|e| e.to_string())?;
                let total = entries.len();
                let mut lines: Vec<String> = entries
                    .into_iter()
                    .filter(|entry| filter.as_ref().is_none_or(|f| f("ls", &entry.path)))
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
                let hidden = total - lines.len();
                lines.sort();
                let truncated = lines.len() > SEARCH_MAX_RESULTS;
                lines.truncate(SEARCH_MAX_RESULTS);
                Ok(text_result(
                    lines,
                    truncated,
                    hidden,
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
    /// The tool, for `filter`.
    tool: &'static str,
    filter: Option<SearchFilter>,
    /// Files `filter` hid.
    hidden_by_rules: AtomicUsize,
    env: Arc<dyn ExecutionEnv>,
}

impl Walk {
    async fn from_args(
        env: Arc<dyn ExecutionEnv>,
        args: &serde_json::Value,
        tool: &'static str,
        filter: Option<SearchFilter>,
    ) -> Result<Self, String> {
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
            tool,
            filter,
            hidden_by_rules: AtomicUsize::new(0),
            env,
        })
    }

    /// Whether the filter lets the tool read `path`; counts the ones it hides.
    fn allowed(&self, path: &Path) -> bool {
        let allowed = self.filter.as_ref().is_none_or(|f| f(self.tool, path));
        if !allowed {
            self.hidden_by_rules.fetch_add(1, Ordering::Relaxed);
        }
        allowed
    }

    /// The files under the root, in walk order, until `cancel` fires.
    fn files<'a>(
        &'a self,
        cancel: Option<&'a CancellationToken>,
    ) -> impl Stream<Item = PathBuf> + 'a {
        async_stream::stream! {
            let mut stack = vec![(self.root.clone(), Vec::<Arc<Gitignore>>::new())];
            let mut visited = HashSet::new();
            while let Some((path, mut ignores)) = stack.pop() {
                if cancel.is_some_and(|c| c.is_cancelled()) { break; }
                if !self.allowed(&path) { continue; }
                let Ok(info) = self.env.file_info(&path).await else { continue; };
                if info.is_file { yield path; continue; }
                if !info.is_dir { continue; }
                let Ok(canonical) = self.env.canonical_path(&path).await else { continue; };
                if !visited.insert(canonical) { continue; }
                // Read ignore rules through the capability too: the host walker
                // would otherwise read a symlinked .gitignore outside the jail.
                for name in [".gitignore", ".ignore"] {
                    // An ignore file the filter hides is not read either: under a
                    // tool lock it may link out of the folder.
                    let ignore = path.join(name);
                    if self.filter.as_ref().is_some_and(|f| !f(self.tool, &ignore)) { continue; }
                    if let Ok(text) = self.env.read_text_file(&ignore).await {
                        let mut builder = GitignoreBuilder::new(&path);
                        for line in text.lines() { let _ = builder.add_line(None, line); }
                        if let Ok(rules) = builder.build() { ignores.push(Arc::new(rules)); }
                    }
                }
                let Ok(mut entries) = self.env.list_dir(&path).await else { continue; };
                entries.sort_by(|a, b| b.path.cmp(&a.path));
                for entry in entries {
                    let name = entry.path.file_name().unwrap_or_default().to_string_lossy();
                    if name == ".git" || (!self.hidden && name.starts_with('.')) { continue; }
                    let matched = ignores.iter().rev().map(|rules| rules.matched_path_or_any_parents(&entry.path, entry.is_dir)).find(|m| !m.is_none());
                    if matched.is_some_and(|m| m.is_ignore()) { continue; }
                    stack.push((entry.path, ignores.clone()));
                }
            }
        }
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

async fn grep_files(
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
    let files = walk.files(cancel);
    futures::pin_mut!(files);
    'files: while let Some(path) = files.next().await {
        if glob.is_some_and(|g| !matches_glob(g, &walk.root, &path)) {
            continue;
        }
        let Some(text) = read_text(walk.env.as_ref(), &path).await else {
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
async fn read_text(env: &dyn ExecutionEnv, path: &Path) -> Option<String> {
    let bytes = env.read_binary_file(path).await.ok()?;
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The tool result for `lines`, with notes when they were cut short or
/// permission rules hid paths.
fn text_result(
    lines: Vec<String>,
    truncated: bool,
    hidden_by_rules: usize,
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
    if hidden_by_rules > 0 {
        text.push_str(&format!(
            "\n[{hidden_by_rules} paths hidden by permission rules]"
        ));
    }
    details["truncated"] = json!(truncated);
    details["hiddenByRules"] = json!(hidden_by_rules);
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
