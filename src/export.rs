use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use feishufs::{ASSETS_DIRECTORY, FeishuFs};
use fsk::{DirectoryEntry, DirectorySink, FileType, Filesystem, ROOT_INODE};

#[derive(Clone, Copy, Debug)]
pub enum WriteMode {
    CreateNew,
    Update,
}

/// Copy a fresh, unmounted filesystem view. No refresher may run during the copy.
/// Sync overwrites matching files and leaves all other local paths in place.
pub fn write(filesystem: &FeishuFs, destination: &Path, mode: WriteMode) -> Result<usize> {
    // Remove trailing separators so symlink_metadata inspects the destination itself.
    let destination: PathBuf = destination.components().collect();
    match mode {
        WriteMode::CreateNew => {
            if let Some(parent) = destination
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
            {
                fs::create_dir_all(parent)?;
            }
            fs::create_dir(&destination).with_context(|| {
                format!("Export requires a new directory: {}", destination.display())
            })?;
        }
        WriteMode::Update => ensure_directory(&destination)?,
    }

    let mut copied = 0;
    // Reading Markdown can create the assets directory or add images to it.
    // List it only after every ordinary file has been read.
    for assets in [false, true] {
        let mut pending = vec![(ROOT_INODE, destination.clone())];
        while let Some((inode, directory)) = pending.pop() {
            for entry in entries(filesystem, inode)
                .with_context(|| format!("Failed to list {}", directory.display()))?
            {
                if inode == ROOT_INODE && (entry.name == ASSETS_DIRECTORY.as_bytes()) != assets {
                    continue;
                }
                let name =
                    std::str::from_utf8(&entry.name).context("Invalid filesystem filename")?;
                let path = directory.join(name);
                match entry.kind {
                    FileType::Directory => {
                        ensure_directory(&path)?;
                        pending.push((entry.inode, path));
                    }
                    FileType::File => {
                        copy_file(filesystem, entry.inode, &path, mode)
                            .with_context(|| format!("Failed to copy {}", path.display()))?;
                        copied += 1;
                    }
                    _ => bail!("Unsupported file type at {}", path.display()),
                }
            }
        }
    }
    Ok(copied)
}

fn ensure_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => bail!("Output path is not a real directory: {}", path.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => fs::create_dir_all(path)
            .with_context(|| format!("Failed to create directory {}", path.display())),
        Err(error) => Err(error).with_context(|| format!("Failed to inspect {}", path.display())),
    }
}

fn copy_file(filesystem: &FeishuFs, inode: u64, path: &Path, mode: WriteMode) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => bail!("Output path is not a regular file"),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Failed to inspect output file"),
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".feishufs-pending-")
        .tempfile_in(path.parent().context("Output file has no parent")?)?;
    let mut buffer = [0; 64 * 1024];
    let mut offset = 0;
    loop {
        // Avoid metadata/open: they refresh expired content. Read each identity
        // once, including attachments shared by original and asset paths.
        let count = filesystem
            .read(inode, offset, &mut buffer)
            .map_err(|error| anyhow!("Content read failed (filesystem error {})", error.0))?;
        if count == 0 {
            break;
        }
        temporary.write_all(&buffer[..count])?;
        offset += count as u64;
    }
    temporary.as_file().sync_all()?;
    match mode {
        WriteMode::CreateNew => temporary.persist_noclobber(path),
        WriteMode::Update => temporary.persist(path),
    }
    .map_err(|error| error.error)
    .context("Failed to publish output file")?;
    Ok(())
}

struct Entry {
    name: Vec<u8>,
    inode: u64,
    kind: FileType,
}

#[derive(Default)]
struct Entries(Vec<Entry>);

impl DirectorySink for Entries {
    fn push(&mut self, entry: DirectoryEntry<'_>) -> bool {
        self.0.push(Entry {
            name: entry.name.to_vec(),
            inode: entry.inode,
            kind: entry.kind,
        });
        true
    }
}

fn entries(filesystem: &FeishuFs, inode: u64) -> Result<Vec<Entry>> {
    let mut entries = Entries::default();
    filesystem
        .read_directory(inode, 0, 0, &mut entries)
        .map_err(|error| anyhow!("Directory read failed (filesystem error {})", error.0))?;
    Ok(entries.0)
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
