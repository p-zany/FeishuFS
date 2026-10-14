//! Atomic, private disk copies of successfully downloaded attachments and images.
//! The backing directory is outside the read-only mount; links use virtual paths.

use std::{
    fs::{self, DirBuilder},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{docx::valid_token, remote::ContentRequest, tree::sanitize_component};

pub(crate) struct AssetStore {
    root: PathBuf,
}

impl AssetStore {
    pub fn new(directory: &Path, host: &str, space: &str) -> Result<Self> {
        if sanitize_component(host) != host || !valid_token(space) {
            bail!("Invalid asset store namespace");
        }
        create_private_directory(directory)?;
        let directory = directory
            .canonicalize()
            .context("Failed to resolve the asset backing directory")?;
        let host_directory = directory.join(host);
        create_private_directory(&host_directory)?;
        let root = host_directory.join(space);
        create_private_directory(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, request: &ContentRequest, name: &str) -> Result<PathBuf> {
        let (kind, token) = match request {
            ContentRequest::Download { token } => ("files", token),
            ContentRequest::Whiteboard { token } => ("whiteboards", token),
            _ => bail!("Only downloaded attachments and images have disk asset paths"),
        };
        if !valid_token(token)
            || name.is_empty()
            || sanitize_component(name) != name
            || matches!(name, "." | "..")
        {
            bail!("Invalid disk asset identity or filename");
        }
        let encoded: String = token.bytes().map(|byte| format!("{byte:02x}")).collect();
        Ok(self.root.join(kind).join(encoded).join(name))
    }

    pub fn persist(&self, request: &ContentRequest, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path(request, name)?;
        let token_directory = path.parent().expect("asset token directory");
        let kind_directory = token_directory.parent().expect("asset kind directory");
        create_private_directory(kind_directory)?;
        create_private_directory(token_directory)?;
        // Write beside the destination so publication is an atomic rename, not
        // truncation of the last good copy. A failed write drops its temp file.
        let mut temporary = tempfile::Builder::new()
            .prefix(".pending-")
            .tempfile_in(token_directory)
            .context("Failed to create a temporary disk asset")?;
        temporary
            .write_all(bytes)
            .context("Failed to write the downloaded disk asset")?;
        temporary
            .as_file()
            .sync_all()
            .context("Failed to flush the downloaded disk asset")?;
        temporary
            .persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("Failed to publish disk asset {}", path.display()))?;
        Ok(())
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "Asset directory is not a real directory: {}",
                path.display()
            );
        }
        return Ok(());
    }
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("Failed to create asset directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(token: &str) -> ContentRequest {
        ContentRequest::Download {
            token: token.into(),
        }
    }

    #[test]
    fn persists_binary_bytes_and_atomically_replaces_the_previous_copy() {
        let temporary = tempfile::tempdir().unwrap();
        let store = AssetStore::new(temporary.path(), "example.feishu.cn", "space-1").unwrap();
        let request = download("Aa");
        let path = store.path(&request, "报告.pdf").unwrap();
        assert!(path.ends_with("files/4161/报告.pdf"));
        store
            .persist(&request, "报告.pdf", b"%PDF-1.7\0first")
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"%PDF-1.7\0first");
        store
            .persist(&request, "报告.pdf", b"%PDF-1.7\0updated")
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"%PDF-1.7\0updated");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
    }

    #[test]
    fn isolates_hosts_spaces_token_case_and_image_requests() {
        let temporary = tempfile::tempdir().unwrap();
        let a = AssetStore::new(temporary.path(), "a.feishu.cn", "space-1").unwrap();
        let b = AssetStore::new(temporary.path(), "a.feishu.cn", "space-2").unwrap();
        let c = AssetStore::new(temporary.path(), "b.feishu.cn", "space-1").unwrap();
        let request = download("A");
        assert_ne!(
            a.path(&request, "file.pdf").unwrap(),
            b.path(&request, "file.pdf").unwrap()
        );
        assert_ne!(
            a.path(&request, "file.pdf").unwrap(),
            c.path(&request, "file.pdf").unwrap()
        );
        assert_ne!(
            a.path(&request, "file.pdf").unwrap(),
            a.path(&download("a"), "file.pdf").unwrap()
        );
        let board = ContentRequest::Whiteboard { token: "A".into() };
        assert_ne!(
            a.path(&request, "image.png").unwrap(),
            a.path(&board, "image.png").unwrap()
        );
    }

    #[test]
    fn rejects_traversal_and_invalid_tokens_without_writing_files() {
        let temporary = tempfile::tempdir().unwrap();
        let store = AssetStore::new(temporary.path(), "example.feishu.cn", "space-1").unwrap();
        for name in [
            "../escape.pdf",
            "dir/file.pdf",
            "dir\\file.pdf",
            ".",
            "..",
            "",
        ] {
            assert!(store.persist(&download("token"), name, b"bad").is_err());
        }
        assert!(
            store
                .persist(&download("../token"), "file.pdf", b"bad")
                .is_err()
        );
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_asset_directories() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = AssetStore::new(temporary.path(), "example.feishu.cn", "space-1").unwrap();
        symlink(outside.path(), store.root().join("files")).unwrap();
        assert!(
            store
                .persist(&download("token"), "file.pdf", b"bad")
                .is_err()
        );
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
