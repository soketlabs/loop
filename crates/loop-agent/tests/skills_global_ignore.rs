//! The user's global ignore file applies to the skill walk through the env
//! as it does to the host walker. This test has its own binary because it
//! sets `HOME` and `XDG_CONFIG_HOME`, which no other test may see change.

#[path = "support/virtual_env.rs"]
mod virtual_env;

use ignore::gitignore::Gitignore;
use loop_agent::harness::skills::{host_global_gitignore, load_skills, load_skills_in};
use virtual_env::VirtualEnv;

fn skill(root: &std::path::Path, dir: &str, name: &str) {
    let dir = root.join(dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: d\n---\nbody"),
    )
    .unwrap();
}

#[tokio::test]
async fn the_global_ignore_file_applies_as_in_the_host_walk() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("xdg/git")).unwrap();
    std::fs::write(home.path().join("xdg/git/ignore"), "globalskip/\n").unwrap();
    // No other git config may name another excludes file.
    std::env::set_var("HOME", home.path());
    std::env::set_var("XDG_CONFIG_HOME", home.path().join("xdg"));

    let ws = tempfile::tempdir().unwrap();
    let base = ws.path().canonicalize().unwrap();
    // A repository: the global rules apply. A plain folder: they do not.
    std::fs::create_dir_all(base.join("repo/.git")).unwrap();
    for top in ["repo", "plain"] {
        skill(
            &base.join(top).join("skills"),
            "globalskip/a",
            &format!("{top}-global"),
        );
        skill(
            &base.join(top).join("skills"),
            "kept",
            &format!("{top}-kept"),
        );
    }
    let global = host_global_gitignore();
    for (top, expected) in [
        ("repo", vec!["repo-kept"]),
        ("plain", vec!["plain-global", "plain-kept"]),
    ] {
        let host = load_skills(&base.join(top).join("skills")).0;
        let env = VirtualEnv::new(&base);
        let root = env.root().join(top).join("skills");
        let through = load_skills_in(&env, &root, &global).await.0;
        let names = |v: &[loop_agent::harness::Skill]| {
            let mut n: Vec<_> = v.iter().map(|s| s.name.clone()).collect();
            n.sort();
            n
        };
        assert_eq!(names(&host), expected, "host walk of {top}");
        assert_eq!(names(&through), expected, "env walk of {top}");
        env.assert_no_leaks();
        if top == "repo" {
            // Without the global file the walk would show the skipped skill.
            let without = load_skills_in(&env, &root, &Gitignore::empty()).await.0;
            assert_eq!(names(&without), ["repo-global", "repo-kept"]);
        }
    }
}
