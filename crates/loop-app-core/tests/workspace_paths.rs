//! Context files, skills, templates and system prompt files of a Workspace
//! load through its execution env; the global ones stay host reads.
//!
//! The env ([`VirtualEnv`], shared with `loop-agent` by `#[path]`) shows a
//! root that is not on the host, so a `std::fs` read of a project path fails.

#[path = "../../loop-agent/tests/support/virtual_env.rs"]
mod virtual_env;

use loop_agent::harness::HostExecutionEnv;
use loop_app_core::config::Settings;
use loop_app_core::resources::{load_resources, load_resources_in};
use loop_app_core::system_prompt::{
    load_context_files, load_context_files_in, resolve_system_prompt_files_in,
    resolve_system_prompt_files_via,
};
use virtual_env::{VirtualEnv, VIRTUAL_ROOT};

fn write(path: std::path::PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn skill(dir: std::path::PathBuf, name: &str) {
    write(
        dir.join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {name} skill\n---\nbody"),
    );
}

#[tokio::test]
async fn context_files_of_the_workspace_load_through_the_env() {
    let agent = tempfile::tempdir().unwrap();
    write(agent.path().join("AGENTS.md"), "fixture: global");
    let ws = tempfile::tempdir().unwrap();
    write(ws.path().join("AGENTS.md"), "fixture: root agents");
    write(ws.path().join("sub/AGENTS.md"), "fixture: agents");
    write(ws.path().join("sub/CLAUDE.local.md"), "fixture: local");
    write(ws.path().join("sub/.claude/other"), "x");
    #[cfg(unix)]
    {
        // CLAUDE.md is a link to AGENTS.md: it must load only once, so the
        // loader must resolve the link through the env.
        std::os::unix::fs::symlink(
            ws.path().join("sub/AGENTS.md"),
            ws.path().join("sub/CLAUDE.md"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            ws.path().join("sub/AGENTS.md"),
            ws.path().join("sub/.claude/CLAUDE.md"),
        )
        .unwrap();
    }
    let env = VirtualEnv::new(ws.path());
    let cwd = env.root().join("sub");

    let files = load_context_files_in(&env, &cwd, agent.path()).await;
    let fixture: Vec<_> = files
        .iter()
        .filter(|f| f.content.starts_with("fixture:"))
        .map(|f| (f.path.clone(), f.content.clone()))
        .collect();
    assert_eq!(
        fixture,
        [
            // The global file is a host read, at its host path.
            (agent.path().join("AGENTS.md"), "fixture: global".into()),
            // Workspace files keep the Workspace's own spelling.
            (cwd.join("AGENTS.md"), "fixture: agents".into()),
            (cwd.join("CLAUDE.local.md"), "fixture: local".into()),
            (env.root().join("AGENTS.md"), "fixture: root agents".into()),
        ]
    );
    assert!(env.called("file_info") && env.called("read_text_file"));
    assert!(env.called("canonical_path"));
    env.assert_no_leaks();
}

/// On a host folder, the loader through the env gives what the host loader
/// gives.
#[tokio::test]
async fn the_env_context_loader_matches_the_host_loader() {
    let agent = tempfile::tempdir().unwrap();
    write(agent.path().join("AGENTS.md"), "fixture: global");
    let ws = tempfile::tempdir().unwrap();
    let base = ws.path().canonicalize().unwrap();
    write(base.join("AGENTS.md"), "fixture: root agents");
    write(base.join("sub/AGENTS.md"), "fixture: agents");
    write(base.join("sub/CLAUDE.md"), "fixture: claude");
    write(base.join("sub/CLAUDE.local.md"), "fixture: local");
    #[cfg(unix)]
    std::os::unix::fs::symlink(base.join("sub/CLAUDE.md"), {
        std::fs::create_dir_all(base.join("sub/.claude")).unwrap();
        base.join("sub/.claude/CLAUDE.md")
    })
    .unwrap();
    let cwd = base.join("sub");
    let host = load_context_files(&cwd, agent.path());
    let env = HostExecutionEnv::new(&base);
    let through = load_context_files_in(&env, &cwd, agent.path()).await;
    let key = |files: &[loop_app_core::system_prompt::ContextFile]| {
        files
            .iter()
            .map(|f| (f.path.clone(), f.content.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(key(&through), key(&host));
}

#[tokio::test]
async fn skills_and_templates_of_the_project_load_through_the_env() {
    let agent = tempfile::tempdir().unwrap();
    skill(agent.path().join("skills/gskill"), "wp-global");
    write(agent.path().join("prompts/gp.md"), "global prompt");
    let ws = tempfile::tempdir().unwrap();
    skill(ws.path().join("sub/.loop/skills/pskill"), "wp-project");
    skill(ws.path().join("sub/.claude/skills/cskill"), "wp-claude");
    skill(ws.path().join(".agents/skills/askill"), "wp-agents");
    write(ws.path().join("sub/.loop/prompts/pp.md"), "project prompt");
    let env = VirtualEnv::new(ws.path());
    let cwd = env.root().join("sub");
    let settings = Settings::default();

    let names = |r: &loop_app_core::resources::LoadedResources| {
        let mut n: Vec<_> = r
            .skills
            .iter()
            .map(|s| s.name.clone())
            .filter(|n| n.starts_with("wp-"))
            .collect();
        n.sort();
        n
    };

    // Trusted: project folders load through the env, global ones from the host.
    let res = load_resources_in(&env, agent.path(), &cwd, true, &settings).await;
    assert_eq!(
        names(&res),
        ["wp-agents", "wp-claude", "wp-global", "wp-project"]
    );
    let project = res.skills.iter().find(|s| s.name == "wp-project").unwrap();
    assert_eq!(project.path, cwd.join(".loop/skills/pskill/SKILL.md"));
    let global = res.skills.iter().find(|s| s.name == "wp-global").unwrap();
    assert!(global.path.starts_with(agent.path()));
    let mut prompts: Vec<_> = res.prompts.iter().map(|p| p.name.as_str()).collect();
    prompts.sort();
    assert_eq!(prompts, ["gp", "pp"]);
    assert!(env.called("list_dir") && env.called("read_text_file"));
    env.assert_no_leaks();

    // Untrusted (and a tool-locked Runtime, which passes false): no project
    // folder is read through the env at all.
    let env = VirtualEnv::new(ws.path());
    let res = load_resources_in(&env, agent.path(), &cwd, false, &settings).await;
    assert_eq!(names(&res), ["wp-global"]);
    assert!(env.calls().is_empty(), "{:?}", env.calls());
}

/// On a host folder, loading through the env gives what the host loader
/// gives.
#[tokio::test]
async fn the_env_resource_loader_matches_the_host_loader() {
    let agent = tempfile::tempdir().unwrap();
    skill(agent.path().join("skills/g"), "wp-global");
    let ws = tempfile::tempdir().unwrap();
    let base = ws.path().canonicalize().unwrap();
    skill(base.join(".loop/skills/p"), "wp-project");
    skill(base.join("sub/.claude/skills/c"), "wp-claude");
    skill(base.join(".agents/skills/dup"), "wp-dup");
    skill(base.join(".claude/skills/dup"), "wp-dup");
    write(base.join(".loop/prompts/a.md"), "a");
    write(base.join(".loop/prompts/b.md"), "b");
    let cwd = base.join("sub");
    let settings = Settings {
        disabled_skills: vec!["wp-claude".into()],
        ..Settings::default()
    };
    let host = load_resources(agent.path(), &cwd, true, &settings);
    let env = HostExecutionEnv::new(&base);
    let through = load_resources_in(&env, agent.path(), &cwd, true, &settings).await;
    let skills = |r: &loop_app_core::resources::LoadedResources| {
        r.skills
            .iter()
            .chain(&r.disabled_skills)
            .map(|s| (s.name.clone(), s.path.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(skills(&through), skills(&host));
    let prompts = |r: &loop_app_core::resources::LoadedResources| {
        r.prompts
            .iter()
            .map(|p| (p.name.clone(), p.path.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(prompts(&through), prompts(&host));
    assert_eq!(through.extension_paths, host.extension_paths);
    assert_eq!(through.hook_paths, host.hook_paths);
}

#[tokio::test]
async fn project_system_prompt_files_load_through_the_env() {
    let agent = tempfile::tempdir().unwrap();
    write(agent.path().join("APPEND_SYSTEM.md"), "global append");
    let ws = tempfile::tempdir().unwrap();
    write(ws.path().join(".loop/SYSTEM.md"), "project system");
    write(ws.path().join(".loop/APPEND_SYSTEM.md"), "project append");
    let env = VirtualEnv::new(ws.path());

    let (custom, append) =
        resolve_system_prompt_files_via(&env, agent.path(), env.root(), true, None, None).await;
    assert_eq!(custom.as_deref(), Some("project system"));
    assert_eq!(append.as_deref(), Some("global append\n\nproject append"));
    // Same as the host function on a host folder.
    let host = resolve_system_prompt_files_in(agent.path(), ws.path(), true, None, None);
    assert_eq!((custom, append), host);
    env.assert_no_leaks();

    // Without trust the project files are not read.
    let env = VirtualEnv::new(ws.path());
    let (custom, append) =
        resolve_system_prompt_files_via(&env, agent.path(), env.root(), false, None, None).await;
    assert_eq!(custom, None);
    assert_eq!(append.as_deref(), Some("global append"));
    assert!(env.calls().is_empty(), "{:?}", env.calls());
    assert!(!std::path::Path::new(VIRTUAL_ROOT).exists());
}
