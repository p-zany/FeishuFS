use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

use std::sync::Arc;

use feishufs::{
    ContentRequest, FeishuFs, FsOptions, OpenLarkOptions, OpenLarkSource, RemoteSource,
};
use serde_json::{Value, json};

// Exercise the actual SDK request builder, auth selection and raw decoder,
// without real credentials or a cloud service.
fn server(
    responses: Vec<(String, &'static str, Vec<u8>)>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (expected, content_type, body) in responses {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "SDK did not send expected request"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let (headers_end, length) = loop {
                let mut buffer = [0; 2048];
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (end + 4, length);
                }
            };
            while bytes.len() < headers_end + length {
                let mut buffer = [0; 2048];
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.starts_with(&expected), "unexpected API path");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-user-token")
            );
            requests.push(request);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
        requests
    });
    (address, handle)
}

fn source(base_url: String) -> OpenLarkSource {
    OpenLarkSource::new(OpenLarkOptions {
        app_id: "test-app".into(),
        app_secret: "test-secret".into(),
        user_access_token: Some("test-user-token".into()),
        api_base_url: base_url,
        allow_custom_api_base_url: true,
        max_download_bytes: 4096,
        request_timeout: Duration::from_secs(5),
    })
    .unwrap()
}

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
