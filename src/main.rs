use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use feishufs::{FeishuFs, FsOptions, KnowledgeLink, OpenLarkOptions, OpenLarkSource, logging};
use fsk::{MountOptions, MountSession};
use tracing::{error, info};

#[derive(Debug, Parser)]
#[command(
    name = "feishufs",
    version,
    about = "Mount a Feishu/Lark space as a read-only filesystem"
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

    /// Maximum number of nodes in a mounted space
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
            let filesystem = connect(&args)?;
            println!("Space: {}", filesystem.root_title());
            println!("Nodes: {}", filesystem.node_count());
            println!("Filesystem preview:");
            for line in filesystem.tree_lines() {
                println!("  {line}");
            }
            Ok(())
        }
        Command::Mount(args) => mount(args, log_path),
    }
}

fn connect(args: &CommonArgs) -> Result<FeishuFs> {
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
            cache_ttl: args.content_ttl,
            content_retry_interval: args.content_retry_interval,
            max_nodes: args.max_nodes,
            ..FsOptions::default()
        },
    )
}

fn mount(args: MountArgs, log_path: Option<PathBuf>) -> Result<()> {
    validate_mountpoint(&args.mountpoint)?;
    let filesystem = connect(&args.common)?;
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

fn validate_mountpoint(path: &Path) -> Result<()> {
    if path.exists() && !path.is_dir() {
        bail!(
            "Mountpoint {} exists but is not a directory",
            path.display()
        );
    }
    #[cfg(target_os = "windows")]
    if path.exists() && path.read_dir()?.next().is_some() {
        bail!(
            "Windows ProjFS requires an empty mountpoint; {} contains files",
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

#[cfg(target_os = "windows")]
fn mount_session(filesystem: FeishuFs, mountpoint: &Path) -> std::io::Result<MountSession> {
    let options = MountOptions {
        read_only: true,
        filesystem_name: "feishufs".into(),
        use_negative_path_cache: false,
        ..MountOptions::default()
    };
    MountSession::mount_network_with_options(filesystem, mountpoint, &options)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn mount_session(_filesystem: FeishuFs, _mountpoint: &Path) -> std::io::Result<MountSession> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Only Linux and Windows mount backends are supported",
    ))
}

#[cfg(target_os = "linux")]
fn platform_mount_hint(mountpoint: &Path) -> String {
    format!(
        "Failed to mount FUSE at {}; check /dev/fuse and fuse3/fusermount3 availability",
        mountpoint.display()
    )
}

#[cfg(target_os = "windows")]
fn platform_mount_hint(mountpoint: &Path) -> String {
    format!(
        "Failed to start ProjFS at {}; enable Client-ProjFS in administrator PowerShell and use an empty NTFS directory",
        mountpoint.display()
    )
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
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
}
