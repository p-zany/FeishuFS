# 使用指南

**简体中文** | [English](USAGE.en.md) · [返回 README](../README.zh-CN.md)

## Linux 挂载条件

支持 x86_64 Linux，需要可用的 `/dev/fuse`、挂载权限和 `fusermount3`。Ubuntu / Debian 可安装：

```bash
sudo apt-get update
sudo apt-get install fuse3
```

应用凭证和知识库访问授权见[应用授权](AUTHORIZATION.md)，Nix 快速开始见 [README](../README.zh-CN.md#快速开始)。

## 使用发布包

当 [GitHub Releases](https://github.com/p-zany/feishufs/releases) 提供对应版本的 Linux 压缩包时，可下载压缩包与同名 `.sha256` 文件，校验后解压。尚无发布包时，请使用 Nix 或下方的源码构建。

发布包采用静态 musl 链接，无需 Nix、Rust、飞书 CLI、glibc 或 `libgcc_s.so.1`，仍需上述 FUSE 条件。以下以 `v0.1.0` 为例，版本号应与下载文件一致：

```bash
sha256sum --check feishufs-v0.1.0-x86_64-unknown-linux-musl.tar.gz.sha256 && \
  tar -xzf feishufs-v0.1.0-x86_64-unknown-linux-musl.tar.gz
```

在解压目录中，按 [README 的快速开始](../README.zh-CN.md#快速开始)配置凭证，将命令中的 `nix run github:p-zany/feishufs --` 替换为 `./feishufs`：

```bash
./feishufs inspect 'https://example.feishu.cn/wiki/wikcnxxx'
mkdir -p ./mnt
./feishufs mount 'https://example.feishu.cn/wiki/wikcnxxx' ./mnt
```

以前台进程运行，按 `Ctrl-C` 卸载。`inspect` 只读取目录和子表元信息，不下载正文，不能验证所有正文 / 图片的读取权限。

## 从源码构建

需要 Rust 1.98 或更新版本。在取得源码并进入仓库根目录后：

```bash
sudo apt-get update
sudo apt-get install build-essential pkg-config libfuse3-dev fuse3
cargo build --release --locked
```

产物为 `target/release/feishufs`。按 [README 的快速开始](../README.zh-CN.md#快速开始)配置凭证，将其中的 `nix run github:p-zany/feishufs --` 替换为 `./target/release/feishufs`。

也可安装到 Cargo 的二进制目录：

```bash
cargo install --path . --locked
```

确保 `~/.cargo/bin` 在 `PATH` 中，即可直接运行 `feishufs`。Nix 用户见 [README 的快速开始](../README.zh-CN.md#快速开始)；本地构建、开发检查及发布流程见[贡献指南](../CONTRIBUTING.md)。

## 常用参数

| 参数 | 默认值 | 含义 |
| --- | --- | --- |
| `--refresh-interval` | `60s` | 每轮目录刷新完成后的等待间隔 |
| `--content-ttl` | `60s` | 独立的正文与附件缓存 TTL |
| `--content-retry-interval` | `5s` | 内容获取失败后的重试冷却，包含首次失败 |
| `--max-nodes` | `10000` | 知识库节点数上限，不含表格子表数 |
| `--max-download-mib` | `100` | 单文件下载 / 导出上限，单位 MiB；不是总缓存限额 |
| `--request-timeout` | `30s` | 单次 OpenAPI 请求超时，不是整个导出任务超时 |
| `mount --cache-dir` | 用户缓存目录下的 `feishufs` | 内部磁盘缓存位置，不是资产入口；必须与挂载点互不包含 |

资产入口统一为 `<mountpoint>/.feishufs-assets`（例如 `wiki/.feishufs-assets`），不再默认创建工作目录下的同名目录。Markdown 和 CSV 中的已知附件链接都改写到该入口，原附件路径仍可读取。下载成功后保存到内部磁盘缓存；缓存保留到用户手动清理，无总容量限制或跨挂载缓存恢复。默认缓存位置与布局见[资产落盘](PRODUCT.md#资产落盘)。

时间参数接受 `60s`、`5m` 等格式，必须大于零。`inspect` 也接受这些参数，但不执行后台刷新。目录失败后的等待间隔按 2、4、8 倍退避，成功后恢复；内容在冷却结束后的下一次访问重试。

完整选项见 `--help`、`inspect --help` 和 `mount --help`。

## 日志与排障

默认向当前目录的 `feishufs.log` 追加日志，启动时打印绝对路径。后台错误在终端显示摘要，完整错误链写入文件；启动失败也会在终端输出错误链。

- `--log-file <PATH>` 或 `FEISHUFS_LOG_FILE` 指定路径；`--no-log-file` 关闭文件日志。
- 默认终端 `info`、文件 `debug`；`-v` 将终端升为 `debug`，`-vv` 两者均为 `trace`。`RUST_LOG` 覆盖两个出口的过滤规则。
- 仅在启动时，超过 8 MiB 的日志轮转为 `<文件名>.1`，保留一份备份；不是运行中的大小上限。文件打开失败则降级为仅终端输出。

无法定位空间时，检查链接是否属于知识库；权限不足时，同时检查应用权限与空间访问权。目录 / 正文仍旧时，查看刷新和内容获取错误，等待重试后重新打开文件。

报错时提供平台、版本、脱敏命令和相关日志；删除凭证、文档 token、私有标题及正文。

