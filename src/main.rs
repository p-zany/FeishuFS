use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use feishufs::{
    ASSETS_DIRECTORY, FeishuFs, FsOptions, KnowledgeLink, OpenLarkOptions, OpenLarkSource, logging,
};
use fsk::{MountOptions, MountSession};
use tracing::{debug, error, info};

use crate::export::WriteMode;

mod export;

#[derive(Debug, Parser)]
#[command(
    name = "feishufs",
    version,
    about = "Mount or export a Feishu/Lark space as local files"
)]
struct Cli {
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Detailed log file (default: feishufs.log in the working directory)
    #[arg(long, env = "FEISHUFS_LOG_FILE", global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,

    /// Disable the log file and write only to the console
    #[arg(long, global = true)]
    no_log_file: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate permissions and preview the filesystem tree
    Inspect(CommonArgs),
    /// Mount a space at a local directory
    Mount(MountArgs),
    /// Export a space to a new directory without mounting
    ///
    /// The destination must not exist. Parent directories are created as needed.
    Export(OutputArgs),
    /// Update local files once, overwriting matching paths and keeping extra files
    ///
    /// The destination is created if missing. Local edits at matching paths are overwritten.
    Sync(OutputArgs),
}

#[derive(Clone, Debug, Args)]
struct CommonArgs {
    /// A document URL used to select the entire space at startup
    wiki_url: String,

    /// Feishu custom application ID
    #[arg(long, env = "FEISHU_APP_ID")]
    app_id: String,

    /// Feishu application secret (prefer an environment variable)
    #[arg(long, env = "FEISHU_APP_SECRET", hide_env_values = true)]
    app_secret: String,

    /// Optional user access token
    #[arg(long, env = "FEISHU_USER_ACCESS_TOKEN", hide_env_values = true)]
    user_access_token: Option<String>,

    /// OpenAPI base URL (inferred from the Feishu/Lark URL by default)
    #[arg(long, env = "FEISHU_API_BASE_URL")]
    api_base_url: Option<String>,

    /// Allow an API base URL outside the official Feishu/Lark domains
    #[arg(long, default_value_t = false)]
    allow_custom_api_base_url: bool,

    /// Delay between completed directory refresh attempts
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    refresh_interval: Duration,

    /// Content cache lifetime, independent of the directory refresh interval
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    content_ttl: Duration,

    /// Cooldown after a failed content fetch, including first-fetch failures
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    content_retry_interval: Duration,

    /// Maximum number of nodes in a space
    #[arg(long, default_value_t = 10_000)]
    max_nodes: usize,

    /// Maximum download/export size per file in MiB
    #[arg(long, default_value_t = 100)]
    max_download_mib: usize,

    /// Timeout for each OpenAPI request
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    request_timeout: Duration,
}

#[derive(Debug, Args)]
struct MountArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Local mountpoint
    mountpoint: PathBuf,

    /// Internal disk backing cache (default: the user's cache directory)
    #[arg(long = "cache-dir", alias = "asset-dir", value_name = "PATH")]
    cache_directory: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct OutputArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Local output directory
    destination: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let log_file = if cli.no_log_file {
        None
    } else {
        Some(
            cli.log_file
                .clone()
                .unwrap_or_else(|| PathBuf::from(logging::DEFAULT_LOG_FILE)),
        )
    };
    let log_path = logging::init(cli.verbose, log_file.as_deref());
    match run(cli.command, log_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(error = %format!("{error:#}"), "Command failed");
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, log_path: Option<PathBuf>) -> Result<()> {
    match command {
        Command::Inspect(args) => {
            let filesystem = connect(&args, None)?;
            println!("Space: {}", filesystem.root_title());
            println!("Nodes: {}", filesystem.node_count());
            println!("Filesystem preview:");
            for line in filesystem.tree_lines() {
                println!("  {line}");
            }
            Ok(())
        }
        Command::Mount(args) => mount(args, log_path),
        Command::Export(args) => copy_space(args, WriteMode::CreateNew),
        Command::Sync(args) => copy_space(args, WriteMode::Update),
    }
}

fn copy_space(args: OutputArgs, mode: WriteMode) -> Result<()> {
    let filesystem = connect(&args.common, None)?;
    let copied = export::write(&filesystem, &args.destination, mode)?;
    println!("Space: {}", filesystem.root_title());
    println!("Files copied: {copied}");
    println!("Output: {}", args.destination.display());
    Ok(())
}

fn connect(args: &CommonArgs, asset_directory: Option<PathBuf>) -> Result<FeishuFs> {
    let link: KnowledgeLink = args.wiki_url.parse().context("Invalid space URL")?;
    let max_download_bytes = args
        .max_download_mib
        .checked_mul(1024 * 1024)
        .context("--max-download-mib is too large")?;
    let api_base_url = args
        .api_base_url
        .clone()
        .unwrap_or_else(|| link.inferred_api_base_url().to_owned());
    let source = OpenLarkSource::new(OpenLarkOptions {
        app_id: args.app_id.clone(),
        app_secret: args.app_secret.clone(),
        user_access_token: args.user_access_token.clone(),
        api_base_url,
        allow_custom_api_base_url: args.allow_custom_api_base_url,
        max_download_bytes,
        request_timeout: args.request_timeout,
    })?;
    FeishuFs::connect(
        Arc::new(source),
        link,
        FsOptions {
            asset_directory,
            cache_ttl: args.content_ttl,
            content_retry_interval: args.content_retry_interval,
            max_nodes: args.max_nodes,
            ..FsOptions::default()
        },
    )
}

fn mount(args: MountArgs, log_path: Option<PathBuf>) -> Result<()> {
    validate_mountpoint(&args.mountpoint)?;
    let cache_directory = match args.cache_directory {
        Some(directory) => directory,
        None => default_cache_directory()?,
    };
    let cache_directory = validate_asset_directory(&cache_directory, &args.mountpoint)?;
    let filesystem = connect(&args.common, Some(cache_directory))?;
    let session = mount_session(filesystem.clone(), &args.mountpoint)
        .with_context(|| platform_mount_hint(&args.mountpoint))?;
    let refresher = filesystem.start_refresher(args.common.refresh_interval);

    println!("Mounted: {}", args.mountpoint.display());
    println!("Space: {}", filesystem.root_title());
    println!(
        "Directory refresh delay: {}",
        humantime::format_duration(args.common.refresh_interval)
    );
    println!(
        "Content TTL: {}",
        humantime::format_duration(args.common.content_ttl)
    );
    println!(
        "Assets directory: {}",
        args.mountpoint.join(ASSETS_DIRECTORY).display()
    );
    if let Some(path) = filesystem.asset_directory() {
        debug!(path = %path.display(), "Asset backing cache enabled");
    }
    match &log_path {
        Some(path) => println!("Log file: {}", path.display()),
        None => println!("Logging: console only (log file disabled)"),
    }
    println!("Press Ctrl-C to unmount and exit.");
    info!(mountpoint = %args.mountpoint.display(), "FeishuFS mounted");

    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = shutdown_tx.send(());
    })
    .context("Failed to install the Ctrl-C handler")?;
    shutdown_rx
        .recv()
        .context("Shutdown signal channel closed unexpectedly")?;
    // Stop workers while their platform invalidation context is still alive.
    drop(refresher);
    drop(session);
    info!("FeishuFS unmounted");
    Ok(())
}

fn default_cache_directory() -> Result<PathBuf> {
    user_cache_directory(
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn user_cache_directory(cache_home: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf> {
    let directory = cache_home
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".cache"))
        })
        .context("No user cache directory is available; specify --cache-dir")?;
    Ok(directory.join("feishufs"))
}

fn validate_asset_directory(directory: &Path, mountpoint: &Path) -> Result<PathBuf> {
    let directory = resolve_path(directory)?;
    let mountpoint = resolve_path(mountpoint)?;
    if directory.starts_with(&mountpoint) || mountpoint.starts_with(&directory) {
        bail!("The internal cache directory and mountpoint must not overlap");
    }
    Ok(directory)
}

fn resolve_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path).context("Failed to resolve an absolute path")?;
    match absolute.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = absolute.parent().context("Path has no existing parent")?;
            let name = absolute.file_name().context("Path has no filename")?;
            Ok(resolve_path(parent)?.join(name))
        }
        Err(error) => {
            Err(error).with_context(|| format!("Failed to resolve path {}", path.display()))
        }
    }
}

fn validate_mountpoint(path: &Path) -> Result<()> {
    if path.exists() && !path.is_dir() {
        bail!(
            "Mountpoint {} exists but is not a directory",
            path.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_session(filesystem: FeishuFs, mountpoint: &Path) -> std::io::Result<MountSession> {
    let options = MountOptions {
        read_only: true,
        filesystem_name: "feishufs".into(),
        metadata_ttl: Duration::from_secs(1),
        ..MountOptions::default()
    };
    MountSession::mount_network_with_options(filesystem, mountpoint, &options)
}

#[cfg(not(target_os = "linux"))]
fn mount_session(_filesystem: FeishuFs, _mountpoint: &Path) -> std::io::Result<MountSession> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Only the Linux FUSE backend is supported",
    ))
}

#[cfg(target_os = "linux")]
fn platform_mount_hint(mountpoint: &Path) -> String {
    format!(
        "Failed to mount FUSE at {}; check /dev/fuse and fuse3/fusermount3 availability",
        mountpoint.display()
    )
}

#[cfg(not(target_os = "linux"))]
fn platform_mount_hint(mountpoint: &Path) -> String {
    format!(
        "Mounting {} is not supported on this platform",
        mountpoint.display()
    )
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    if duration.is_zero()
        || Instant::now()
            .checked_add(duration.saturating_mul(8))
            .is_none()
    {
        return Err("Duration must be positive and representable".into());
    }
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_and_sync_help_describe_their_own_destination_policy() {
        let create_new = "The destination must not exist.";
        let update = "The destination is created if missing.";
        for (command, expected, unrelated) in
            [("export", create_new, update), ("sync", update, create_new)]
        {
            let error = Cli::try_parse_from(["feishufs", command, "--help"]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
            let help = error.to_string();
            assert!(help.contains(expected), "{help}");
            assert!(!help.contains(unrelated), "{help}");
            assert!(!help.contains("must not exist for export"), "{help}");
        }
    }

    #[test]
    fn export_and_sync_accept_a_destination_and_existing_connection_options() {
        for command in ["export", "sync"] {
            let result = Cli::try_parse_from([
                "feishufs",
                command,
                "https://example.feishu.cn/wiki/doc",
                "output",
                "--app-id",
                "test-app",
                "--app-secret",
                "test-secret",
                "--max-download-mib",
                "20",
                "--no-log-file",
            ]);
            assert!(result.is_ok(), "{result:?}");
        }
    }

    #[test]
    fn directory_interval_and_content_ttl_are_independent() {
        let cli = Cli::try_parse_from([
            "feishufs",
            "inspect",
            "https://example.feishu.cn/wiki/doc",
            "--app-id",
            "test-app",
            "--app-secret",
            "test-secret",
            "--refresh-interval",
            "5m",
            "--content-ttl",
            "30s",
            "--content-retry-interval",
            "2s",
        ])
        .unwrap();
        let Command::Inspect(args) = cli.command else {
            panic!("Inspect expected")
        };
        assert_eq!(args.refresh_interval, Duration::from_secs(300));
        assert_eq!(args.content_ttl, Duration::from_secs(30));
        assert_eq!(args.content_retry_interval, Duration::from_secs(2));
    }

    #[test]
    fn durations_must_be_positive() {
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("invalid").is_err());
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn mount_does_not_create_a_second_assets_directory_in_the_working_directory() {
        let cli = Cli::try_parse_from([
            "feishufs",
            "mount",
            "https://example.feishu.cn/wiki/doc",
            "wiki",
            "--app-id",
            "test-app",
            "--app-secret",
            "test-secret",
        ])
        .unwrap();
        let Command::Mount(args) = cli.command else {
            panic!("Mount expected")
        };
        assert_eq!(args.cache_directory, None);
    }

    #[test]
    fn mount_accepts_an_explicit_internal_cache_directory() {
        for flag in ["--cache-dir", "--asset-dir"] {
            let cli = Cli::try_parse_from([
                "feishufs",
                "mount",
                "https://example.feishu.cn/wiki/doc",
                "wiki",
                "--app-id",
                "test-app",
                "--app-secret",
                "test-secret",
                flag,
                "private-cache",
            ])
            .unwrap();
            let Command::Mount(args) = cli.command else {
                panic!("Mount expected")
            };
            assert_eq!(args.cache_directory, Some(PathBuf::from("private-cache")));
        }
    }

    #[test]
    fn default_cache_uses_user_locations_not_the_working_directory_or_shared_temp() {
        let temporary = tempfile::tempdir().unwrap();
        let cache_home = temporary.path().join("user-cache");
        let home = temporary.path().join("user-home");
        assert_eq!(
            user_cache_directory(Some(cache_home.clone()), Some(home.clone())).unwrap(),
            cache_home.join("feishufs")
        );
        for cache in [None, Some(PathBuf::from("relative-cache"))] {
            assert_eq!(
                user_cache_directory(cache, Some(home.clone())).unwrap(),
                home.join(".cache/feishufs")
            );
        }
        assert!(user_cache_directory(None, None).is_err());
        assert!(
            user_cache_directory(
                Some(PathBuf::from("relative-cache")),
                Some(PathBuf::from("relative-home"))
            )
            .is_err()
        );
        assert_eq!(temporary.path().read_dir().unwrap().count(), 0);
    }

    #[test]
    fn disk_assets_and_mountpoint_cannot_overlap() {
        let temporary = tempfile::tempdir().unwrap();
        let mountpoint = temporary.path().join("wiki");
        let directory = temporary.path().join(".feishufs-assets");
        assert_eq!(
            validate_asset_directory(&directory, &mountpoint).unwrap(),
            directory
        );
        assert!(validate_asset_directory(&mountpoint, &mountpoint).is_err());
        assert!(
            validate_asset_directory(&mountpoint.join(".feishufs-assets"), &mountpoint).is_err()
        );
        assert!(validate_asset_directory(temporary.path(), &mountpoint).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn disk_assets_overlap_check_resolves_parent_symlinks() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let mountpoint = temporary.path().join("wiki");
        std::fs::create_dir(&mountpoint).unwrap();
        let alias = temporary.path().join("alias");
        symlink(&mountpoint, &alias).unwrap();
        assert!(validate_asset_directory(&alias.join(".feishufs-assets"), &mountpoint).is_err());
    }
}
