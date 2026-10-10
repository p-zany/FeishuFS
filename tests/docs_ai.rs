mod support;

use std::sync::Arc;

use feishufs::{ContentRequest, FeishuFs, FsOptions, RemoteSource};
use serde_json::{Value, json};
use support::{server, source};

fn document_request() -> ContentRequest {
    ContentRequest::Markdown {
        token: "document".into(),
        document_type: "docx".into(),
        url: Some("https://tenant.feishu.cn/docx/document".into()),
    }
}

#[test]
fn fetches_xml_via_sdk_and_stages_the_actual_image_format() {
    let data = json!({"code": 0, "msg": "success", "data": {"document": {
        "content": "<p>before</p><whiteboard token=\"board\"/><p>after</p>"
    }}});
    let image = b"\xff\xd8\xfftest-image".to_vec();
    let (base, handle) = server(vec![
        (
            "POST /open-apis/docs_ai/v1/documents/document/fetch ".into(),
            "application/json",
            serde_json::to_vec(&data).unwrap(),
        ),
        (
            "GET /open-apis/board/v1/whiteboards/board/download_as_image ".into(),
            "image/jpeg",
            image.clone(),
        ),
    ]);
    let bundle = source(base).fetch_bundle(&document_request()).unwrap();
    assert_eq!(bundle.assets.len(), 1);
    assert_eq!(bundle.assets[0].bytes, image);
    assert!(bundle.assets[0].name.ends_with(".jpg"));
    let requests = handle.join().unwrap();
    let body: Value = serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["format"], "xml");
    assert_eq!(body["export_option"]["export_block_id"], false);
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
}

#[test]
fn mermaid_source_needs_no_media_request_and_preserves_standard_links() {
    let data = json!({"code": 0, "data": {"document": {
        "content": "<cite type=\"doc\" file-type=\"wiki\" doc-id=\"other\" title=\"Other\"/><whiteboard token=\"board\" type=\"mermaid\">A --&gt; B</whiteboard>"
    }}});
    let (base, handle) = server(vec![(
        "POST /open-apis/docs_ai/v1/documents/document/fetch ".into(),
        "application/json",
        serde_json::to_vec(&data).unwrap(),
    )]);
    let bundle = source(base).fetch_bundle(&document_request()).unwrap();
    assert!(bundle.assets.is_empty());
    let body = String::from_utf8(bundle.bytes).unwrap();
    assert!(body.contains("[Other](<https://tenant.feishu.cn/wiki/other>)"));
    assert!(body.contains("```mermaid\nA --> B\n```"));
    assert_eq!(handle.join().unwrap().len(), 1);
}

#[test]
fn raw_download_permission_errors_are_not_reported_as_empty_or_valid_images() {
    let error = json!({"code": 99991672, "msg": "Access denied: board:whiteboard:node:read"});
    let (base, handle) = server(vec![(
        "GET /open-apis/board/v1/whiteboards/board/download_as_image ".into(),
        "application/json",
        serde_json::to_vec(&error).unwrap(),
    )]);
    let error = source(base)
        .fetch_content(&ContentRequest::Whiteboard {
            token: "board".into(),
        })
        .unwrap_err();
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("99991672"));
    assert!(diagnostic.contains("API error"));
    assert!(diagnostic.is_ascii());
    handle.join().unwrap();
}

#[test]
fn directory_refresh_reuses_the_selected_space_without_resolving_the_entry_again() {
    let entry =
        json!({"code": 0, "data": {"node": {"space_id": "space-1", "node_token": "entry"}}});
    let space = |name| json!({"code": 0, "data": {"space": {"space_id": "space-1", "name": name}}});
    let children = |title| {
        json!({"code": 0, "data": {"items": [{
        "space_id": "space-1", "node_token": "one", "obj_token": "obj-one",
        "obj_type": "docx", "title": title, "has_child": false
    }], "has_more": false}})
    };
    let (base, handle) = server(vec![
        (
            "GET /open-apis/wiki/v2/spaces/get_node?".into(),
            "application/json",
            serde_json::to_vec(&entry).unwrap(),
        ),
        (
            "GET /open-apis/wiki/v2/spaces/space-1 ".into(),
            "application/json",
            serde_json::to_vec(&space("Team Wiki")).unwrap(),
        ),
        (
            "GET /open-apis/wiki/v2/spaces/space-1/nodes?".into(),
            "application/json",
            serde_json::to_vec(&children("One")).unwrap(),
        ),
        (
            "GET /open-apis/wiki/v2/spaces/space-1 ".into(),
            "application/json",
            serde_json::to_vec(&space("Renamed Wiki")).unwrap(),
        ),
        (
            "GET /open-apis/wiki/v2/spaces/space-1/nodes?".into(),
            "application/json",
            serde_json::to_vec(&children("Renamed")).unwrap(),
        ),
    ]);
    let fs = FeishuFs::connect(
        Arc::new(source(base)),
        "https://example.feishu.cn/wiki/entry".parse().unwrap(),
        FsOptions::default(),
    )
    .unwrap();
    assert_eq!(fs.root_title(), "Team Wiki");
    assert_eq!(fs.tree_lines(), ["One.md"]);
    assert!(fs.refresh_now().unwrap().revision_changed);
    assert_eq!(fs.root_title(), "Renamed Wiki");
    assert_eq!(fs.tree_lines(), ["Renamed.md"]);
    let requests = handle.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.contains("/get_node?"))
            .count(),
        1
    );
}
