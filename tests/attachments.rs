use feishufs::{ContentRequest, RemoteSource};
use serde_json::json;

mod support;
use support::{server, server_with_status, source};

#[test]
fn file_download_preserves_original_binary_bytes() {
    let bytes = b"%PDF-1.7\n\0\xffbinary\n".to_vec();
    let (base, handle) = server(vec![(
        "GET /open-apis/drive/v1/files/filetoken/download ".into(),
        "application/pdf",
        bytes.clone(),
    )]);
    let content = source(base)
        .fetch_content(&ContentRequest::Download {
            token: "filetoken".into(),
        })
        .unwrap();
    assert_eq!(content, bytes);
    handle.join().unwrap();
}

#[test]
fn download_errors_keep_server_code_message_and_request_id() {
    for status in [200, 400] {
        let (base, handle) = server_with_status(vec![(
            "GET /open-apis/drive/v1/files/filetoken/download ".into(),
            status,
            "application/json",
            serde_json::to_vec(&json!({
                "code": 99991672,
                "msg": "Access denied: drive:file:download",
                "request_id": "download-request-1",
            }))
            .unwrap(),
        )]);
        let error = source(base)
            .fetch_content(&ContentRequest::Download {
                token: "filetoken".into(),
            })
            .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("99991672"), "{diagnostic}");
        assert!(diagnostic.contains("drive:file:download"), "{diagnostic}");
        assert!(diagnostic.contains("download-request-1"), "{diagnostic}");
        assert!(!diagnostic.contains("returned no data"), "{diagnostic}");
        handle.join().unwrap();
    }
}

#[test]
fn attachment_downloads_obey_the_per_file_limit() {
    let (base, handle) = server(vec![(
        "GET /open-apis/drive/v1/files/filetoken/download ".into(),
        "application/pdf",
        vec![0; 5000],
    )]);
    let error = source(base)
        .fetch_content(&ContentRequest::Download {
            token: "filetoken".into(),
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("4096"));
    handle.join().unwrap();
}

#[test]
fn invalid_attachment_tokens_are_rejected_before_network_io() {
    let source = source("http://127.0.0.1:1".into());
    for token in ["", "../file", "file/token", "file?query", "file#fragment"] {
        let error = source
            .fetch_content(&ContentRequest::Download {
                token: token.into(),
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "Invalid attachment token");
    }
}
