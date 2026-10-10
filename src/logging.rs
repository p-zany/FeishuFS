//! Console and log-file setup.
//!
//! The console keeps short, human-oriented lines. The log file records the same
//! events at `debug` (or `trace` with `-vv`) and, for every failure, the full
//! `anyhow` error chain. Feishu error codes and response bodies only exist in
//! that chain, so the file is what makes a failure diagnosable.

use std::{
    env,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use anyhow::Error;
use tracing::{debug, info, warn};
use tracing_subscriber::{
    EnvFilter, Layer, fmt, fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt,
};

/// Log file created in the working directory when no path is configured.
pub const DEFAULT_LOG_FILE: &str = "feishufs.log";

/// A log file larger than this is rotated to `<name>.1` when it is reopened.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// Start logging, returning the absolute path of the log file in use.
///
/// A log file that cannot be opened is not fatal: the process keeps running with
/// console output only.
#[must_use]
pub fn init(verbose: u8, log_file: Option<&Path>) -> Option<PathBuf> {
    let console = fmt::layer()
        .compact()
        .with_target(false)
        // Escape codes are only useful on a terminal, and they would otherwise
        // pollute redirected output such as `2> build.log`.
        .with_ansi(io::stderr().is_terminal())
        .with_writer(io::stderr)
        .with_filter(env_filter(console_level(verbose)));

    let file = log_file.and_then(|path| match open_log_file(path, MAX_LOG_BYTES) {
        Ok(file) => Some((absolute(path), file)),
        Err(error) => {
            eprintln!(
                "Warning: failed to open log file {}: {error}; using console output only",
                path.display()
            );
            None
        }
    });

    let subscriber = tracing_subscriber::registry().with(console);
    let Some((path, file)) = file else {
        if subscriber.try_init().is_err() {
            eprintln!("Warning: logging is already initialized; retaining the current settings");
        }
        return None;
    };

    let detail = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .with_writer(FileAppender::new(file))
        .with_filter(env_filter(file_level(verbose)));
    if subscriber.with(detail).try_init().is_err() {
        eprintln!("Warning: logging is already initialized; retaining the current settings");
    }
    info!(path = %path.display(), "Detailed logging enabled");
    Some(path)
}

/// Report a failed operation that is not tied to a specific inode.
///
/// The console keeps the short outer message; the log file also gets the chain.
pub(crate) fn failure(context: &str, error: &Error) {
    warn!(error = %error, "{}", context);
    debug!(error = %format!("{error:#}"), "{}", context);
}

/// Report a failed operation on a knowledge-base node.
pub(crate) fn node_failure(inode: u64, context: &str, error: &Error) {
    warn!(inode, error = %error, "{}", context);
    debug!(inode, error = %format!("{error:#}"), "{}", context);
}

fn console_level(verbose: u8) -> &'static str {
    match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    }
}

/// The file is always at least as detailed as the console.
fn file_level(verbose: u8) -> &'static str {
    match verbose {
        0 | 1 => "debug",
        _ => "trace",
    }
}

fn env_filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("feishufs={level}")))
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_owned();
    }
    env::current_dir().map_or_else(|_| path.to_owned(), |cwd| cwd.join(path))
}

fn open_log_file(path: &Path, max_bytes: u64) -> io::Result<File> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    rotate_log_file(path, max_bytes);
    OpenOptions::new().create(true).append(true).open(path)
}

/// Keep one previous file so a long-running mount cannot grow without bound.
fn rotate_log_file(path: &Path, max_bytes: u64) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() <= max_bytes {
        return;
    }
    let backup = path.with_extension(match path.extension().and_then(OsStr::to_str) {
        Some(extension) => format!("{extension}.1"),
        None => "1".to_owned(),
    });
    let _ = fs::rename(path, backup);
}

/// Serialises log lines from every thread into a single file.
#[derive(Clone)]
struct FileAppender(Arc<Mutex<File>>);

impl FileAppender {
    fn new(file: File) -> Self {
        Self(Arc::new(Mutex::new(file)))
    }
}

impl<'a> MakeWriter<'a> for FileAppender {
    type Writer = FileGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        FileGuard(self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Holds the file lock for one whole event, so concurrent lines never interleave.
struct FileGuard<'a>(MutexGuard<'a, File>);

impl Write for FileGuard<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_stays_shorter_than_the_log_file() {
        assert_eq!(console_level(0), "info");
        assert_eq!(console_level(1), "debug");
        assert_eq!(console_level(2), "trace");

        assert_eq!(file_level(0), "debug");
        assert_eq!(file_level(1), "debug");
        assert_eq!(file_level(2), "trace");
    }

    #[test]
    fn log_file_is_created_and_appended() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("nested/feishufs.log");

        let mut file = open_log_file(&path, 1024).unwrap();
        file.write_all(b"first\n").unwrap();
        let mut file = open_log_file(&path, 1024).unwrap();
        file.write_all(b"second\n").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn oversized_log_file_is_rotated_once() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("feishufs.log");
        fs::write(&path, "x".repeat(64)).unwrap();

        open_log_file(&path, 16).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
        assert_eq!(
            fs::read_to_string(path.with_extension("log.1")).unwrap(),
            "x".repeat(64)
        );
    }

    #[test]
    fn small_log_file_is_not_rotated() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("feishufs.log");
        fs::write(&path, "keep").unwrap();

        open_log_file(&path, 1024).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep");
        assert!(!path.with_extension("log.1").exists());
    }

    #[test]
    fn appender_writes_through_the_shared_file() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("feishufs.log");
        let appender = FileAppender::new(open_log_file(&path, 1024).unwrap());

        appender.make_writer().write_all(b"line\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "line\n");
    }

    #[test]
    fn relative_paths_are_reported_as_absolute() {
        let path = absolute(Path::new("feishufs.log"));
        assert!(path.is_absolute());
        assert!(path.ends_with("feishufs.log"));
    }
}
