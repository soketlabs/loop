//! HostExecutionEnv + built-in tools tests.

use std::sync::Arc;

use loop_agent::harness::{
    create_bash_tool, create_edit_tool, create_read_tool, create_write_tool, HostExecutionEnv,
};
use serde_json::json;

#[tokio::test]
async fn write_read_edit_bash() {
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn loop_agent::harness::ExecutionEnv>;

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
    assert!(matches!(&r.content[0], loop_ai::ToolResultContent::Text(t) if t.text.contains("hello")));

    let edit = create_edit_tool(Arc::clone(&env));
    (edit.execute)(
        "3".into(),
        json!({"path": "f.txt", "oldText": "hello", "newText": "hi"}),
        None,
        None,
    )
    .await
    .unwrap();

    let bash = create_bash_tool(env);
    let out = (bash.execute)(
        "4".into(),
        json!({"command": "echo ok"}),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(&out.content[0], loop_ai::ToolResultContent::Text(t) if t.text.contains("ok")));
}
