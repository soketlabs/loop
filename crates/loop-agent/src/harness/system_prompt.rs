//! System prompt helpers for skills (pi / Agent Skills progressive disclosure).

use crate::harness::types::Skill;

/// Format skills for inclusion in a system prompt (skips disable_model_invocation).
///
/// Matches pi's `formatSkillsForPrompt`: XML catalog with absolute paths so the
/// model can `read` a skill's `SKILL.md` when the task matches its description.
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks."
            .to_string(),
        "Use the read tool to load a skill's file when the task matches its description."
            .to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands."
            .to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];

    for skill in visible {
        let description = if skill.description.is_empty() {
            "(no description)"
        } else {
            skill.description.as_str()
        };
        let location = skill.path.display().to_string().replace('\\', "/");
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&location)
        ));
        lines.push("  </skill>".to_string());
    }

    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
