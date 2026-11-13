//! Tools and loaders reach a Workspace only through its execution env.
//!
//! The env here ([`VirtualEnv`]) shows a root that does not exist on the
//! host, so a `std::fs` call on a Workspace path fails these tests.

#[path = "support/virtual_env.rs"]
mod virtual_env;

use std::path::Path;
use std::sync::Arc;

use loop_agent::harness::prompt_templates::{load_prompt_templates, load_prompt_templates_in};
use loop_agent::harness::skills::{host_global_gitignore, load_skills, load_skills_in};
use loop_agent::harness::{
    create_edit_tool, create_glob_tool, create_grep_tool, create_ls_tool, create_read_tool,
    create_write_tool, set_document_reader, DocumentReader, ExecutionEnv, HostExecutionEnv,
};
use loop_agent::AgentTool;
use serde_json::{json, Value};
use virtual_env::{VirtualEnv, VIRTUAL_ROOT};

async fn call(tool: &AgentTool, args: Value) -> String {
    let result = (tool.execute)("t".into(), args, None, None)
        .await
        .unwrap_or_else(|e| panic!("{} failed: {e}", tool.name));
    match &result.content[0] {
        loop_ai::ToolResultContent::Text(t) => t.text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn workspace() -> (tempfile::TempDir, Arc<VirtualEnv>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("notes.txt"), "alpha\nbeta\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "beta hidden\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {\n    beta();\n}\n").unwrap();
    std::fs::write(root.join("src/nested/lib.rs"), "pub fn beta() {}\n").unwrap();
    let env = Arc::new(VirtualEnv::new(root));
    (dir, env)
}

#[tokio::test]
async fn the_virtual_root_is_not_on_the_host() {
    assert!(!Path::new(VIRTUAL_ROOT).exists());
}

#[tokio::test]
async fn read_write_edit_work_through_the_env() {
    let (dir, env) = workspace();
    let dyn_env: Arc<dyn ExecutionEnv> = env.clone();

    let text = call(
        &create_read_tool(dyn_env.clone()),
        json!({"path": "notes.txt"}),
    )
    .await;
    assert!(text.contains("alpha"), "{text}");
    let abs = format!("{VIRTUAL_ROOT}/notes.txt");
    let text = call(&create_read_tool(dyn_env.clone()), json!({"path": abs})).await;
    assert!(text.contains("beta"), "{text}");

    let write = create_write_tool(dyn_env.clone());
    let text = call(&write, json!({"path": "out/new.txt", "content": "one\n"})).await;
    assert!(
        !text.contains(&dir.path().to_string_lossy().to_string()),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out/new.txt")).unwrap(),
        "one\n"
    );
    // An overwrite takes the path with a previous file (snapshot and diff).
    call(&write, json!({"path": "out/new.txt", "content": "two\n"})).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out/new.txt")).unwrap(),
        "two\n"
    );

    let edit = create_edit_tool(dyn_env.clone());
    call(
        &edit,
        json!({"path": "notes.txt", "oldText": "beta", "newText": "gamma"}),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        "alpha\ngamma\n"
    );

    assert!(env.called("read_text_file") || env.called("read_binary_file"));
    assert!(env.called("write_file"));
    env.assert_no_leaks();
}

#[tokio::test]
async fn grep_glob_and_ls_work_through_the_env() {
    let (_dir, env) = workspace();
    let dyn_env: Arc<dyn ExecutionEnv> = env.clone();

    let text = call(
        &create_grep_tool(dyn_env.clone()),
        json!({"pattern": "beta"}),
    )
    .await;
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            "notes.txt:2:beta",
            "src/main.rs:2:    beta();",
            "src/nested/lib.rs:1:pub fn beta() {}"
        ],
        "relative to the virtual root, ignored file skipped: {text}"
    );

    let text = call(
        &create_glob_tool(dyn_env.clone()),
        json!({"pattern": "*.rs"}),
    )
    .await;
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort();
    assert_eq!(lines, ["src/main.rs", "src/nested/lib.rs"], "{text}");

    let text = call(&create_ls_tool(dyn_env.clone()), json!({"path": "src"})).await;
    assert!(
        text.contains("main.rs") && text.contains("nested"),
        "{text}"
    );

    assert!(env.called("list_dir"));
    assert!(env.called("canonical_path"));
    env.assert_no_leaks();
}

#[tokio::test]
async fn a_document_is_read_and_written_through_the_env() {
    // The unit tests of the crate parse in process; this binary must say so.
    set_document_reader(DocumentReader::InProcess);
    let (dir, env) = workspace();
    let dyn_env: Arc<dyn ExecutionEnv> = env.clone();

    let write = create_write_tool(dyn_env.clone());
    call(
        &write,
        json!({"path": "report.docx", "content": "# Title\n\nSome body text.\n"}),
    )
    .await;
    assert!(dir.path().join("report.docx").is_file());
    // The second write diffs against the document read back through the reader.
    call(
        &write,
        json!({"path": "report.docx", "content": "# Title\n\nChanged body text.\n"}),
    )
    .await;

    let text = call(
        &create_read_tool(dyn_env.clone()),
        json!({"path": "report.docx"}),
    )
    .await;
    assert!(
        text.contains("Title") && text.contains("Changed body text"),
        "{text}"
    );
    // The reader-root check resolves the root through the env.
    assert!(
        env.calls()
            .iter()
            .any(|c| c == &format!("canonical_path {VIRTUAL_ROOT}")),
        "{:?}",
        env.calls()
    );
    env.assert_no_leaks();
}

fn skill(root: &Path, dir: &str, name: &str) {
    let dir = root.join(dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} skill\n---\nbody of {name}"),
    )
    .unwrap();
}

#[tokio::test]
async fn skills_and_templates_load_through_the_env() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    skill(root, "skills/alpha", "alpha");
    skill(root, "skills/deep/beta", "beta");
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::write(
        root.join("prompts/greet.md"),
        "---\nname: greet\n---\nHello $1",
    )
    .unwrap();
    std::fs::write(root.join("prompts/skip.txt"), "no").unwrap();
    let env = VirtualEnv::new(root);

    let (skills, diags) =
        load_skills_in(&env, &env.root().join("skills"), &host_global_gitignore()).await;
    assert!(diags.is_empty(), "{diags:?}");
    let mut names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["alpha", "beta"]);
    // Paths are shown as the Workspace spells them.
    assert!(
        skills.iter().all(|s| s.path.starts_with(VIRTUAL_ROOT)),
        "{skills:?}"
    );

    let (templates, diags) = load_prompt_templates_in(&env, &env.root().join("prompts")).await;
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "greet");
    assert_eq!(templates[0].path, env.root().join("prompts/greet.md"));

    // A folder that does not exist reports a diagnostic, as on the host.
    let (none, diags) = load_prompt_templates_in(&env, &env.root().join("nope")).await;
    assert!(none.is_empty());
    assert_eq!(diags.len(), 1);
    env.assert_no_leaks();
}

fn write_file(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Compare the host walk of `base/rel` with the walk through the virtual env
/// (same skills, same paths, same order) and with the names expected; a
/// difference is added to `diffs`, so one run shows every case that differs.
async fn same_walk(diffs: &mut Vec<String>, base: &Path, rel: &str, expected: &[&str]) {
    let host = load_skills(&base.join(rel)).0;
    let host: Vec<_> = host
        .iter()
        .map(|s| (s.name.clone(), s.path.clone()))
        .collect();
    let env = VirtualEnv::new(base);
    let through = load_skills_in(&env, &env.root().join(rel), &host_global_gitignore())
        .await
        .0;
    let through: Vec<_> = through
        .iter()
        .map(|s| {
            let rest = s.path.strip_prefix(env.root()).unwrap();
            (s.name.clone(), base.join(rest))
        })
        .collect();
    let names = |v: &[(String, std::path::PathBuf)]| {
        let mut n: Vec<String> = v.iter().map(|(n, _)| n.clone()).collect();
        n.sort();
        n
    };
    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();
    if names(&host) != expected {
        diffs.push(format!(
            "{rel}: the host walk found {:?}, the case expects {expected:?}",
            names(&host)
        ));
    }
    if through != host {
        diffs.push(format!(
            "{rel}: env walk {:?} differs from host walk {:?}",
            names(&through),
            names(&host)
        ));
    }
    if !env.leaks().is_empty() {
        diffs.push(format!("{rel}: leaks {:?}", env.leaks()));
    }
}

/// The loader through the env finds what the host walker finds, in the same
/// order, on a fixture with the cases that decide it.
#[cfg(unix)]
#[tokio::test]
async fn the_env_skill_walk_matches_the_host_walk() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    // `repo/` is a git repo (it has `.git`); `plain/` is not.
    for repo in ["repo", "plain"] {
        let top = base.join(repo);
        let skills = top.join(".claude/skills");
        skill(&skills, "one", &format!("{repo}-one"));
        skill(&skills, "nested/two", &format!("{repo}-two"));
        skill(&skills, ".hidden/three", &format!("{repo}-three"));
        skill(&skills, "ignored_dir/four", &format!("{repo}-four"));
        skill(&skills, "skipme", &format!("{repo}-five"));
        skill(&skills, "dup/a", "dup");
        skill(&skills, "dup/b", "dup");
        std::fs::write(
            top.join(".gitignore"),
            "ignored_dir/\n.claude/skills/skipme/\n",
        )
        .unwrap();
        std::fs::write(skills.join(".ignore"), "# keep nested\nnested/two/x\n").unwrap();
        // A folder that is a link is not entered; a linked SKILL.md loads.
        std::os::unix::fs::symlink(base.join("outside"), skills.join("linked")).unwrap();
        skill(&base, "outside/six", &format!("{repo}-six"));
        std::fs::create_dir_all(skills.join("linkfile")).unwrap();
        std::os::unix::fs::symlink(
            skills.join("one/SKILL.md"),
            skills.join("linkfile/SKILL.md"),
        )
        .unwrap();
    }
    std::fs::create_dir_all(base.join("repo/.git/info")).unwrap();
    std::fs::write(base.join("repo/.git/info/exclude"), "excluded_dir/\n").unwrap();
    skill(
        &base.join("repo/.claude/skills"),
        "excluded_dir/seven",
        "repo-seven",
    );

    for repo in ["repo", "plain"] {
        let root = base.join(repo).join(".claude/skills");
        let host: Vec<_> = load_skills(&root).0;
        let host: Vec<_> = host
            .iter()
            .map(|s| (s.name.clone(), s.path.clone(), s.body.clone()))
            .collect();

        let env = VirtualEnv::new(&base);
        let virt_root = env.root().join(repo).join(".claude/skills");
        let through: Vec<_> = load_skills_in(&env, &virt_root, &host_global_gitignore())
            .await
            .0;
        let through: Vec<_> = through
            .iter()
            .map(|s| {
                let rest = s.path.strip_prefix(env.root()).unwrap();
                (s.name.clone(), base.join(rest), s.body.clone())
            })
            .collect();
        assert_eq!(through, host, "walk of {repo} differs");
        // The fixture decides these cases the same way in both walkers:
        // hidden folders load, a linked folder is not entered, a linked
        // SKILL.md loads, and `.gitignore` and the exclude file apply only
        // inside the repo.
        let mut names: Vec<&str> = host.iter().map(|(n, _, _)| n.as_str()).collect();
        names.sort();
        let expected: Vec<String> = if repo == "repo" {
            ["dup", "dup", "one", "one", "three", "two"]
                .iter()
                .map(|n| {
                    if *n == "dup" {
                        "dup".to_string()
                    } else {
                        format!("repo-{n}")
                    }
                })
                .collect()
        } else {
            ["dup", "dup", "five", "four", "one", "one", "three", "two"]
                .iter()
                .map(|n| {
                    if *n == "dup" {
                        "dup".to_string()
                    } else {
                        format!("plain-{n}")
                    }
                })
                .collect()
        };
        assert_eq!(names, expected, "{repo}");
        env.assert_no_leaks();
    }

    // Rule precedence, repositories and links: cases the two walkers must
    // decide alike (each came from a review of the first env walker).
    let mut diffs: Vec<String> = Vec::new();
    let w = |rel: &str, text: &str| write_file(&base.join(rel), text);
    // P1: a match in any `.ignore` wins over any `.gitignore`, whatever the
    // folder depth. (a) a whitelist in the root `.ignore` beats a deeper
    // `.gitignore` rule; (b) an ignore rule in the root `.ignore` beats a
    // whitelist in a deeper `.gitignore`.
    std::fs::create_dir_all(base.join("p1a/.git")).unwrap();
    w("p1a/skills/.ignore", "!wip/\n");
    w("p1a/skills/sub/.gitignore", "wip/\n");
    skill(&base.join("p1a/skills/sub"), "wip/w", "p1a-wip");
    skill(&base.join("p1a/skills/sub"), "ok", "p1a-ok");
    same_walk(&mut diffs, &base, "p1a/skills", &["p1a-ok", "p1a-wip"]).await;
    std::fs::create_dir_all(base.join("p1b/.git")).unwrap();
    w("p1b/skills/.ignore", "wip/\n");
    w("p1b/skills/sub/.gitignore", "!wip/\n");
    skill(&base.join("p1b/skills/sub"), "wip/w", "p1b-wip");
    same_walk(&mut diffs, &base, "p1b/skills", &[]).await;
    // P2: the rules of a repository above the nearest repository do not apply.
    std::fs::create_dir_all(base.join("p2/.git")).unwrap();
    std::fs::create_dir_all(base.join("p2/inner/.git")).unwrap();
    w("p2/.gitignore", "skipme/\n");
    skill(&base.join("p2/inner/skills"), "skipme/a", "p2-skipme");
    skill(&base.join("p2/inner/skills"), "kept", "p2-kept");
    same_walk(
        &mut diffs,
        &base,
        "p2/inner/skills",
        &["p2-kept", "p2-skipme"],
    )
    .await;
    // P4: a `.jj` folder also makes a folder a repository.
    std::fs::create_dir_all(base.join("p4/.jj")).unwrap();
    w("p4/.gitignore", "skipme/\n");
    skill(&base.join("p4/skills"), "skipme/a", "p4-skipme");
    skill(&base.join("p4/skills"), "kept", "p4-kept");
    same_walk(&mut diffs, &base, "p4/skills", &["p4-kept"]).await;
    // P6: a root that is a link takes its parent folders from the resolved
    // path, so the repository's `.gitignore` applies.
    std::fs::create_dir_all(base.join("p6/repo/.git")).unwrap();
    std::fs::create_dir_all(base.join("p6/elsewhere")).unwrap();
    w("p6/repo/.gitignore", "skipme/\n");
    skill(&base.join("p6/repo/skills"), "skipme/a", "p6-skipme");
    skill(&base.join("p6/repo/skills"), "kept", "p6-kept");
    std::os::unix::fs::symlink("../repo/skills", base.join("p6/elsewhere/link")).unwrap();
    same_walk(&mut diffs, &base, "p6/elsewhere/link", &["p6-kept"]).await;
    // P3: in a git worktree `.git` is a file; the exclude file is the one of
    // the common git dir. (Its `gitdir:` line holds a host path, so this
    // case runs on the host env itself.)
    let main = base.join("p3/main");
    std::fs::create_dir_all(main.join(".git/info")).unwrap();
    std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
    std::fs::write(main.join(".git/info/exclude"), "excl/\n").unwrap();
    std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(base.join("p3/wt")).unwrap();
    std::fs::write(
        base.join("p3/wt/.git"),
        format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
    )
    .unwrap();
    skill(&base.join("p3/wt/skills"), "excl/a", "p3-excl");
    skill(&base.join("p3/wt/skills"), "kept", "p3-kept");
    let host: Vec<_> = load_skills(&base.join("p3/wt/skills")).0;
    assert_eq!(
        host.len(),
        1,
        "the worktree exclude applies on the host walk"
    );
    let env = HostExecutionEnv::new(&base);
    let through = load_skills_in(&env, &base.join("p3/wt/skills"), &host_global_gitignore())
        .await
        .0;
    let names =
        |v: &[loop_agent::harness::Skill]| v.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
    if names(&through) != names(&host) {
        diffs.push(format!(
            "p3: env walk {:?} differs from host walk {:?}",
            names(&through),
            names(&host)
        ));
    }
    assert!(
        diffs.is_empty(),
        "the env walk differs from the host walk:\n{}",
        diffs.join("\n")
    );

    // Templates: the same files in the same order.
    let prompts = base.join("repo/prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    for n in ["b", "a", "c"] {
        std::fs::write(prompts.join(format!("{n}.md")), format!("body {n}")).unwrap();
    }
    std::fs::create_dir_all(prompts.join("dir.md")).unwrap();
    let (host, host_diags) = load_prompt_templates(&prompts);
    let env = VirtualEnv::new(&base);
    let (through, through_diags) =
        load_prompt_templates_in(&env, &env.root().join("repo/prompts")).await;
    let key = |t: &loop_agent::harness::PromptTemplate| (t.name.clone(), t.body.clone());
    assert_eq!(
        through.iter().map(key).collect::<Vec<_>>(),
        host.iter().map(key).collect::<Vec<_>>()
    );
    assert_eq!(through_diags.len(), host_diags.len());
}
