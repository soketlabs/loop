//! The sessions `/sessions` offers: Loop's own and subscription ones opened
//! in this folder, most recently used first.

use std::path::{Path, PathBuf};

use loop_agent::harness::session::SessionSummary;

use crate::agents::AgentSessionRecord;

/// Longest title shown, in characters.
const TITLE_CHARS: usize = 72;

/// One session in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChoice {
    /// Session id (`--resume` takes it).
    pub id: String,
    /// The `/name` if set, else the first prompt.
    pub title: String,
    /// What ran it: `provider/model` for Loop, the subscription id otherwise.
    pub source: Option<String>,
    /// Messages, when known (Loop's own sessions).
    pub message_count: Option<usize>,
    /// Last used (unix ms).
    pub updated_at: i64,
    /// Set for a subscription session, which resumes through its agent.
    pub agent: Option<AgentSessionRecord>,
}

/// `text` on one line, shortened to [`TITLE_CHARS`]; `None` when blank.
pub fn short_title(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= TITLE_CHARS {
        return Some(line);
    }
    let mut short: String = line.chars().take(TITLE_CHARS - 1).collect();
    short.push('…');
    Some(short)
}

/// The extension of the session files `/export` writes and `/import` reads.
pub const SESSION_FILE_EXTENSION: &str = ".loop.jsonl";

/// Where `/export [path]` writes the Markdown transcript and the session
/// file: side by side, named after `path` (relative to `cwd`) without its
/// extension, or `loop-<id>` in `cwd`.
pub fn export_paths(cwd: &Path, id: &str, path: Option<&str>) -> (PathBuf, PathBuf) {
    let stem = match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => {
            let path = path
                .strip_suffix(SESSION_FILE_EXTENSION)
                .or_else(|| path.strip_suffix(".jsonl"))
                .or_else(|| path.strip_suffix(".md"))
                .unwrap_or(path);
            cwd.join(path)
        }
        None => cwd.join(format!("loop-{id}")),
    };
    let with = |ext: &str| {
        let mut name = stem.clone().into_os_string();
        name.push(ext);
        PathBuf::from(name)
    };
    (with(".md"), with(SESSION_FILE_EXTENSION))
}

/// Loop's sessions and subscription sessions in one list, newest first.
pub fn merge(native: Vec<SessionSummary>, agents: Vec<AgentSessionRecord>) -> Vec<SessionChoice> {
    let mut out: Vec<SessionChoice> = native
        .into_iter()
        .map(|s| SessionChoice {
            title: s.name.or(s.title).unwrap_or_else(|| "(no title)".into()),
            id: s.id,
            source: s.model,
            message_count: Some(s.message_count),
            updated_at: s.updated_at,
            agent: None,
        })
        .chain(agents.into_iter().map(|r| SessionChoice {
            id: r.id.clone(),
            title: r.title.clone().unwrap_or_else(|| "(no title)".into()),
            source: Some(r.agent.clone()),
            message_count: None,
            updated_at: r.updated_at.unwrap_or(r.created_at),
            agent: Some(r),
        }))
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    out
}

/// How long ago `then_ms` was, as `just now`, `5m ago`, `3h ago` or `2d ago`.
pub fn ago(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, updated_at: i64, name: Option<&str>) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            cwd: Some("/w".into()),
            name: name.map(str::to_owned),
            title: Some(format!("prompt {id}")),
            model: Some("soket/qwen".into()),
            message_count: 4,
            created_at: 0,
            updated_at,
        }
    }

    #[test]
    fn titles_are_one_short_line() {
        assert_eq!(
            short_title("  fix\n\tthe  bug "),
            Some("fix the bug".into())
        );
        assert_eq!(short_title(" \n "), None);
        let long = short_title(&"x".repeat(200)).unwrap();
        assert_eq!(long.chars().count(), TITLE_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn loop_and_subscription_sessions_merge_newest_first() {
        let agent = AgentSessionRecord {
            id: "a1".into(),
            agent: "chatgpt".into(),
            cwd: "/w".into(),
            created_at: 10,
            title: Some("ask codex".into()),
            updated_at: Some(50),
        };
        let merged = merge(
            vec![summary("n1", 20, None), summary("n2", 90, Some("named"))],
            vec![agent],
        );
        let ids: Vec<_> = merged.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["n2", "a1", "n1"]);
        assert_eq!(merged[0].title, "named", "a /name beats the first prompt");
        assert_eq!(merged[1].source.as_deref(), Some("chatgpt"));
        assert!(merged[1].agent.is_some());
        assert_eq!(merged[2].title, "prompt n1");
        assert_eq!(merged[2].message_count, Some(4));
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(ago(10_000, 9_000), "just now");
        assert_eq!(ago(10 * 60_000, 0), "10m ago");
        assert_eq!(ago(3 * 3_600_000, 0), "3h ago");
        assert_eq!(ago(2 * 86_400_000, 0), "2d ago");
        assert_eq!(ago(0, 5_000), "just now", "a clock skew is not negative");
    }

    #[test]
    fn export_writes_a_transcript_and_a_session_file_side_by_side() {
        let cwd = std::path::Path::new("/work");
        let paths = |arg: Option<&str>| {
            let (md, file) = export_paths(cwd, "01a18bf7-9c86-7d50", arg);
            (
                md.to_string_lossy().replace('\\', "/"),
                file.to_string_lossy().replace('\\', "/"),
            )
        };
        assert_eq!(
            paths(None),
            (
                "/work/loop-01a18bf7-9c86-7d50.md".into(),
                "/work/loop-01a18bf7-9c86-7d50.loop.jsonl".into()
            )
        );
        assert_eq!(
            paths(Some("notes/review.md")),
            (
                "/work/notes/review.md".into(),
                "/work/notes/review.loop.jsonl".into()
            )
        );
        assert_eq!(
            paths(Some("review.loop.jsonl")),
            ("/work/review.md".into(), "/work/review.loop.jsonl".into())
        );
        assert_eq!(
            paths(Some("review")),
            ("/work/review.md".into(), "/work/review.loop.jsonl".into())
        );
    }
}
