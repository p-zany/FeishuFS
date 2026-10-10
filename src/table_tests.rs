use crate::remote::{
    RemoteSubtable,
    test_support::{FakeSource, leaf, node},
};

use super::*;

fn table(id: &str, name: &str) -> RemoteSubtable {
    RemoteSubtable {
        id: id.into(),
        name: name.into(),
    }
}

fn table_fs(kind: &str) -> (FeishuFs, FakeSource) {
    let source = FakeSource::with_root(node("entry", "Team Wiki", "docx"));
    source
        .children
        .lock()
        .insert("space-1".into(), vec![leaf("report", "报表", kind)]);
    source.subtables.lock().insert(
        "obj-report".into(),
        Ok(vec![table("b", "支出"), table("a", "收入")]),
    );
    source.content.lock().insert(
        "obj-report/a".into(),
        Ok("项目,金额\r\n收入,100\r\n".as_bytes().to_vec()),
    );
    source.content.lock().insert(
        "obj-report/b".into(),
        Ok("项目,金额\r\n支出,50\r\n".as_bytes().to_vec()),
    );
    let fs = FeishuFs::connect(
        Arc::new(source.clone()),
        "https://example.feishu.cn/wiki/entry".parse().unwrap(),
        FsOptions::default(),
    )
    .unwrap();
    (fs, source)
}

fn inode(fs: &FeishuFs, name: &str) -> u64 {
    fs.lookup(ROOT_INODE, name.as_bytes()).unwrap().0
}

fn read_all(fs: &FeishuFs, inode: u64) -> Vec<u8> {
    let mut bytes = vec![0; 4096];
    let count = fs.read(inode, 0, &mut bytes).unwrap();
    bytes.truncate(count);
    bytes
}

#[test]
fn sheet_and_bitable_leaves_expose_each_csv_without_prefetching() {
    for kind in ["sheet", "bitable"] {
        let (fs, source) = table_fs(kind);
        assert_eq!(fs.tree_lines(), ["报表.支出.csv", "报表.收入.csv"]);
        assert_eq!(fs.node_count(), 1);
        assert_eq!(*source.subtable_lists.lock(), ["obj-report"]);
        assert!(source.fetches.lock().is_empty());
        let a = inode(&fs, "报表.收入.csv");
        assert_eq!(
            file_request(&fs.node(a).unwrap()).unwrap(),
            &ContentRequest::TableCsv {
                token: "obj-report".into(),
                document_type: kind.into(),
                sub_id: "a".into(),
            }
        );
        assert_eq!(read_all(&fs, a), "项目,金额\r\n收入,100\r\n".as_bytes());
        assert_eq!(*source.fetches.lock(), ["obj-report/a"]);
        assert_eq!(fs.metadata(a).unwrap().mode, 0o444);
        assert_eq!(fs.write(a, 0, b"no"), Err(ACCESS_DENIED));
        assert_eq!(fs.open_file(a, true), Err(ACCESS_DENIED));
        read_all(&fs, a);
        assert_eq!(source.fetches.lock().len(), 1);
    }
}

#[test]
fn subtable_csv_cache_and_failure_cooldown_are_independent() {
    let (fs, source) = table_fs("sheet");
    let a = inode(&fs, "报表.收入.csv");
    let b = inode(&fs, "报表.支出.csv");
    let old = read_all(&fs, a);
    read_all(&fs, b);
    let stale = Instant::now() - Duration::from_secs(61);
    for id in [a, b] {
        fs.inner.content.lock().get_mut(&id).unwrap().fetched_at = stale;
    }
    source
        .content
        .lock()
        .insert("obj-report/a".into(), Err("offline".into()));
    source
        .content
        .lock()
        .insert("obj-report/b".into(), Ok(b"updated\n".to_vec()));
    for _ in 0..3 {
        fs.open_file(a, false).unwrap();
        assert_eq!(read_all(&fs, a), old);
    }
    assert_eq!(fs.inner.content.lock()[&a].fetched_at, stale);
    fs.open_file(b, false).unwrap();
    assert_eq!(read_all(&fs, b), b"updated\n");
    assert_eq!(
        source
            .fetches
            .lock()
            .iter()
            .filter(|key| *key == "obj-report/a")
            .count(),
        2
    );
    source
        .content
        .lock()
        .insert("obj-report/a".into(), Ok(b"recovered\n".to_vec()));
    fs.inner.failures.lock().get_mut(&a).unwrap().retry_at = Instant::now();
    fs.open_file(a, false).unwrap();
    assert_eq!(read_all(&fs, a), b"recovered\n");
}

#[test]
fn subtable_rename_and_leaf_branch_move_preserve_inode_and_cache() {
    let (fs, source) = table_fs("bitable");
    let a = inode(&fs, "报表.收入.csv");
    let old = read_all(&fs, a);
    source.subtables.lock().insert(
        "obj-report".into(),
        Ok(vec![table("a", "新收入"), table("b", "支出")]),
    );
    source
        .children
        .lock()
        .insert("space-1".into(), vec![node("report", "新报表", "bitable")]);
    source
        .children
        .lock()
        .insert("report".into(), vec![leaf("child", "说明", "docx")]);
    assert!(fs.refresh_now().unwrap().revision_changed);
    let directory = inode(&fs, "新报表");
    assert_eq!(
        fs.lookup(directory, "新报表.新收入.csv".as_bytes())
            .unwrap()
            .0,
        a
    );
    assert!(fs.lookup(directory, b"index.csv").is_err());
    assert!(fs.lookup(directory, b"index.xlsx").is_err());
    assert_eq!(read_all(&fs, a), old);
    source
        .children
        .lock()
        .insert("space-1".into(), vec![node("parent", "目录", "docx")]);
    source
        .children
        .lock()
        .insert("parent".into(), vec![leaf("report", "新报表", "bitable")]);
    fs.refresh_now().unwrap();
    let parent = inode(&fs, "目录");
    assert_eq!(
        fs.lookup(parent, "新报表.新收入.csv".as_bytes()).unwrap().0,
        a
    );
    assert_eq!(read_all(&fs, a), old);
    assert_eq!(source.fetches.lock().len(), 1);
}

#[test]
fn subtable_addition_and_deletion_update_the_snapshot_not_document_count() {
    let (fs, source) = table_fs("sheet");
    let a = inode(&fs, "报表.收入.csv");
    let b = inode(&fs, "报表.支出.csv");
    read_all(&fs, b);
    source.subtables.lock().insert(
        "obj-report".into(),
        Ok(vec![table("a", "收入"), table("c", "利润")]),
    );
    fs.refresh_now().unwrap();
    assert_eq!(inode(&fs, "报表.收入.csv"), a);
    let c = inode(&fs, "报表.利润.csv");
    assert_ne!(c, b);
    assert_eq!(fs.metadata(b), Err(NOT_FOUND));
    assert!(!fs.inner.content.lock().contains_key(&b));
    assert_eq!(fs.node_count(), 1);
    source.subtables.lock().insert(
        "obj-report".into(),
        Ok(vec![table("a", "收入"), table("b", "支出")]),
    );
    fs.refresh_now().unwrap();
    assert_eq!(inode(&fs, "报表.支出.csv"), b);
}

#[test]
fn unchanged_subtable_order_does_not_publish_a_new_revision() {
    let (fs, source) = table_fs("sheet");
    source
        .subtables
        .lock()
        .get_mut("obj-report")
        .unwrap()
        .as_mut()
        .unwrap()
        .reverse();
    assert!(!fs.refresh_now().unwrap().revision_changed);
    assert!(source.fetches.lock().is_empty());
}

#[test]
fn failing_subtable_metadata_keeps_the_complete_previous_snapshot() {
    let (fs, source) = table_fs("bitable");
    let paths = fs.tree_lines();
    let revision = fs.inner.state.read().revision();
    source
        .subtables
        .lock()
        .insert("obj-report".into(), Err("permission denied".into()));
    assert!(fs.refresh_now().is_err());
    assert_eq!(fs.tree_lines(), paths);
    assert_eq!(fs.inner.state.read().revision(), revision);
}

#[test]
fn new_case_insensitive_duplicate_cannot_rename_an_existing_csv() {
    let (fs, source) = table_fs("sheet");
    let a = inode(&fs, "报表.收入.csv");
    source.subtables.lock().insert(
        "obj-report".into(),
        Ok(vec![
            table("0", "收入"),
            table("a", "收入"),
            table("b", "支出"),
        ]),
    );
    fs.refresh_now().unwrap();
    assert_eq!(inode(&fs, "报表.收入.csv"), a);
    let duplicate = inode(&fs, "报表.收入~0.csv");
    assert_ne!(duplicate, a);
}

#[test]
fn csv_names_conflicting_with_sibling_files_or_child_pages_are_disambiguated() {
    let (fs, source) = table_fs("sheet");
    let a = inode(&fs, "报表.收入.csv");
    source.children.lock().insert(
        "space-1".into(),
        vec![
            leaf("file", "报表.收入.csv", "file"),
            leaf("report", "报表", "sheet"),
        ],
    );
    fs.refresh_now().unwrap();
    assert_eq!(inode(&fs, "报表.收入.csv"), a);
    assert!(
        fs.lookup(ROOT_INODE, "报表.收入~file.csv".as_bytes())
            .is_ok()
    );
    source
        .children
        .lock()
        .insert("space-1".into(), vec![node("report", "报表", "sheet")]);
    source.children.lock().insert(
        "report".into(),
        vec![leaf("child", "报表.收入.csv", "file")],
    );
    fs.refresh_now().unwrap();
    let directory = inode(&fs, "报表");
    assert_eq!(
        fs.lookup(directory, "报表.收入.csv".as_bytes()).unwrap().0,
        a
    );
    assert!(
        fs.lookup(directory, "报表.收入~child.csv".as_bytes())
            .is_ok()
    );
}

#[test]
fn a_new_subtable_cannot_shadow_or_rename_an_existing_child_page() {
    let (fs, source) = table_fs("sheet");
    source
        .children
        .lock()
        .insert("space-1".into(), vec![node("report", "报表", "sheet")]);
    source.children.lock().insert(
        "report".into(),
        vec![leaf("child", "报表.利润.csv", "file")],
    );
    fs.refresh_now().unwrap();
    let directory = inode(&fs, "报表");
    let child = fs.lookup(directory, "报表.利润.csv".as_bytes()).unwrap().0;
    source
        .subtables
        .lock()
        .get_mut("obj-report")
        .unwrap()
        .as_mut()
        .unwrap()
        .push(table("c", "利润"));
    fs.refresh_now().unwrap();
    assert_eq!(
        fs.lookup(directory, "报表.利润.csv".as_bytes()).unwrap().0,
        child
    );
    assert!(fs.lookup(directory, "报表.利润~c.csv".as_bytes()).is_ok());
}

#[test]
fn csv_case_collisions_are_disambiguated_before_the_extension() {
    let (fs, source) = table_fs("sheet");
    source.subtables.lock().insert(
        "obj-report".into(),
        Ok(vec![table("a", "Data"), table("b", "data")]),
    );
    fs.refresh_now().unwrap();
    assert!(fs.lookup(ROOT_INODE, "报表.Data.csv".as_bytes()).is_ok());
    assert!(fs.lookup(ROOT_INODE, "报表.data~b.csv".as_bytes()).is_ok());
}

#[test]
fn unicode_long_and_reserved_csv_names_keep_a_safe_csv_extension() {
    let (fs, source) = table_fs("sheet");
    source.children.lock().get_mut("space-1").unwrap()[0].title = "CON".into();
    source.subtables.lock().insert(
        "obj-report".into(),
        Ok(vec![table("a", "A/B:*? ."), table("b", &"文".repeat(100))]),
    );
    fs.refresh_now().unwrap();
    assert!(fs.tree_lines().contains(&"CON_.A_B___.csv".to_owned()));
    for path in fs.tree_lines() {
        assert!(path.ends_with(".csv"));
        assert!(path.len() <= 180);
        assert!(
            !path
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        );
    }
    source.children.lock().get_mut("space-1").unwrap()[0].title = "标题".repeat(100);
    fs.refresh_now().unwrap();
    for path in fs.tree_lines() {
        assert!(path.ends_with(".csv"));
        assert!(path.len() <= 180);
    }
}

#[test]
fn empty_tables_and_missing_object_tokens_explain_instead_of_faking_csv() {
    let (fs, source) = table_fs("sheet");
    source
        .subtables
        .lock()
        .insert("obj-report".into(), Ok(vec![]));
    fs.refresh_now().unwrap();
    assert_eq!(fs.tree_lines(), ["报表.md"]);
    assert!(
        String::from_utf8(read_all(&fs, inode(&fs, "报表.md")))
            .unwrap()
            .contains("no visible subtables")
    );
    let mut missing = leaf("missing", "缺少对象", "bitable");
    missing.object_token = None;
    source
        .children
        .lock()
        .insert("space-1".into(), vec![missing]);
    let count = source.subtable_lists.lock().len();
    fs.refresh_now().unwrap();
    assert_eq!(source.subtable_lists.lock().len(), count);
    assert!(
        String::from_utf8(read_all(&fs, inode(&fs, "缺少对象.md")))
            .unwrap()
            .contains("no readable obj_token")
    );
    assert!(source.fetches.lock().is_empty());
}

#[test]
fn table_links_select_the_correct_csv_and_rerender_after_subtable_rename() {
    let (fs, source) = table_fs("bitable");
    source
        .children
        .lock()
        .get_mut("space-1")
        .unwrap()
        .push(leaf("doc", "正文", "docx"));
    let unknown = "[unknown](https://example.feishu.cn/base/obj-report?table=unknown)\n";
    let text = format!(
        "[default](https://example.feishu.cn/wiki/report?from=copy)\n[table](https://example.feishu.cn/base/obj-report?table=b&view=view1)\n[sheet](https://example.feishu.cn/wiki/report?sheet=a#cell)\n{unknown}"
    );
    source
        .content
        .lock()
        .insert("obj-doc".into(), Ok(text.into_bytes()));
    fs.refresh_now().unwrap();
    let doc = inode(&fs, "正文.md");
    let expected = format!(
        "[default](<报表.收入.csv>)\n[table](<报表.支出.csv>)\n[sheet](<报表.收入.csv#cell>)\n{unknown}"
    );
    assert_eq!(read_all(&fs, doc), expected.as_bytes());
    source.subtables.lock().insert(
        "obj-report".into(),
        Ok(vec![table("a", "收入"), table("b", "支出 #1")]),
    );
    fs.refresh_now().unwrap();
    let rendered = String::from_utf8(read_all(&fs, doc)).unwrap();
    assert!(rendered.contains("[table](<报表.支出 %231.csv>)"));
    assert_eq!(*source.fetches.lock(), ["obj-doc"]);
}

#[test]
fn replacing_the_table_object_invalidates_subtable_content_identity() {
    let (fs, source) = table_fs("sheet");
    let a = inode(&fs, "报表.收入.csv");
    read_all(&fs, a);
    source.children.lock().get_mut("space-1").unwrap()[0].object_token = Some("replacement".into());
    source
        .subtables
        .lock()
        .insert("replacement".into(), Ok(vec![table("a", "收入")]));
    source
        .content
        .lock()
        .insert("replacement/a".into(), Ok(b"new\n".to_vec()));
    fs.refresh_now().unwrap();
    assert_eq!(inode(&fs, "报表.收入.csv"), a);
    assert_eq!(read_all(&fs, a), b"new\n");
}
