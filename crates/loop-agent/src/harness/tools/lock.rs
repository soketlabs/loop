//! Which tools a Runtime tool lock ([`ToolLock`]) offers.
//!
//! The lock is an allowlist by exact tool name, so it fails closed: a tool
//! that is not named here (a new built-in tool, an MCP tool, the Runtime's
//! `subagent` tool) is removed. The permission gate turns the same list into
//! a Policy layer (`loop_app_core::permissions::tool_lock`).

pub use loop_protocol::ToolLock;

/// Tools the `chat` lock offers: reading and searching files, and the web.
pub const CHAT_TOOLS: &[&str] = &["read", "grep", "glob", "ls", "web_fetch", "web_search"];

/// Every tool Loop registers itself, for the classification test and the
/// docs. MCP tools (`mcp__<server>__<tool>`) are not listed: a lock removes
/// all of them.
pub const KNOWN_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "ls",
    "web_fetch",
    "web_search",
    "write",
    "edit",
    "bash",
    "todo",
    "exit_plan",
    "subagent",
    "spawn_task",
    "memory_read",
    "memory_write",
    "memory_list",
];

/// What a lock does with one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// Offered: it only reads files or the web.
    Offered,
    /// Removed from the toolset and refused if called; the reason says why.
    Removed(&'static str),
}

/// What `lock` does with `tool`.
pub fn classify(lock: ToolLock, tool: &str) -> ToolClass {
    let offered = match lock {
        ToolLock::Chat => CHAT_TOOLS,
        // A lock this build does not know offers nothing.
        ToolLock::Unknown => &[],
    };
    if offered.contains(&tool) {
        return ToolClass::Offered;
    }
    ToolClass::Removed(match tool {
        "bash" => "runs commands",
        "write" => "writes files, documents included",
        "edit" => "changes files",
        "subagent" => "starts a Subagent Session",
        "spawn_task" | "memory_read" | "memory_write" | "memory_list" => "workflow tool",
        "todo" | "exit_plan" => "not needed in chat",
        _ if tool.starts_with("mcp__") => "MCP tool",
        _ => "not on the lock's list",
    })
}

/// Whether `lock` offers `tool`.
pub fn offers(lock: ToolLock, tool: &str) -> bool {
    classify(lock, tool) == ToolClass::Offered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_tool_is_classified() {
        let rows: &[(&str, bool)] = &[
            ("read", true),
            ("grep", true),
            ("glob", true),
            ("ls", true),
            ("web_fetch", true),
            ("web_search", true),
            ("write", false),
            ("edit", false),
            ("bash", false),
            ("todo", false),
            ("exit_plan", false),
            ("subagent", false),
            ("spawn_task", false),
            ("memory_read", false),
            ("memory_write", false),
            ("memory_list", false),
        ];
        assert_eq!(rows.len(), KNOWN_TOOLS.len());
        for (tool, offered) in rows {
            assert!(KNOWN_TOOLS.contains(tool), "{tool}");
            assert_eq!(offers(ToolLock::Chat, tool), *offered, "{tool}");
        }
    }

    #[test]
    fn unknown_tools_fail_closed() {
        for tool in [
            "mcp__fs__read_file",
            "mcp__github__create_issue",
            "fetch",
            "find",
            "delete",
            "Read",
            "read ",
            "",
        ] {
            assert!(!offers(ToolLock::Chat, tool), "{tool}");
        }
        assert!(matches!(
            classify(ToolLock::Chat, "mcp__fs__read_file"),
            ToolClass::Removed("MCP tool")
        ));
        for tool in CHAT_TOOLS {
            assert!(!offers(ToolLock::Unknown, tool), "{tool}");
        }
    }
}
