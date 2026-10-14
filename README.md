# FeishuFS

将飞书/Lark 知识库挂载为本地只读文件系统，让 AI Agent 用已有的文件工具检索和读取文档。

## 为什么使用

- **统一工作区**：文档与代码、数据及其他工作材料处于同一目录体系，知识与工作环境不再分裂。
- **先检索，再精读**：跨文档搜索，按行或范围读取相关片段，减少无关上下文，也减少逐篇选择文档造成的遗漏。
- **无需专用 skill**：Agent 沿用已有的目录浏览、搜索和文件读取工具，不增加飞书专用操作说明。
- **复用缓存**：FeishuFS 集中处理云端获取与缓存，多个工具或 Agent 可以共用同一挂载，减少重复请求。
- **自由选择模型**：不绑定特定模型；能访问挂载目录的 Agent 即可使用。

## 当前能力

- 输入知识库内任意文档链接，挂载该文档所在的整个可见空间。
- Docx 转 Markdown，旧版文档导出 DOCX，表格按子表输出 CSV，文件保留原始内容。
- 挂载范围内的文档链接改写为相对本地路径。
- 只读访问，不将本地修改同步回飞书。

内容转换和刷新边界见[产品说明](docs/PRODUCT.md)。

## 应用授权

### 创建自建应用

1. 登录[飞书开发者后台](https://open.feishu.cn/app)，创建企业自建应用；Lark 用户在对应的 Lark 开发者后台创建。流程见官方[企业自建应用开发流程](https://open.feishu.cn/document/home/introduction-to-custom-app-development/self-built-application-development-process)。
2. 在应用的 **凭证与基础信息** 页面取得 App ID、App Secret，供下方环境变量使用；不要公开或提交这些凭证。
3. 若采用下方的群授权方式，在 **添加应用能力** 中添加 **机器人**，使自建应用可作为应用机器人加入群聊。机器人不是所有授权方式的必需条件。

这里使用的是有 App ID / App Secret 的自建应用。群聊中通过 Webhook 推送消息的“自定义机器人”不提供知识库数据访问能力，不能代替此应用；两者区别见官方[机器人概述](https://open.feishu.cn/document/client-docs/bot-v3/bot-overview)。FeishuFS 无需配置消息接收或机器人事件订阅。

### 配置接口权限

在 **开发配置 > 权限管理** 中按所需能力申请权限。以下是读取 / 导出能力提示，不是完整的最低权限清单；具体要求以接口文档及控制台为准。

| 内容 | 权限提示 |
| --- | --- |
| 知识库 | 空间与节点读取，如 `wiki:wiki:readonly` |
| Docx | 文档正文读取，如 `docx:document:readonly`；正文接口的实际要求以控制台为准 |
| 电子表格 | [工作表读取](https://open.feishu.cn/document/server-docs/docs/sheets-v3/spreadsheet-sheet/query)，如 `sheets:spreadsheet:readonly`；下方 JSON 未包含此类权限，CSV 还需导出权限 |
| 多维表格 | 数据表读取，如 `bitable:app:readonly`；CSV 还需导出权限 |
| 旧版文档 / 表格 CSV | [云文档导出](https://open.feishu.cn/document/server-docs/docs/drive-v1/export_task/create)，如 `docs:document:export` |
| 文件 | [云空间文件下载](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/reference/drive-v1/file/download)，如 `drive:file:download`；下方 JSON 未包含此类权限，还需目标文件的下载授权 |
| 无 Mermaid 源码的画板 | `board:whiteboard:node:read` |

[原始权限 JSON](misc/feishu-scopes.json)是用户提供的**现有应用权限清单**，仅保存权限名：`tenant` 含创建 / 写入权限，`user` 为空。它**不是最低只读清单，也不保证覆盖所有功能**。FeishuFS 不调用云端文档写 API，但如此广权限的凭证仍可被其他程序用于写入；新应用应按实际需要选择权限。

如需复用该清单，在 **开发配置 > 权限管理 > 批量导入/导出权限 > 导入** 中粘贴 JSON，点击 **下一步，确认新增权限**，核对后点击 **申请开通**。导入只新增申请，不移除已有权限；需审核的权限仍须发布与审批。官方流程见[申请 API 权限](https://open.feishu.cn/document/ukTMukTMukTM/uQjN3QjL0YzN04CN2cDN)。使用用户访问凭证时，需另按接口要求申请用户身份权限，不能把该 JSON 的空 `user` 清单视为已授权。

### 发布与知识库访问授权

在 **应用发布 > 版本管理与发布** 创建并申请发布应用版本，确认能力、权限及可用范围；需审核的权限等管理员批准后生效。群授权时，可用范围应包含知识库资源所有者及添加机器人的人员。免审和测试规则见官方[发布与审核自建应用](https://open.feishu.cn/document/home/intro-to-custom-app-review)。

**接口权限不等于具体知识库 / 文档的访问权**。默认以应用身份访问，还需知识库管理员或文档管理者授权。官方[知识库常见问题](https://open.feishu.cn/document/server-docs/docs/wiki-v2/wiki-qa)列出了授权方式：

- **整个空间的群授权**：将已发布的应用作为应用机器人加入群聊；知识库管理员在 **知识库设置 > 成员设置** 添加该群为可阅读成员。不要在此使用 Webhook 自定义机器人，也无需为只读使用授予管理员或可编辑角色。
- **节点文档授权**：文档所有者、拥有管理权限的协作者或知识库管理员，可在文档 **… > …更多 > 添加文档应用** 中添加应用。仅节点授权不代表能访问整个知识空间。已有应用成员授权、文档应用授权或使用已获授权的用户身份时，按对应身份检查权限，不必一律开启机器人。

### 配置凭证

推荐使用环境变量，不要把密钥写入命令行或提交到 Git。

| 环境变量 | 对应参数 | 用途 |
| --- | --- | --- |
| `FEISHU_APP_ID` | `--app-id` | 必填，应用 ID |
| `FEISHU_APP_SECRET` | `--app-secret` | 必填，应用密钥 |
| `FEISHU_USER_ACCESS_TOKEN` | `--user-access-token` | 可选，以用户身份访问 |
| `FEISHU_API_BASE_URL` | `--api-base-url` | 可选，覆盖默认 API 地址 |

[`.env.example`](.env.example)仅作模板。Binary（包括 `nix run`）不自动加载 `.env`，可直接使用环境变量，无需安装 SecretSpec；开发时用 SecretSpec dotenv 仅向应用子进程注入凭证，见[贡献指南](CONTRIBUTING.md#开发环境)。

Lark 链接自动使用 `https://open.larksuite.com`，飞书链接使用 `https://open.feishu.cn`。非官方 API 地址还需显式传入 `--allow-custom-api-base-url`；仅对可信服务启用。

支持 `wiki`、`docx`、`doc/docs`、`sheet/sheets`、`base/bitable`、`file`、`mindnote/mindnotes`、`slides` 路径。可解析链接不代表能转换其内容，见[文件映射](docs/PRODUCT.md#文件映射)。

## 使用 Nix（Linux）

支持 x86_64 Linux。安装 Nix 并启用 `nix-command` / `flakes` 后，无需克隆仓库、安装 devenv 或使用 `--impure`，直接运行（`--` 后为应用参数）：

```bash
nix run github:p-zany/feishufs -- --help

export FEISHU_APP_ID=cli_xxxxxxxxxxxxxxxx
export FEISHU_APP_SECRET=replace-me

nix run github:p-zany/feishufs -- inspect 'https://example.feishu.cn/wiki/wikcnxxx'
mkdir -p ./mnt
nix run github:p-zany/feishufs -- mount 'https://example.feishu.cn/wiki/wikcnxxx' ./mnt
```

挂载仍需系统提供 FUSE 设备和 `fusermount3`，安装方式见下方 [Linux](#linux)。凭证使用上面的环境变量，不自动加载 `.env`。

## 使用 binary

从本仓库 GitHub Releases 下载对应平台的压缩包与同名 `.sha256` 文件，校验后解压；无需安装 Nix、Rust 或飞书 CLI。以下以 `v0.1.0` 为例，版本号请替换为下载的版本。

挂载命令在解压目录中运行。

### Linux

提供 `x86_64-unknown-linux-musl` binary：由 devenv 静态链接构建，无需 Nix、glibc 或 `libgcc_s.so.1`。需要 x86_64 Linux、可用的 `/dev/fuse` 和 `fusermount3`。Ubuntu / Debian 安装挂载工具：

```bash
sudo apt-get update
sudo apt-get install fuse3
sha256sum --check feishufs-v0.1.0-x86_64-unknown-linux-musl.tar.gz.sha256 && \
  tar -xzf feishufs-v0.1.0-x86_64-unknown-linux-musl.tar.gz
```

配置凭证，预览目录并挂载：

```bash
chmod +x ./feishufs
export FEISHU_APP_ID=cli_xxxxxxxxxxxxxxxx
export FEISHU_APP_SECRET=replace-me

./feishufs inspect 'https://example.feishu.cn/wiki/wikcnxxx'
mkdir -p ./mnt
./feishufs mount 'https://example.feishu.cn/wiki/wikcnxxx' ./mnt
```

### Windows

**Windows 平台尚未测试**。提供 `x86_64-pc-windows-msvc` binary，由 Linux 交叉编译并检查 PE 格式，不代表已验证 Windows 上的运行或挂载行为。需要 x86_64 Windows 10 1809 / Windows Server 2019 或更新版本、ProjFS 和 NTFS 挂载目录。CRT 静态链接，无需另装 VC++ 运行库。

在下载目录的 PowerShell 中校验并解压：

```powershell
$archive = 'feishufs-v0.1.0-x86_64-pc-windows-msvc.zip'
$expected = (Get-Content "$archive.sha256").Split(' ')[0]
if ((Get-FileHash $archive -Algorithm SHA256).Hash -ne $expected) { throw 'SHA-256 mismatch' }
Expand-Archive -Path $archive -DestinationPath .\feishufs
Set-Location .\feishufs
```

在管理员 PowerShell 中启用 ProjFS；如系统提示需要重启，先重启：

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart
```

然后在 binary 所在目录打开普通 PowerShell，使用空的 NTFS 目录作为挂载点：

```powershell
$env:FEISHU_APP_ID = 'cli_xxxxxxxxxxxxxxxx'
$env:FEISHU_APP_SECRET = 'replace-me'

.\feishufs.exe inspect 'https://example.feishu.cn/wiki/wikcnxxx'
New-Item -ItemType Directory -Path .\mnt
.\feishufs.exe mount 'https://example.feishu.cn/wiki/wikcnxxx' .\mnt
```

复用挂载路径前需清理旧投影，见[平台边界](docs/PRODUCT.md#平台与非目标)。

两个平台均以前台进程运行，按 `Ctrl-C` 卸载。`inspect` 只读取目录和子表元信息，不下载正文，不能验证所有正文 / 图片的读取权限。

挂载后，将 `mnt` 交给 Agent 或其他本地工具。若已安装 ripgrep，可在另一个终端搜索：

```bash
rg -n '关键词' ./mnt
```

## 从源码构建（Linux）

需要 Rust 1.98 或更新版本。在取得源码并进入仓库根目录后：

```bash
sudo apt-get update
sudo apt-get install build-essential pkg-config libfuse3-dev fuse3
cargo build --release --locked
```

产物为 `target/release/feishufs`，按上面的 Linux 用法运行，将 `./feishufs` 替换为 `./target/release/feishufs`。

也可安装到 Cargo 的二进制目录：

```bash
cargo install --path . --locked
```

确保 `~/.cargo/bin` 在 `PATH` 中，即可直接运行 `feishufs`。Nix 用户见上方[使用 Nix](#使用-nixlinux)；本地构建、开发检查及发布流程见[贡献指南](CONTRIBUTING.md)。

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

资产入口统一为 `<mountpoint>/.feishufs-assets`（例如 `wiki/.feishufs-assets`），不再默认创建工作目录下的同名目录。Markdown 和 CSV 中的已知附件链接都改写到该入口，原附件路径仍可读取。下载成功后保存到内部磁盘缓存；缓存保留到用户手动清理，无总容量限制或跨挂载缓存恢复。默认缓存位置与布局见[资产落盘](docs/PRODUCT.md#资产落盘)。

时间参数接受 `60s`、`5m` 等格式，必须大于零。`inspect` 也接受这些参数，但不执行后台刷新。目录失败后的等待间隔按 2、4、8 倍退避，成功后恢复；内容在冷却结束后的下一次访问重试。

完整选项见 `--help`、`inspect --help` 和 `mount --help`。

## 日志与排障

默认向当前目录的 `feishufs.log` 追加日志，启动时打印绝对路径。后台错误在终端显示摘要，完整错误链写入文件；启动失败也会在终端输出错误链。

- `--log-file <PATH>` 或 `FEISHUFS_LOG_FILE` 指定路径；`--no-log-file` 关闭文件日志。
- 默认终端 `info`、文件 `debug`；`-v` 将终端升为 `debug`，`-vv` 两者均为 `trace`。`RUST_LOG` 覆盖两个出口的过滤规则。
- 仅在启动时，超过 8 MiB 的日志轮转为 `<文件名>.1`，保留一份备份；不是运行中的大小上限。文件打开失败则降级为仅终端输出。

无法定位空间时，检查链接是否属于知识库；权限不足时，同时检查应用权限与空间访问权。目录 / 正文仍旧时，查看刷新和内容获取错误，等待重试后重新打开文件。

报错时提供平台、版本、脱敏命令和相关日志；删除凭证、文档 token、私有标题及正文。

## 文档

- [产品与边界](docs/PRODUCT.md)：能力、产品取舍和限制
- [路线图](docs/ROADMAP.md)：后续方向与未决事项
- [架构说明](docs/ARCHITECTURE.md)：技术栈、模块边界和技术取舍
- [贡献指南](CONTRIBUTING.md)：开发、测试和提交要求

## License

[MIT](LICENSE)
