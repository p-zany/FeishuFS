# FeishuFS

将飞书/Lark 知识库节点挂载为本地**只读文件系统**，让编辑器、搜索工具、脚本和 AI 工具通过普通文件 API 读取云文档。

- Linux：FSK + FUSE
- Windows：FSK + ProjFS
- 飞书 API：[OpenLark `0.20.0`](https://docs.rs/openlark-docs/0.20.0/openlark_docs/)
- 默认每 60 秒刷新目录与内容缓存
- 不包含任何写入飞书的 API 调用

完整产品合同见 [`docs/PRODUCT.md`](docs/PRODUCT.md)。

## 文件映射

输入链接指向的节点直接成为挂载根。每个知识库节点映射为目录，节点内容位于该目录的 `index.*`：

```text
<mountpoint>/
├── index.md
├── 快速开始/
│   ├── index.md
│   └── API 说明/
│       └── index.md
├── 数据报表/
│   └── index.xlsx
└── 附件/
    └── index.pdf
```

| 飞书对象 | 本地内容 |
| --- | --- |
| `docx` | `index.md` |
| 旧版 `doc` | 导出的 `index.docx` |
| `sheet` / `bitable` | 导出的 `index.xlsx` |
| `file` | 原始下载内容 `index.<扩展名>` |
| 暂不支持的类型 | 说明原因和云端链接的 `index.md` |

文件名会清理跨平台非法字符、Windows 设备名和大小写不敏感的同级冲突。inode 在一次挂载期间保持稳定且不复用。

### 文档内链接

飞书正文里指向挂载子树内节点的链接会被改写成相对的本地 Markdown 链接，例如 `[API](<快速开始/API 说明/index.md>)`。指向子树外、非飞书域名或未知节点的链接保持原样；围栏代码块、缩进代码块和行内代码中的示例链接不改写。云端改名或移动节点后，下一次目录刷新会用缓存的原始 Markdown 重新渲染链接，无需重新下载正文。

## 前置条件

### 飞书/Lark 应用

创建自建应用，并为它启用知识库、云文档内容、云盘文件下载和文档导出的只读权限。应用或对应用户还必须能访问目标知识库。

认证参数：

```bash
export FEISHU_APP_ID=cli_xxxxxxxxxxxxxxxx
export FEISHU_APP_SECRET=xxxxxxxxxxxxxxxx
# 可选：需要以用户身份访问时设置
export FEISHU_USER_ACCESS_TOKEN=u-xxxxxxxx
```

不要提交真实凭证；`.env` 已被 Git 忽略。可参考 [`.env.example`](.env.example)。

### Linux

需要可用的 `/dev/fuse`。项目的 devenv 会提供 Rust 1.88、FUSE 3、格式化/检查工具及 Git hooks：

```bash
devenv shell
```

### Windows

1. 在管理员 PowerShell 启用 ProjFS：

   ```powershell
   Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart
   ```

2. 安装 Rust 1.88（MSVC 工具链）和 Visual Studio C++ Build Tools。
3. 使用空的 NTFS 目录作为挂载点。

ProjFS 是投影缓存而非 FUSE 的完全等价物。FeishuFS 会拒绝覆盖、删除、重命名、硬链接和转为完整文件等修改通知，并且永远不会把本地变化同步到飞书。

## 使用

先检查权限和映射：

```bash
cargo run -- inspect 'https://example.feishu.cn/wiki/wikcnxxx'
```

再以前台进程挂载：

```bash
mkdir -p ./mnt
cargo run -- mount 'https://example.feishu.cn/wiki/wikcnxxx' ./mnt
```

按 `Ctrl-C` 安全卸载。

常用参数：

```text
--refresh-interval 60s   目录与内容缓存周期
--max-nodes 10000        单次挂载节点上限
--max-download-mib 100   单文件下载/导出上限
--request-timeout 30s    OpenAPI 请求超时
-v / -vv                 debug / trace 日志
```

Lark 链接会自动使用 `https://open.larksuite.com`，飞书链接使用 `https://open.feishu.cn`。只有在显式传入 `--allow-custom-api-base-url` 后才允许自定义 API 地址。

## 刷新和故障行为

- 启动时读取所选节点的完整可见子树，正文和附件按需获取。
- 后台按刷新周期重建目录快照；新增、删除、移动和重命名随后可见。
- 文件每次重新打开时检查内容 TTL；同一打开周期内保持内容一致。
- API 暂时失败时继续提供最后一次成功的目录快照或内容缓存。
- 首次获取失败时返回 I/O 错误，而不是生成空文件。

## 开发

```bash
# 全部常规检查
devenv test

# 或分别运行
devenv tasks run feishufs:fmt
devenv tasks run feishufs:clippy
devenv tasks run feishufs:test

# 在 Linux 上检查 Windows MSVC 条件编译；首次运行会下载 Windows SDK/CRT
devenv tasks run feishufs:check-windows

# 真实 Linux FUSE 冒烟测试（需要 /dev/fuse 和可挂载权限）
devenv tasks run feishufs:smoke-linux

# 如果环境没有 setuid fusermount、但允许非交互 sudo
FEISHUFS_FUSE_SUDO=1 devenv tasks run feishufs:smoke-linux
```

普通测试不会要求 FUSE 设备；真实挂载测试被标记为 `ignored`，只通过上面的任务显式运行。

Git 提交钩子由 devenv 安装，需要 devenv 提供的 `cc`、`cargo` 等工具。请在 devenv shell 中提交（已提供 `.envrc`，启用 direnv 后普通终端也会自动进入环境）：

```bash
# 推荐
devenv shell -- git commit

# 或启用 direnv 后直接提交
direnv allow .
git commit
```

若在环境外提交，钩子会报 `linker \`cc\` not found`；此时用上面的命令重试即可，代码本身的检查结果不受影响。

## 当前边界

- 首个版本不支持本地修改同步、离线首次挂载、GUI、多知识库合并或 macOS。
- 飞书文档内嵌图片由 Markdown 接口以远程 URL 返回，暂不缓存或改写为本地文件；指向子树内 `file` 节点的链接会改写为本地路径。
- 大型知识库受飞书 Wiki API 速率限制影响，一次完整刷新可能超过一分钟。
- Windows 的最终原生挂载行为需在启用了 ProjFS 的 Windows 主机上验证；CI 和 `cargo-xwin` 负责条件编译检查。

## License

[MIT](LICENSE)
