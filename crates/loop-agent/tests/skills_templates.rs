//! Skills and prompt template tests.

use std::fs;

use loop_agent::harness::prompt_templates::{
    parse_command_args, substitute_args,
};
use loop_agent::harness::skills::load_skills;
use loop_agent::harness::system_prompt::format_skills_for_system_prompt;

#[test]
fn substitute_args_matrix() {
    let args = parse_command_args(r#"one "two three" four"#);
    assert_eq!(args, vec!["one", "two three", "four"]);
    let out = substitute_args("A=$1 B=$@ C=$ARGUMENTS D=${@:2}", &args);
    assert!(out.contains("A=one"));
    assert!(out.contains("two three"));
}

#[test]
fn load_skill_and_format() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("myskill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: A demo skill\n---\n\nDo the thing.\n",
    )
    .unwrap();
    let (skills, diags) = load_skills(dir.path());
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "demo");
    let formatted = format_skills_for_system_prompt(&skills);
    assert!(formatted.contains("demo"));
}
