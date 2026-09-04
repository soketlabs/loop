//! Minimal default system prompt (Loop by Soket AI).

use std::path::{Path, PathBuf};

use crate::config::paths::{
    append_system_md_path, get_agent_dir, get_project_dir, system_md_path,
};

/// Context file candidate names (first match wins per directory).
const CONTEXT_NAMES: &[&str] = &["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

/// A loaded project context file.
#[derive(Debug, Clone)]
pub struct ContextFile {
    /// Absolute path.
    pub path: PathBuf,
    /// File contents.
    pub content: String,
}

/// Options for building the system prompt.
pub struct BuildSystemPromptOptions<'a> {
    /// Custom replace prompt.
    pub custom_prompt: Option<&'a str>,
    /// Append text.
    pub append_system_prompt: Option<&'a str>,
    /// Working directory.
    pub cwd: &'a Path,
    /// Tool names selected.
    pub selected_tools: &'a [&'a str],
    /// One-line snippets.
    pub tool_snippets: &'a [(&'a str, &'a str)],
    /// Context files.
    pub context_files: &'a [ContextFile],
}

/// Default tool snippets.
pub fn default_tool_snippets() -> Vec<(&'static str, &'static str)> {
    vec![
        ("read", "Read file contents"),
        ("bash", "Run shell commands"),
        ("edit", "Apply exact string replacements in a file"),
        ("write", "Create or overwrite a file"),
    ]
}

/// Build the system prompt.
///
/// Skills are not embedded here: `AgentHarness` appends the pi-style
/// `<available_skills>` block each turn (with paths) when `read` is available.
pub fn build_system_prompt(opts: BuildSystemPromptOptions<'_>) -> String {
    let cwd = opts.cwd.display().to_string().replace('\\', "/");
    let append = opts
        .append_system_prompt
        .map(|s| format!("\n\n{s}"))
        .unwrap_or_default();

    let mut prompt = if let Some(custom) = opts.custom_prompt {
        custom.to_string()
    } else {
        let tools_list = opts
            .tool_snippets
            .iter()
            .filter(|(name, _)| opts.selected_tools.contains(name))
            .map(|(n, s)| format!("- {n}: {s}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tools_list = if tools_list.is_empty() {
            "(none)".into()
        } else {
            tools_list
        };

        let mut guidelines = vec![
            "Be concise and direct. Prefer action over lengthy explanation.".to_string(),
            "Use tools to inspect the codebase before making changes.".to_string(),
        ];
        if opts.selected_tools.contains(&"bash")
            && !opts.selected_tools.contains(&"grep")
            && !opts.selected_tools.contains(&"find")
        {
            guidelines.push(
                "Use bash (rg/fd/find/grep) to explore when specialized search tools are unavailable."
                    .into(),
            );
        }

        format!(
            "You are an expert coding assistant in Loop, created by Soket AI.\n\n\
             # Available tools\n{tools_list}\n\n\
             # Guidelines\n{}\n",
            guidelines
                .iter()
                .map(|g| format!("- {g}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    prompt.push_str(&append);

    if !opts.context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for cf in opts.context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                cf.path.display(),
                cf.content
            ));
        }
        prompt.push_str("</project_context>\n");
    }

    prompt.push_str(&format!("\nCurrent working directory: {cwd}"));
    prompt
}

/// Resolve SYSTEM.md / APPEND_SYSTEM.md (project trusted > agent dir > CLI).
pub fn resolve_system_prompt_files(
    cwd: &Path,
    project_trusted: bool,
    cli_system: Option<&str>,
    cli_append: Option<&str>,
) -> (Option<String>, Option<String>) {
    let agent_dir = get_agent_dir();
    let mut custom = cli_system.map(|s| s.to_string());
    let mut append = cli_append.map(|s| s.to_string());

    let agent_sys = system_md_path(&agent_dir);
    if custom.is_none() && agent_sys.exists() {
        custom = std::fs::read_to_string(agent_sys).ok();
    }
    let agent_append = append_system_md_path(&agent_dir);
    if let Ok(extra) = std::fs::read_to_string(agent_append) {
        append = Some(match append {
            Some(a) => format!("{a}\n\n{extra}"),
            None => extra,
        });
    }

    if project_trusted {
        let project = get_project_dir(cwd);
        let p_sys = system_md_path(&project);
        if p_sys.exists() {
            if let Ok(s) = std::fs::read_to_string(p_sys) {
                custom = Some(s);
            }
        }
        let p_append = append_system_md_path(&project);
        if let Ok(extra) = std::fs::read_to_string(p_append) {
            append = Some(match append {
                Some(a) => format!("{a}\n\n{extra}"),
                None => extra,
            });
        }
    }

    (custom, append)
}

/// Load AGENTS.md / CLAUDE.md from agent dir + ancestors of cwd.
pub fn load_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    if let Some(cf) = read_context_in_dir(agent_dir) {
        files.push(cf);
    }

    let mut dir = cwd.to_path_buf();
    let mut seen = std::collections::HashSet::new();
    loop {
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            if let Some(cf) = read_context_in_dir(&dir) {
                files.push(cf);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    files
}

fn read_context_in_dir(dir: &Path) -> Option<ContextFile> {
    for name in CONTEXT_NAMES {
        let path = dir.join(name);
        if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                return Some(ContextFile { path, content });
            }
        }
    }
    None
}
