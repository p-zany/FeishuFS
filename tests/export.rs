use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::json;

#[allow(dead_code)]
mod support;

fn run(command: &str, destination: &Path, body: Option<&str>) -> Output {
    let response = match body {
        Some(body) => json!({"code": 0, "data": {"document": {"content": body}}}),
        None => json!({"code": 99991672, "msg": "Access denied"}),
    };
    let (base, handle) = support::server(vec![
        (
            "GET /open-apis/wiki/v2/spaces/get_node?".into(),
            "application/json",
            serde_json::to_vec(&json!({"code": 0, "data": {"node": {
                "space_id": "space", "node_token": "doc"
            }}}))
            .unwrap(),
        ),
        (
            "GET /open-apis/wiki/v2/spaces/space ".into(),
            "application/json",
            serde_json::to_vec(&json!({"code": 0, "data": {"space": {
                "space_id": "space", "name": "Team"
            }}}))
            .unwrap(),
        ),
        (
            "GET /open-apis/wiki/v2/spaces/space/nodes?".into(),
            "application/json",
            serde_json::to_vec(&json!({"code": 0, "data": {"items": [{
                "space_id": "space", "node_token": "doc", "obj_token": "doc",
                "obj_type": "docx", "title": "Guide", "has_child": false
            }], "has_more": false}}))
            .unwrap(),
        ),
        (
            "POST /open-apis/docs_ai/v1/documents/doc/fetch ".into(),
            "application/json",
            serde_json::to_vec(&response).unwrap(),
        ),
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_feishufs"))
        .args([command, "https://example.feishu.cn/wiki/doc"])
        .arg(destination)
        .args([
            "--app-id",
            "test-app",
            "--app-secret",
            "test-secret",
            "--user-access-token",
            "test-user-token",
            "--api-base-url",
            &base,
            "--allow-custom-api-base-url",
            "--no-log-file",
        ])
        .output()
        .unwrap();
    handle.join().unwrap();
    output
}

#[test]
fn export_and_sync_run_without_a_mount_and_fetch_fresh_content_each_time() {
    let temporary = tempfile::tempdir().unwrap();
    let destination = temporary.path().join("output");
    for (command, body) in [("export", "first"), ("sync", "second")] {
        let output = run(command, &destination, Some(&format!("<p>{body}</p>")));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Files copied: 1"));
        assert_eq!(
            fs::read_to_string(destination.join("Guide.md"))
                .unwrap()
                .trim(),
            body
        );
    }
}

#[test]
fn a_failed_cli_sync_returns_failure_and_preserves_the_previous_file() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("Guide.md"), b"previous").unwrap();
    let output = run("sync", temporary.path(), None);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Guide.md"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Files copied:"));
    assert_eq!(
        fs::read(temporary.path().join("Guide.md")).unwrap(),
        b"previous"
    );
    assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
}
