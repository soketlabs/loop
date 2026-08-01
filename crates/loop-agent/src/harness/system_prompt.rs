//! System prompt helpers for skills.

use crate::harness::types::Skill;

/// Format skills for inclusion in a system prompt (skips disable_model_invocation).
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String {
    let mut out = String::from("Available skills:\n");
    for skill in skills.iter().filter(|s| !s.disable_model_invocation) {
        out.push_str(&format!(
            "- {} — {}\n",
            skill.name,
            if skill.description.is_empty() {
                "(no description)"
            } else {
                &skill.description
            }
        ));
    }
    out
}
