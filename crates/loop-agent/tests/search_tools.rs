//! The read-only search tools: `grep`, `glob` and `ls`.

use std::path::Path;
use std::sync::Arc;

use loop_agent::harness::{
    create_glob_tool, create_grep_tool, create_ls_tool, search_tools_with_filter, ExecutionEnv,
    HostExecutionEnv, SearchFilter,
};
use loop_agent::AgentTool;
use serde_json::{json, Value};

/// A project with a `.gitignore` but no `.git`, as in a fresh temp dir.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::create_dir_all(root.join(".github")).unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {\n    run();\n}\n").unwrap();
    std::fs::write(
        root.join("src/nested/lib.rs"),
        "pub fn run() {}\n// TODO later\n",
    )
    .unwrap();
    std::fs::write(root.join("README.md"), "Run it with cargo run.\n").unwrap();
    std::fs::write(root.join("target/out.rs"), "fn run() {}\n").unwrap();
    std::fs::write(root.join("debug.log"), "run failed\n").unwrap();
    std::fs::write(root.join(".github/ci.yml"), "run: cargo test\n").unwrap();
    std::fs::write(root.join("blob.bin"), b"run\0\x01\x02").unwrap();
    dir
}

fn env(root: &Path) -> Arc<dyn ExecutionEnv> {
    Arc::new(HostExecutionEnv::new(root))
}

async fn call(tool: &AgentTool, args: Value) -> (String, Value) {
    let result = (tool.execute)("t".into(), args, None, None).await.unwrap();
    let text = match &result.content[0] {
        loop_ai::ToolResultContent::Text(t) => t.text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    (text, result.details)
}

#[tokio::test]
async fn grep_finds_lines_and_skips_ignored_hidden_and_binary_files() {
    let dir = project();
    let grep = create_grep_tool(env(dir.path()));
    let (text, details) = call(&grep, json!({"pattern": "run"})).await;
    assert!(text.contains("src/main.rs:2:    run();"), "{text}");
    assert!(
        text.contains("src/nested/lib.rs:1:pub fn run() {}"),
        "{text}"
    );
    assert!(text.contains("README.md:1:"), "{text}");
    assert!(!text.contains("target/"), "gitignored dir: {text}");
    assert!(!text.contains("debug.log"), "gitignored file: {text}");
    assert!(!text.contains(".github"), "hidden: {text}");
    assert!(!text.contains("blob.bin"), "binary: {text}");
    assert_eq!(details["truncated"], false);
    assert_eq!(details["matches"], 3);
}

#[tokio::test]
async fn grep_options_narrow_and_shape_the_output() {
    let dir = project();
    let grep = create_grep_tool(env(dir.path()));

    let (text, _) = call(
        &grep,
        json!({"pattern": "RUN", "path": "src", "ignoreCase": true}),
    )
    .await;
    assert!(text.contains("src/main.rs:2:"), "{text}");
    assert!(!text.contains("README"), "path narrows the search: {text}");

    let (text, _) = call(&grep, json!({"pattern": "RUN"})).await;
    assert_eq!(text, "No matches.");

    let (text, _) = call(&grep, json!({"pattern": "run", "glob": "*.md"})).await;
    assert_eq!(text.trim(), "README.md:1:Run it with cargo run.");

    let (text, _) = call(&grep, json!({"pattern": "run", "outputMode": "files"})).await;
    let mut files: Vec<&str> = text.lines().collect();
    files.sort();
    assert_eq!(files, ["README.md", "src/main.rs", "src/nested/lib.rs"]);

    let (text, _) = call(&grep, json!({"pattern": "run", "outputMode": "count"})).await;
    assert!(text.lines().any(|l| l == "src/main.rs:1"), "{text}");

    let (text, _) = call(&grep, json!({"pattern": "cargo", "hidden": true})).await;
    assert!(text.contains(".github/ci.yml:1:"), "{text}");

    let (text, _) = call(
        &grep,
        json!({"pattern": "TODO", "path": "src/nested/lib.rs"}),
    )
    .await;
    assert_eq!(text.trim(), "src/nested/lib.rs:2:// TODO later");
}

#[tokio::test]
async fn grep_reports_a_bad_pattern_and_a_missing_path() {
    let dir = project();
    let grep = create_grep_tool(env(dir.path()));
    let err = (grep.execute)("t".into(), json!({"pattern": "("}), None, None)
        .await
        .unwrap_err();
    assert!(err.contains("invalid regex"), "{err}");
    let err = (grep.execute)(
        "t".into(),
        json!({"pattern": "x", "path": "nope"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("nope"), "{err}");
}

#[tokio::test]
async fn grep_caps_its_output_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    let many: String = (0..1000).map(|i| format!("hit {i}\n")).collect();
    std::fs::write(dir.path().join("big.txt"), many).unwrap();
    let grep = create_grep_tool(env(dir.path()));
    let (text, details) = call(&grep, json!({"pattern": "hit"})).await;
    assert_eq!(details["truncated"], true);
    assert!(text.lines().count() < 1000);
    assert!(
        text.trim_end()
            .ends_with("narrow the pattern, path or glob to see the rest]"),
        "{}",
        text.lines().last().unwrap()
    );
}

#[tokio::test]
async fn glob_matches_paths_and_respects_gitignore() {
    let dir = project();
    let glob = create_glob_tool(env(dir.path()));
    let (text, details) = call(&glob, json!({"pattern": "*.rs"})).await;
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        ["src/main.rs", "src/nested/lib.rs"]
    );
    assert_eq!(details["truncated"], false);

    let (text, _) = call(&glob, json!({"pattern": "nested/*.rs", "path": "src"})).await;
    assert_eq!(text.trim(), "src/nested/lib.rs");

    let (text, _) = call(&glob, json!({"pattern": "*.yml", "hidden": true})).await;
    assert_eq!(text.trim(), ".github/ci.yml");

    let (text, _) = call(&glob, json!({"pattern": "*.py"})).await;
    assert_eq!(text, "No files found.");
}

#[tokio::test]
async fn glob_caps_its_output_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..600 {
        std::fs::write(dir.path().join(format!("f{i:03}.txt")), "").unwrap();
    }
    let glob = create_glob_tool(env(dir.path()));
    let (text, details) = call(&glob, json!({"pattern": "*.txt"})).await;
    assert_eq!(details["truncated"], true);
    assert!(text.contains("[truncated:"), "{text}");
}

#[tokio::test]
async fn ls_lists_one_directory_with_dirs_marked() {
    let dir = project();
    let ls = create_ls_tool(env(dir.path()));
    let (text, _) = call(&ls, json!({})).await;
    let entries: Vec<&str> = text.lines().collect();
    assert_eq!(
        entries,
        [
            ".github/",
            ".gitignore",
            "README.md",
            "blob.bin",
            "debug.log",
            "src/",
            "target/"
        ]
    );
    let (text, _) = call(&ls, json!({"path": "src"})).await;
    assert_eq!(text.lines().collect::<Vec<_>>(), ["main.rs", "nested/"]);

    let err = (ls.execute)("t".into(), json!({"path": "src/main.rs"}), None, None)
        .await
        .unwrap_err();
    assert!(err.contains("not a directory"), "{err}");
}

#[test]
fn the_parameters_are_documented() {
    let dir = tempfile::tempdir().unwrap();
    for tool in [
        create_grep_tool(env(dir.path())),
        create_glob_tool(env(dir.path())),
        create_ls_tool(env(dir.path())),
    ] {
        let props = tool.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("path"), "{}", tool.name);
        for (name, schema) in props {
            assert!(
                schema["description"]
                    .as_str()
                    .is_some_and(|d| !d.is_empty()),
                "{}.{name} has no description",
                tool.name
            );
        }
    }
}

/// A home-like directory with `.ssh/id_rsa` and `notes.txt`, both
/// mentioning "key".
fn home() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
    std::fs::write(dir.path().join(".ssh/id_rsa"), "private key\n").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "key ideas\n").unwrap();
    dir
}

/// Hides what is under `.ssh`, as a `read(~/.ssh/*)` deny rule would.
fn hide_ssh(home: &Path) -> SearchFilter {
    let ssh = home.join(".ssh");
    Arc::new(move |_tool: &str, path: &Path| !path.starts_with(&ssh))
}

#[tokio::test]
async fn files_permission_rules_hide_are_not_searched_listed_or_shown() {
    let dir = home();
    let root = dir.path().to_string_lossy().to_string();
    let [grep, glob, ls] = search_tools_with_filter(env(dir.path()), hide_ssh(dir.path()));

    let (text, details) = call(
        &grep,
        json!({"pattern": "key", "path": root, "hidden": true}),
    )
    .await;
    assert!(text.contains("notes.txt:1:key ideas"), "{text}");
    assert!(!text.contains("private"), "{text}");
    assert!(
        text.ends_with("[1 paths hidden by permission rules]"),
        "{text}"
    );
    assert_eq!(details["hiddenByRules"], 1);

    let (text, _) = call(&glob, json!({"pattern": "*", "path": root, "hidden": true})).await;
    assert!(!text.contains("id_rsa"), "{text}");
    assert!(
        text.contains("[1 paths hidden by permission rules]"),
        "{text}"
    );

    let ssh = dir.path().join(".ssh").to_string_lossy().to_string();
    let (text, _) = call(&ls, json!({"path": ssh})).await;
    assert!(!text.contains("id_rsa"), "{text}");
    assert!(
        text.contains("[1 paths hidden by permission rules]"),
        "{text}"
    );

    let (text, _) = call(&grep, json!({"pattern": "ideas", "path": root})).await;
    assert!(
        !text.contains("hidden by"),
        "nothing was hidden here: {text}"
    );
}
