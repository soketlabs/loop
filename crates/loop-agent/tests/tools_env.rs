//! HostExecutionEnv + built-in tools tests.

use std::sync::Arc;

use loop_agent::harness::{
    create_bash_tool, create_edit_tool, create_read_tool, create_write_tool, HostExecutionEnv,
};
use serde_json::json;

#[tokio::test]
async fn write_read_edit_bash() {
    let dir = tempfile::tempdir().unwrap();
    let env =
        Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn loop_agent::harness::ExecutionEnv>;

    let write = create_write_tool(Arc::clone(&env));
    (write.execute)(
        "1".into(),
        json!({"path": "f.txt", "content": "hello world"}),
        None,
        None,
    )
    .await
    .unwrap();

    let read = create_read_tool(Arc::clone(&env));
    let r = (read.execute)("2".into(), json!({"path": "f.txt"}), None, None)
        .await
        .unwrap();
    assert!(
        matches!(&r.content[0], loop_ai::ToolResultContent::Text(t) if t.text.contains("hello"))
    );

    let edit = create_edit_tool(Arc::clone(&env));
    (edit.execute)(
        "3".into(),
        json!({"path": "f.txt", "oldText": "hello", "newText": "hi"}),
        None,
        None,
    )
    .await
    .unwrap();

    let bash = create_bash_tool(Arc::clone(&env));
    let out = (bash.execute)("4".into(), json!({"command": "echo ok"}), None, None)
        .await
        .unwrap();
    assert!(
        matches!(&out.content[0], loop_ai::ToolResultContent::Text(t) if t.text.contains("ok"))
    );
}

fn text(result: &loop_agent::AgentToolResult) -> &str {
    match &result.content[0] {
        loop_ai::ToolResultContent::Text(t) => &t.text,
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn documents_are_written_from_text_and_read_back_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let env =
        Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn loop_agent::harness::ExecutionEnv>;
    let write = create_write_tool(Arc::clone(&env));
    let read = create_read_tool(Arc::clone(&env));
    let edit = create_edit_tool(Arc::clone(&env));

    let sheet = "## Sheet: Totals\n\n| | A | B |\n|---|---|---|\n| 1 | Apples | 3 |\n| 2 | Pears | 4 |\n| 3 | Sum | =SUM(B1:B2) |";
    let written = (write.execute)(
        "1".into(),
        json!({"path": "t.xlsx", "content": sheet}),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(written.details["created"], true);
    let bytes = std::fs::read(dir.path().join("t.xlsx")).unwrap();
    assert!(bytes.starts_with(b"PK"), "an xlsx is a zip");
    let r = (read.execute)("2".into(), json!({"path": "t.xlsx"}), None, None)
        .await
        .unwrap();
    assert_eq!(text(&r), sheet);

    // Overwriting keeps the original bytes for revert and diffs the text.
    let rewritten = (write.execute)(
        "3".into(),
        json!({"path": "t.xlsx", "content": sheet.replace("| 4 |", "| 5 |")}),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(rewritten.details["created"], false);
    let snapshot = rewritten.details["previousPath"].as_str().unwrap();
    assert_eq!(std::fs::read(snapshot).unwrap(), bytes);
    let diff = rewritten.details["diff"].as_str().unwrap();
    assert!(
        diff.contains("-| 2 | Pears | 4 |") && diff.contains("+| 2 | Pears | 5 |"),
        "{diff}"
    );

    let err = (edit.execute)(
        "4".into(),
        json!({"path": "t.xlsx", "oldText": "Pears", "newText": "Plums"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("write the whole document"), "{err}");

    let err = (write.execute)(
        "5".into(),
        json!({"path": "bad.ods", "content": "| A | B |"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("column letters"), "{err}");

    (write.execute)(
        "6".into(),
        json!({"path": "r.docx", "content": "# Hi\n\n- a\n- b"}),
        None,
        None,
    )
    .await
    .unwrap();
    let r = (read.execute)("7".into(), json!({"path": "r.docx"}), None, None)
        .await
        .unwrap();
    assert_eq!(text(&r), "# Hi\n\n- a\n- b");
    assert_eq!(r.details["format"], "Word");
}
