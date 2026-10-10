mod support;

use std::sync::Arc;

use feishufs::{ContentRequest, FeishuFs, FsOptions, RemoteNode, RemoteSource, RemoteSubtable};
use serde_json::{Value, json};
use support::{server, source};

fn node(kind: &str) -> RemoteNode {
    RemoteNode {
        space_id: "space-1".into(),
        node_token: "report".into(),
        object_token: Some("tabletoken".into()),
        object_type: Some(kind.into()),
        title: "报表".into(),
        url: None,
        has_child: false,
    }
}

fn json_response(path: &str, data: Value) -> (String, &'static str, Vec<u8>) {
    (
        path.into(),
        "application/json",
        serde_json::to_vec(&json!({"code": 0, "data": data})).unwrap(),
    )
}

fn sheet(id: &str, title: &str, hidden: bool) -> Value {
    json!({"sheet_id": id, "title": title, "index": 0, "hidden": hidden,
        "grid_properties": {"row_count": 10, "column_count": 5, "frozen_row_count": 0, "frozen_column_count": 0},
        "resource_type": "sheet"})
}

#[test]
fn sdk_lists_all_worksheets_including_hidden_ones() {
    let (base, handle) = server(vec![json_response(
        "GET /open-apis/sheets/v3/spreadsheets/tabletoken/sheets/query ",
        json!({"sheets": [sheet("one", "收入", false), sheet("two", "隐藏页", true)]}),
    )]);
    let tables = source(base).list_subtables(&node("sheet")).unwrap();
    assert_eq!(
        tables,
        [
            RemoteSubtable {
                id: "one".into(),
                name: "收入".into()
            },
            RemoteSubtable {
                id: "two".into(),
                name: "隐藏页".into()
            }
        ]
    );
    assert_eq!(handle.join().unwrap().len(), 1);
}

#[test]
fn sdk_lists_all_bitable_pages_with_the_same_auth() {
    let path = "GET /open-apis/bitable/v1/apps/tabletoken/tables?";
    let (base, handle) = server(vec![
        json_response(
            path,
            json!({"items": [{"table_id": "tbl1", "name": "收入"}], "has_more": true, "page_token": "next-page"}),
        ),
        json_response(
            path,
            json!({"items": [{"table_id": "tbl2", "name": "支出"}], "has_more": false}),
        ),
    ]);
    let tables = source(base).list_subtables(&node("bitable")).unwrap();
    assert_eq!(tables.len(), 2);
    assert_eq!(tables[1].id, "tbl2");
    let requests = handle.join().unwrap();
    assert!(requests[0].contains("page_size=100"));
    assert!(requests[1].contains("page_token=next-page"));
    assert!(!requests[0].contains("page_token="));
}

#[test]
fn broken_bitable_pagination_and_missing_table_ids_are_errors_not_partial_lists() {
    let path = "GET /open-apis/bitable/v1/apps/tabletoken/tables?";
    for data in [
        json!({"has_more": true}),
        json!({"has_more": true, "page_token": ""}),
        json!({"items": [{"name": "Missing ID"}], "has_more": false}),
    ] {
        let (base, handle) = server(vec![json_response(path, data)]);
        assert!(source(base).list_subtables(&node("bitable")).is_err());
        handle.join().unwrap();
    }
    let (base, handle) = server(
        ["page1", "page2", "page1"]
            .into_iter()
            .map(|token| {
                json_response(
                    path,
                    json!({"items": [], "has_more": true, "page_token": token}),
                )
            })
            .collect(),
    );
    let error = source(base).list_subtables(&node("bitable")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("pagination token did not advance")
    );
    assert_eq!(handle.join().unwrap().len(), 3);
}

fn export_responses(
    kind: &str,
    content_type: &'static str,
    bytes: Vec<u8>,
) -> Vec<(String, &'static str, Vec<u8>)> {
    vec![
        json_response(
            "POST /open-apis/drive/v1/export_tasks ",
            json!({"ticket": "ticket1"}),
        ),
        json_response(
            "GET /open-apis/drive/v1/export_tasks/ticket1?",
            json!({"result": {
                "file_extension": "csv", "type": kind, "job_status": 0, "file_token": "export1"
            }}),
        ),
        (
            "GET /open-apis/drive/v1/export_tasks/file/export1/download ".into(),
            content_type,
            bytes,
        ),
    ]
}

#[test]
fn sdk_exports_each_table_kind_as_csv_with_an_explicit_sub_id_and_preserves_bytes() {
    let csv = "\u{feff}名称,备注\r\n收入,\"逗号,引号\"\"与\n换行\"\r\n"
        .as_bytes()
        .to_vec();
    for kind in ["sheet", "bitable"] {
        let (base, handle) = server(export_responses(kind, "text/csv", csv.clone()));
        let bytes = source(base)
            .fetch_content(&ContentRequest::TableCsv {
                token: "tabletoken".into(),
                document_type: kind.into(),
                sub_id: "subtable1".into(),
            })
            .unwrap();
        assert_eq!(bytes, csv);
        let requests = handle.join().unwrap();
        let body: Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({"token": "tabletoken", "type": kind, "file_extension": "csv", "sub_id": "subtable1"})
        );
        assert!(requests[1].contains("token=tabletoken"));
    }
}

#[test]
fn export_download_api_errors_are_not_published_as_csv() {
    let error = serde_json::to_vec(&json!({"code": 99991672, "msg": "Permission denied"})).unwrap();
    let (base, handle) = server(export_responses("sheet", "application/json", error));
    let error = source(base)
        .fetch_content(&ContentRequest::TableCsv {
            token: "tabletoken".into(),
            document_type: "sheet".into(),
            sub_id: "subtable1".into(),
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("99991672"));
    handle.join().unwrap();
}

#[test]
fn csv_downloads_respect_the_configured_per_file_limit() {
    let (base, handle) = server(export_responses("bitable", "text/csv", vec![b'a'; 5000]));
    assert!(
        source(base)
            .fetch_content(&ContentRequest::TableCsv {
                token: "tabletoken".into(),
                document_type: "bitable".into(),
                sub_id: "subtable1".into(),
            })
            .is_err()
    );
    handle.join().unwrap();
}

#[test]
fn invalid_csv_selectors_are_rejected_without_sending_requests() {
    for (kind, id) in [
        ("docx", "subtable"),
        ("sheet", ""),
        ("bitable", "../escape"),
    ] {
        let (base, handle) = server(vec![]);
        assert!(
            source(base)
                .fetch_content(&ContentRequest::TableCsv {
                    token: "tabletoken".into(),
                    document_type: kind.into(),
                    sub_id: id.into(),
                })
                .is_err()
        );
        assert!(handle.join().unwrap().is_empty());
    }
}

#[test]
fn sdk_connect_and_refresh_discover_subtables_without_exporting_any_content() {
    let entry = json!({"node": {"space_id": "space-1", "node_token": "entry"}});
    let space = json!({"space": {"space_id": "space-1", "name": "Team Wiki"}});
    let nodes = json!({"items": [{"space_id": "space-1", "node_token": "report", "obj_token": "tabletoken", "obj_type": "sheet", "title": "报表", "has_child": false}], "has_more": false});
    let mut responses = vec![json_response(
        "GET /open-apis/wiki/v2/spaces/get_node?",
        entry,
    )];
    for sheets in [
        vec![sheet("one", "收入", false)],
        vec![sheet("one", "改名收入", false), sheet("two", "支出", false)],
    ] {
        responses.extend([
            json_response("GET /open-apis/wiki/v2/spaces/space-1 ", space.clone()),
            json_response(
                "GET /open-apis/wiki/v2/spaces/space-1/nodes?",
                nodes.clone(),
            ),
            json_response(
                "GET /open-apis/sheets/v3/spreadsheets/tabletoken/sheets/query ",
                json!({"sheets": sheets}),
            ),
        ]);
    }
    let (base, handle) = server(responses);
    let fs = FeishuFs::connect(
        Arc::new(source(base)),
        "https://example.feishu.cn/wiki/entry".parse().unwrap(),
        FsOptions::default(),
    )
    .unwrap();
    assert_eq!(fs.tree_lines(), ["报表.收入.csv"]);
    assert!(fs.refresh_now().unwrap().revision_changed);
    assert_eq!(fs.tree_lines(), ["报表.支出.csv", "报表.改名收入.csv"]);
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 7);
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
}
