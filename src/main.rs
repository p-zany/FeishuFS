use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use feishufs::{FeishuFs, FsOptions, KnowledgeLink, OpenLarkOptions, OpenLarkSource};
use fsk::{MountOptions, MountSession};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "feishufs",
    version,
    about = "将飞书/Lark 知识库挂载为本地只读文件系统"
)]
struct Cli {
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 验证链接和权限，并预览文件系统目录树
    Inspect(CommonArgs),
    /// 将知识库挂载到本地目录
    Mount(MountArgs),
}

#[derive(Clone, Debug, Args)]
struct CommonArgs {
    /// 知识库空间内任意文档的链接（用于定位要挂载的整个知识库）
    wiki_url: String,

    /// 飞书自建应用 App ID
    #[arg(long, env = "FEISHU_APP_ID")]
    app_id: String,

    /// 飞书自建应用 App Secret（推荐只通过环境变量传入）
    #[arg(long, env = "FEISHU_APP_SECRET", hide_env_values = true)]
    app_secret: String,

    /// 可选的用户访问令牌
    #[arg(long, env = "FEISHU_USER_ACCESS_TOKEN", hide_env_values = true)]
    user_access_token: Option<String>,

    /// OpenAPI base URL；默认根据飞书/Lark 链接自动判断
    #[arg(long, env = "FEISHU_API_BASE_URL")]
    api_base_url: Option<String>,

    /// 允许非飞书官方域名的 API base URL
    #[arg(long, default_value_t = false)]
    allow_custom_api_base_url: bool,

    /// 目录与内容缓存刷新周期
    #[arg(long, default_value = "60s", value_parser = parse_duration)]
    refresh_interval: Duration,

    /// 单次挂载允许的最大知识库节点数
    #[arg(long, default_value_t = 10_000)]
    max_nodes: usize,

    /// 单个下载/导出文件的最大 MiB
    #[arg(long, default_value_t = 100)]
    max_download_mib: usize,

    /// 单个 OpenAPI 请求超时
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    request_timeout: Duration,
}

#[derive(Debug, Args)]
struct MountArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// 本地挂载目录
    mountpoint: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(error = %format!("{error:#}"), "命令失败");
            eprintln!("错误：{error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<()> {
    match command {
        Command::Inspect(args) => {
            let filesystem = connect(&args)?;
            println!("知识库：{}", filesystem.root_title());
            println!("节点数：{}", filesystem.node_count());
            println!("映射预览：");
            for line in filesystem.tree_lines() {
                println!("  {line}");
            }
            Ok(())
        }
        Command::Mount(args) => mount(args),
    }
}

fn connect(args: &CommonArgs) -> Result<FeishuFs> {
    if args.refresh_interval.is_zero() {
        bail!("--refresh-interval 必须大于 0");
    }
    let link: KnowledgeLink = args.wiki_url.parse().context("知识库链接无效")?;
    let max_download_bytes = args
        .max_download_mib
        .checked_mul(1024 * 1024)
        .context("--max-download-mib 数值过大")?;
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
            cache_ttl: args.refresh_interval,
            max_nodes: args.max_nodes,
        },
    )
}

fn mount(args: MountArgs) -> Result<()> {
    validate_mountpoint(&args.mountpoint)?;
    let filesystem = connect(&args.common)?;
    let session = mount_session(filesystem.clone(), &args.mountpoint)
        .with_context(|| platform_mount_hint(&args.mountpoint))?;
    let _refresher = filesystem.start_refresher(args.common.refresh_interval);

    println!("已挂载：{}", args.mountpoint.display());
    println!("知识库：{}", filesystem.root_title());
    println!(
        "刷新周期：{}",
        humantime::format_duration(args.common.refresh_interval)
    );
    println!("按 Ctrl-C 卸载并退出。");
    info!(mountpoint = %args.mountpoint.display(), "FeishuFS 挂载成功");

    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = shutdown_tx.send(());
    })
    .context("无法安装 Ctrl-C 处理器")?;
    shutdown_rx.recv().context("退出信号通道异常关闭")?;
    drop(session);
    info!("FeishuFS 已卸载");
    Ok(())
}

fn validate_mountpoint(path: &Path) -> Result<()> {
    if path.exists() && !path.is_dir() {
        bail!("挂载点 {} 已存在但不是目录", path.display());
    }
    #[cfg(target_os = "windows")]
    if path.exists() && path.read_dir()?.next().is_some() {
        bail!(
            "Windows ProjFS 挂载点必须为空；{} 当前包含文件",
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
        "当前版本只发布 Linux 和 Windows 挂载后端",
    ))
}

#[cfg(target_os = "linux")]
fn platform_mount_hint(mountpoint: &Path) -> String {
    format!(
        "无法在 {} 建立 FUSE 挂载；请确认 /dev/fuse 可用且已安装 fuse3/fusermount3",
        mountpoint.display()
    )
}

#[cfg(target_os = "windows")]
fn platform_mount_hint(mountpoint: &Path) -> String {
    format!(
        "无法在 {} 建立 ProjFS 投影；请以管理员 PowerShell 启用 Client-ProjFS，并使用空的 NTFS 目录",
        mountpoint.display()
    )
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn platform_mount_hint(mountpoint: &Path) -> String {
    format!("当前平台不支持挂载 {}", mountpoint.display())
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

fn init_tracing(verbose: u8) {
    let fallback = match verbose {
        0 => "feishufs=info",
        1 => "feishufs=debug",
        _ => "feishufs=trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
