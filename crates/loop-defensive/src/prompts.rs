//! The `/patch` and `/audit` prompt templates (installed into Loop's prompts
//! directory by `loop-defensive install`), and the prompts headless runs send.
//!
//! Loop names a server's tools `mcp__<server>__<tool>`; the servers are
//! registered as `defensive` (`/patch`) and `defensive-audit` (`/audit`).

/// `patch.md` for Loop's prompts directory.
pub const PATCH_TEMPLATE: &str = concat!(
    "---\nname: patch\nargument-hint: <CVE-ID | github-url | /path/to/local/repo>\n---\n",
    include_str!("prompts/patch.md")
);

/// `audit.md` for Loop's prompts directory.
pub const AUDIT_TEMPLATE: &str = concat!(
    "---\nname: audit\nargument-hint: <github-url>\n---\n",
    include_str!("prompts/audit.md")
);

/// The `/patch <target>` prompt.
pub fn patch(target: &str) -> String {
    include_str!("prompts/patch.md").replace("$1", target.trim())
}

/// The `/audit <url>` prompt.
pub fn audit(url: &str) -> String {
    include_str!("prompts/audit.md").replace("$1", url.trim())
}

#[cfg(test)]
mod tests {
    #[test]
    fn prompts_fill_the_target_and_name_the_mcp_tools() {
        let p = super::patch(" https://github.com/o/r ");
        assert!(p.starts_with("Run the defensive workflow for https://github.com/o/r "));
        assert!(!p.contains("$1"));
        assert!(p.contains("`mcp__defensive__prepare_task`") && p.contains("`mcp__defensive__bash`"));
        let a = super::audit("https://github.com/o/r");
        assert!(!a.contains("$1") && a.contains("`mcp__defensive-audit__prepare_audit`"));
        assert!(super::PATCH_TEMPLATE.starts_with("---\nname: patch\n"));
        assert!(super::AUDIT_TEMPLATE.contains("argument-hint: <github-url>"));
    }
}
