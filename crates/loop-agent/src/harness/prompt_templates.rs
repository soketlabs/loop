//! Prompt template loading and argument substitution.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use crate::harness::types::{FileError, FileErrorCode, PromptTemplate};

/// Template load diagnostic.
#[derive(Debug, Clone)]
pub struct PromptTemplateDiagnostic {
    /// Path.
    pub path: PathBuf,
    /// Message.
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct TemplateFrontmatter {
    name: Option<String>,
    #[serde(default, rename = "argument-hint")]
    argument_hint: Option<String>,
}

/// Load `.md` templates non-recursively from a directory.
pub fn load_prompt_templates(dir: &Path) -> (Vec<PromptTemplate>, Vec<PromptTemplateDiagnostic>) {
    let mut templates = Vec::new();
    let mut diags = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            diags.push(PromptTemplateDiagnostic {
                path: dir.to_path_buf(),
                message: e.to_string(),
            });
            return (templates, diags);
        }
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        match load_template_file(&path) {
            Ok(t) => templates.push(t),
            Err(e) => diags.push(PromptTemplateDiagnostic {
                path,
                message: e.to_string(),
            }),
        }
    }
    (templates, diags)
}

fn load_template_file(path: &Path) -> Result<PromptTemplate, FileError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| FileError::new(FileErrorCode::Io, e.to_string()))?;
    let (fm, body) = split_frontmatter(&text);
    let fm: TemplateFrontmatter = serde_yaml::from_str(&fm).unwrap_or(TemplateFrontmatter {
        name: None,
        argument_hint: None,
    });
    let name = fm.name.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("template")
            .to_string()
    });
    Ok(PromptTemplate {
        name,
        body: body.trim().to_string(),
        path: path.to_path_buf(),
        argument_hint: fm.argument_hint,
    })
}

fn split_frontmatter(text: &str) -> (String, String) {
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            return (
                rest[..end].trim().to_string(),
                rest[end + 4..].to_string(),
            );
        }
    }
    (String::new(), text.to_string())
}

/// Quote-aware command arg parse.
pub fn parse_command_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quotes: Option<char> = None;
    for c in input.chars() {
        if let Some(q) = in_quotes {
            if c == q {
                in_quotes = None;
            } else {
                cur.push(c);
            }
        } else if c == '"' || c == '\'' {
            in_quotes = Some(c);
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                args.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        args.push(cur);
    }
    args
}

/// Substitute `$1`, `$@`, `$ARGUMENTS`, `${@:N}`, `${@:N:L}`.
pub fn substitute_args(template: &str, args: &[String]) -> String {
    let mut out = template.to_string();
    out = out.replace("$ARGUMENTS", &args.join(" "));
    out = out.replace("$@", &args.join(" "));
    let re_slice = Regex::new(r"\$\{@:(\d+)(?::(\d+))?\}").unwrap();
    out = re_slice
        .replace_all(&out, |caps: &regex::Captures| {
            let start: usize = caps[1].parse().unwrap_or(1);
            let start_idx = start.saturating_sub(1);
            if let Some(len_s) = caps.get(2) {
                let len: usize = len_s.as_str().parse().unwrap_or(0);
                args
                    .get(start_idx..start_idx.saturating_add(len).min(args.len()))
                    .map(|s| s.join(" "))
                    .unwrap_or_default()
            } else {
                args.get(start_idx..).map(|s| s.join(" ")).unwrap_or_default()
            }
        })
        .into_owned();
    for (i, arg) in args.iter().enumerate() {
        out = out.replace(&format!("${}", i + 1), arg);
    }
    out
}

/// Format template invocation.
pub fn format_prompt_template_invocation(template: &PromptTemplate, args: &str) -> String {
    let parsed = parse_command_args(args);
    substitute_args(&template.body, &parsed)
}
