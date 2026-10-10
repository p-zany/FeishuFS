//! Explicitly opted-in read-only verification; never run by normal tests/CI.
use std::{env, fs, sync::Arc, time::Duration};

use feishufs::{
    ContentRequest, FeishuFs, FsOptions, KnowledgeLink, OpenLarkOptions, OpenLarkSource,
    RemoteSource,
};
use fsk::{Filesystem, ROOT_INODE};

#[test]
#[ignore = "requires credentials and an explicitly selected document; reads real APIs"]
fn reads_live_mermaid_links_and_whiteboard_image() {
    let url = env::var("FEISHUFS_LIVE_DOC_URL").expect("select FEISHUFS_LIVE_DOC_URL explicitly");
    let path = env::var("FEISHUFS_LIVE_DOC_PATH")
        .expect("select mounted FEISHUFS_LIVE_DOC_PATH explicitly");
    let output = env::var("FEISHUFS_LIVE_OUTPUT").expect("select FEISHUFS_LIVE_OUTPUT explicitly");
    let link: KnowledgeLink = url.parse().unwrap();
    let source = Arc::new(
        OpenLarkSource::new(OpenLarkOptions {
            app_id: env::var("FEISHU_APP_ID").expect("FEISHU_APP_ID"),
            app_secret: env::var("FEISHU_APP_SECRET").expect("FEISHU_APP_SECRET"),
            user_access_token: env::var("FEISHU_USER_ACCESS_TOKEN").ok(),
            api_base_url: link.inferred_api_base_url().into(),
            allow_custom_api_base_url: false,
            max_download_bytes: 100 * 1024 * 1024,
            request_timeout: Duration::from_secs(30),
        })
        .unwrap(),
    );
    let filesystem = FeishuFs::connect(source.clone(), link, FsOptions::default()).unwrap();
    let mut inode = ROOT_INODE;
    for component in path.split('/') {
        inode = filesystem.lookup(inode, component.as_bytes()).unwrap().0;
    }
    let metadata = filesystem.metadata(inode).unwrap();
    let mut bytes = vec![0; metadata.size as usize];
    let count = filesystem.read(inode, 0, &mut bytes).unwrap();
    bytes.truncate(count);
    let body = String::from_utf8(bytes).unwrap();
    assert!(
        body.contains("```mermaid"),
        "selected document must contain Mermaid source"
    );
    assert!(body.contains("flowchart LR"));
    assert!(!body.contains("<cite"));
    assert!(!body.contains(r"\&\#34;"));
    assert!(
        body.contains("](<"),
        "local relative document links expected"
    );
    fs::create_dir_all(&output).unwrap();
    fs::write(format!("{output}/homepage.md"), &body).unwrap();
    if let Ok(token) = env::var("FEISHUFS_LIVE_BOARD_TOKEN") {
        let image = source
            .fetch_content(&ContentRequest::Whiteboard { token })
            .unwrap();
        assert!(image.starts_with(b"\xff\xd8\xff") || image.starts_with(b"\x89PNG"));
        fs::write(format!("{output}/board.image"), image).unwrap();
    }
    println!("Verified native Rust document read, local links and Mermaid; artifacts: {output}");
}
