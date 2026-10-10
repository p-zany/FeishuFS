# FeishuFS

将飞书/Lark 知识库挂载为本地**只读文件系统**，让编辑器、搜索工具、脚本和 AI 工具通过普通文件 API 读取云文档。

- 输入知识库空间内任意一个文档链接，挂载该文档所在的**整个知识库**
- Linux：FSK + FUSE
- Windows：FSK + ProjFS
- 飞书 API：[OpenLark `0.20.0`](https://docs.rs/openlark-docs/0.20.0/openlark_docs/)
- 默认目录刷新等待间隔 60 秒，正文缓存 TTL 独立为 60 秒
- 不包含任何写入飞书的 API 调用

完整产品合同见 [`docs/PRODUCT.md`](docs/PRODUCT.md)。

## 文件映射

挂载根就是知识库空间目录。有子页面的节点映射为目录，子页面放在目录内；普通文档正文使用 `index.*`，叶子文档直接映射为同级文件。`sheet` / `bitable` 每个子表各输出一个 `标题.<子表名>.csv`：有子页面时放在 `标题/` 内，没有子页面时放在当前目录，不额外创建目录。

```text
<mountpoint>/            ← 整个知识库空间
├── 快速开始/            ← 有子页面的文档
│   ├── index.md
│   ├── API 说明.md      ← 没有子页面的文档
│   └── 常见问题/
│       └── index.md
├── 数据报表.收入.csv    ← 没有子页面的表格，各子表并列
├── 数据报表.支出.csv
└── 附件.pdf
```

| 飞书对象 | 有子页面 | 没有子页面 |
| --- | --- | --- |
| `docx` | `标题/index.md` | `标题.md` |
| 旧版 `doc` | `标题/index.docx` | `标题.docx` |
| `sheet` / `bitable` | `标题/标题.<子表名>.csv`（每个子表一份） | `标题.<子表名>.csv`（每个子表一份） |
| `file` | `标题/index.<扩展名>` | `标题.<扩展名>` |
| 暂不支持的类型 | `标题/index.md` | `标题.md`（说明原因和云端链接） |

文件名会清理跨平台非法字符、Windows 设备名和大小写不敏感的同级冲突。inode 在一次挂载期间保持稳定且不复用。

### 独立表格

- 目录遍历读取 sheet 工作表元信息和 bitable 数据表列表（完整分页），不预取 CSV；每个文件首次访问时带对应 `sub_id` 创建 CSV 导出任务。
- 不生成 `index.csv` 或 XLSX；空的可见子表列表生成 Markdown 说明，不伪造空 CSV。
- 子表改名、增删及节点移动在下一次成功目录刷新后生效；每个 CSV 独立缓存，inode 按节点与子表 ID 保持稳定。同名、非法字符及超长名称按跨平台规则处理，保留 `.csv` 扩展名。
- 带 `?sheet=<ID>` / `?table=<ID>` 的挂载内链接指向对应 CSV；未知子表保持云端链接。不指定子表时指向按子表 ID 排序的首个 CSV。
- 直接保留飞书原生 CSV 导出字节，不重写引号、换行或编码。每个工作表/数据表只导出一次，不分别导出多视图或筛选视图；不保留公式源码、格式、图表和布局，也不额外下载附件字段的二进制内容。隐藏工作表不会从清单中过滤；隐藏行列、视图筛选和附件字段的实际导出表示以服务端结果为准，尚未用真实复杂表格完整验证。

### 正文与画板

`docx` 使用官方 CLI 同样的 Docs AI Fetch 接口读取 XML，由原生 Rust 按结构转为 Markdown；不需要安装或调用 CLI。文档引用转为标准 Markdown 链接。

- 画板有非空 Mermaid 源码时，原位输出 `mermaid` 代码块，不从图片反推。
- 无 Mermaid 源码时，下载实际图片格式，原位引用挂载内图片；成功路径原型暂用 `.feishufs-assets/<正文 inode>/<编码 token>.<扩展名>`。
- 图片目录在成功读取正文后按需出现；叶子文档仍是一个 `.md` 文件，不因此改成目录。
- 本轮只落实成功路径。部分资源失败、缓存保留、重试与整体发布等策略尚未定案，见 [产品定义 §3.1.3](docs/PRODUCT.md#313-待讨论fetch-失败与资源一致性)；原有错误处理保持不变。
- 字符实体及其外层 Markdown 转义按文本上下文规范化；代码和链接目标不做全局替换。
- 未支持的特殊块保留说明及原始结构，不静默丢弃。普通图片全面本地化及同步块展开尚未实现。

### 文档内链接

飞书正文里指向挂载范围内其它文档的链接会被改写成相对的本地 Markdown 链接，例如 `[API](<../快速开始/常见问题/index.md>)`、`[说明](<API 说明.md>)`。指向范围外、非飞书域名或未知节点的链接保持原样；围栏代码块、缩进代码块和行内代码中的示例链接不改写。云端改名或移动节点后，下一次目录刷新会用缓存的原始 Markdown 重新渲染链接，无需重新下载正文。

## 前置条件

### 飞书/Lark 应用

创建自建应用，并为它启用知识库、Docx 文档读取（例如 `docx:document:readonly`）、电子表格读取（例如 `sheets:spreadsheet:readonly`）、多维表格读取（例如 `bitable:app:readonly`）、云盘文件下载和文档导出的只读权限。无 Mermaid 源码的画板图片还需要 `board:whiteboard:node:read`。应用或对应用户还必须能访问目标知识库。

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
# 链接可以是空间内任意一个文档（wiki / docx / sheet / ... 路径均可）
cargo run -- inspect 'https://example.feishu.cn/wiki/wikcnxxx'
```

输出的是该文档所在整个知识库的目录预览。链接中的文档不需要是空间首页，也不影响挂载根：

```text
Space: <空间名称>
Nodes: <可见文档数>
Filesystem preview:
  ...
```

再以前台进程挂载：

```bash
mkdir -p ./mnt
cargo run -- mount 'https://example.feishu.cn/wiki/wikcnxxx' ./mnt
```

按 `Ctrl-C` 安全卸载。

常用参数：

```text
--refresh-interval 60s        每次目录刷新完成后的等待间隔
--content-ttl 60s             独立的正文与附件缓存 TTL
--content-retry-interval 5s   内容获取失败后的重试冷却
--max-nodes 10000        单次挂载的文档数上限
--max-download-mib 100   单文件下载/导出上限
--request-timeout 30s    OpenAPI 请求超时
-v / -vv                 debug / trace 日志
```

Lark 链接会自动使用 `https://open.larksuite.com`，飞书链接使用 `https://open.feishu.cn`。只有在显式传入 `--allow-custom-api-base-url` 后才允许自定义 API 地址。

## 日志

应用自身的 CLI、日志、注释和错误标签使用英文；云端标题、正文及原始服务端消息保留原文。终端保持简短（只说明发生了什么），**完整错误链写入日志文件**——飞书错误码与响应体只在那里，排障请看文件：

```bash
# 默认写入当前目录的 feishufs.log，启动时会打印绝对路径
tail -f feishufs.log

# 指定路径
feishufs mount '链接' ./mnt --log-file /var/log/feishufs.log
FEISHUFS_LOG_FILE=/var/log/feishufs.log feishufs mount '链接' ./mnt

# 只要终端，不写文件
feishufs mount '链接' ./mnt --no-log-file

# 让终端也变详细（默认终端 info，文件 debug；-vv 两者都 trace）
feishufs mount '链接' ./mnt -vv
# 或用 RUST_LOG 接管两个出口的过滤规则
RUST_LOG=feishufs=trace feishufs inspect '链接'
```

日志文件以追加方式写，启动时超过 8 MiB 会轮转为 `feishufs.log.1`（只保留一份）。若用 sudo 运行，日志文件属主为 root。日志文件打开失败不会中断运行，只会降级为仅终端输出。

## 刷新和故障行为

- 输入链接只在启动时用于定位空间；之后固定该空间，入口文档被删除或移到别处不会阻止刷新或切换挂载空间。正文和附件按需获取。
- 一轮目录遍历完成后再等待 `--refresh-interval`；不重叠、不追赶，实际周期包含遍历耗时。完整目录没有变化时不发布新版本、不重渲染正文链接、不全量清理投影缓存。
- 打开文件或查询元数据时检查独立的 `--content-ttl`；普通 `read()` 不主动检查过期。同一文件获取去重，不同文件最多并发获取 4 份；慢下载不阻塞其他已缓存文件或目录发布。
- API 暂时失败时继续提供旧目录或旧内容。内容失败后默认冷却 5 秒，首次失败也冷却但仍返回 I/O 错误；不把旧缓存的成功时间重置成当前时间。
- 目录刷新失败后等待间隔按 2、4、8 倍退避，最多 8 倍；成功后恢复基础间隔。不增加跨进程限流或共享配额协调。
- Windows 单独按内容 TTL 检查已缓存文件的投影失效，不依赖目录是否发生变化，也不主动下载所有正文；正在使用的投影可能暂时无法删除，实际行为仍需 Windows 原生验证。
- 本轮不修改 FSK、不引入本地文件快照或按句柄固定版本。因此另一处打开、元数据查询或目录链接重渲染可能改变正在读取的共享缓存，不保证单次打开期间的严格快照一致性；该问题留待单独讨论。

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
- 有 Mermaid 源码的画板保留代码，无源码画板的图片本地化。普通内嵌图片/附件尚未全面本地化，有 URL 时保留链接，否则保留资源原文；指向挂载内 `file` 节点的标准链接会改写为本地路径。
- 简单文档表格可渲染为 Markdown 表格；合并单元格或其他未支持结构保留原始 XML，不伪造完整转换结果。
- 新内容接口与图片资源的失败恢复策略尚待讨论，不将原型行为视为已确定的产品合同。
- 大型知识库受飞书 Wiki API 速率限制影响，一次完整刷新可能超过一分钟。
- Windows 的最终原生挂载行为需在启用了 ProjFS 的 Windows 主机上验证；CI 和 `cargo-xwin` 负责条件编译检查。

## License

[MIT](LICENSE)
