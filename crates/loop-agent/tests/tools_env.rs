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

/// This test binary parses documents in process; the `loop` binary uses a
/// child (see loop-cli's `document_reader` test).
fn in_process_reader() {
    loop_agent::harness::set_document_reader(loop_agent::harness::DocumentReader::InProcess);
}

#[tokio::test]
async fn documents_are_written_from_text_and_read_back_as_text() {
    in_process_reader();
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

#[tokio::test]
async fn overwriting_a_document_warns_that_it_is_rebuilt_from_text() {
    in_process_reader();
    let dir = tempfile::tempdir().unwrap();
    let env =
        Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn loop_agent::harness::ExecutionEnv>;
    let write = create_write_tool(Arc::clone(&env));
    let first = (write.execute)(
        "1".into(),
        json!({"path": "a.docx", "content": "# One"}),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        !text(&first).contains("rebuilt"),
        "a new file loses nothing"
    );
    let second = (write.execute)(
        "2".into(),
        json!({"path": "a.docx", "content": "# Two"}),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        text(&second).contains("formatting, images, comments"),
        "{}",
        text(&second)
    );
    let diff = second.details["diff"].as_str().unwrap();
    assert!(
        diff.starts_with("# The document was rebuilt from this text"),
        "{diff}"
    );
    assert!(diff.contains("-# One") && diff.contains("+# Two"), "{diff}");

    // A previous file the reader cannot read is not rebuilt from text the
    // model never saw.
    std::fs::write(dir.path().join("broken.docx"), b"not a zip").unwrap();
    let err = (write.execute)(
        "3".into(),
        json!({"path": "broken.docx", "content": "# New"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("could not be read") && err.contains("another path"),
        "{err}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("broken.docx")).unwrap(),
        b"not a zip"
    );
    // A valid zip whose document part is not well-formed XML is damaged,
    // not empty: the write is refused and the bytes stay (Linux check D1).
    let damaged = {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"<w:document xmlns:w='x'><w:body><w:p><w:r><w:t>unclosed")
            .unwrap();
        zip.finish().unwrap().into_inner()
    };
    std::fs::write(dir.path().join("badxml.docx"), &damaged).unwrap();
    let read = create_read_tool(Arc::clone(&env));
    let err = (read.execute)("3a".into(), json!({"path": "badxml.docx"}), None, None)
        .await
        .map(|r| text(&r).to_owned())
        .unwrap_or_else(|e| e);
    assert!(err.contains("malformed XML"), "{err}");
    let err = (write.execute)(
        "3c".into(),
        json!({"path": "badxml.docx", "content": "# New"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("could not be read") && err.contains("malformed XML"),
        "{err}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("badxml.docx")).unwrap(),
        damaged
    );
    // A valid document with no text is empty, not damaged: it stays writable.
    (write.execute)(
        "3d".into(),
        json!({"path": "blank.docx", "content": ""}),
        None,
        None,
    )
    .await
    .unwrap();
    let blank = (read.execute)("3e".into(), json!({"path": "blank.docx"}), None, None)
        .await
        .unwrap();
    assert_eq!(text(&blank).trim(), "");
    (write.execute)(
        "3f".into(),
        json!({"path": "blank.docx", "content": "# Filled"}),
        None,
        None,
    )
    .await
    .unwrap();

    // An empty file has nothing to lose.
    std::fs::write(dir.path().join("empty.docx"), b"").unwrap();
    (write.execute)(
        "3b".into(),
        json!({"path": "empty.docx", "content": "# New"}),
        None,
        None,
    )
    .await
    .unwrap();

    // A document with more text than read shows is not overwritten.
    let long = (0..3000)
        .map(|i| format!("Paragraph {i} with some words in it."))
        .collect::<Vec<_>>()
        .join("\n\n");
    (write.execute)(
        "4".into(),
        json!({"path": "long.docx", "content": long}),
        None,
        None,
    )
    .await
    .unwrap();
    let read = create_read_tool(Arc::clone(&env));
    let shown = (read.execute)("5".into(), json!({"path": "long.docx"}), None, None)
        .await
        .unwrap();
    assert_eq!(shown.details["truncated"], true);
    assert!(text(&shown).contains("Do not overwrite it"));
    let before = std::fs::read(dir.path().join("long.docx")).unwrap();
    let err = (write.execute)(
        "6".into(),
        json!({"path": "long.docx", "content": "# Short"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("more text than read can show"), "{err}");
    assert_eq!(std::fs::read(dir.path().join("long.docx")).unwrap(), before);
}

/// A read-only file in the working directory: the tool errors say how to
/// make it writable, so the agent does not edit a copy elsewhere.
#[cfg(unix)]
#[tokio::test]
async fn permission_denied_errors_hint_at_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("ro.txt");
    std::fs::write(&file, "hello").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().append(true).open(&file).is_ok() {
        // Root writes read-only files: nothing to test.
        return;
    }
    let env =
        Arc::new(HostExecutionEnv::new(dir.path())) as Arc<dyn loop_agent::harness::ExecutionEnv>;

    let write = create_write_tool(Arc::clone(&env));
    let err = (write.execute)(
        "1".into(),
        json!({"path": "ro.txt", "content": "x"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("chmod u+w"), "{err}");

    let edit = create_edit_tool(Arc::clone(&env));
    let err = (edit.execute)(
        "2".into(),
        json!({"path": "ro.txt", "oldText": "hello", "newText": "hi"}),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("chmod u+w"), "{err}");

    let bash = create_bash_tool(env);
    let out = (bash.execute)(
        "3".into(),
        json!({"command": "echo x > ro.txt"}),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        matches!(&out.content[0], loop_ai::ToolResultContent::Text(t) if t.text.contains("chmod u+w")),
        "{:?}",
        out.content
    );
}
