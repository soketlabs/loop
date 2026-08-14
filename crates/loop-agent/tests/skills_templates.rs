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
    let formatted = format_skills_for_system_prompt(&skills, &[]);
    assert!(formatted.contains("Use the read tool to load a skill's file"));
    assert!(formatted.contains("<available_skills>"));
    assert!(formatted.contains("<name>demo</name>"));
    assert!(formatted.contains("<description>A demo skill</description>"));
    assert!(formatted.contains("<location>"));
    assert!(formatted.contains("SKILL.md</location>"));
    assert!(formatted.contains("</available_skills>"));

    let muted = loop_agent::harness::Skill {
        name: "hidden".into(),
        description: "secret".into(),
        body: String::new(),
        path: skill_dir.join("SKILL.md"),
        disable_model_invocation: true,
    };
    assert!(format_skills_for_system_prompt(&[muted.clone()], &[]).is_empty());
    let forced = format_skills_for_system_prompt(&[muted], &["hidden".into()]);
    assert!(forced.contains("<name>hidden</name>"));
    assert!(forced.contains("<description>secret</description>"));

    let escaped = loop_agent::harness::Skill {
        name: "amp".into(),
        description: "A & B <C>".into(),
        body: String::new(),
        path: skill_dir.join("SKILL.md"),
        disable_model_invocation: false,
    };
    let xml = format_skills_for_system_prompt(&[escaped], &[]);
    assert!(xml.contains("<description>A &amp; B &lt;C&gt;</description>"));
}
