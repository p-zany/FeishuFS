# 架构说明

面向协作者：说明代码在哪里、边界是什么。用户行为以[产品与边界](PRODUCT.md)为准，运行和测试见[贡献指南](../CONTRIBUTING.md)。

## 数据流

```text
CLI / 环境变量
  → KnowledgeLink 定位空间
  → RemoteSource 读取节点与子表清单
  → RemoteTree → FsState 构建目录快照
  → FeishuFs → FSK → Linux FUSE

export / sync → FeishuFs → 遍历与读取 → 普通本地文件

文件访问 → 按需获取内容 → XML / 导出 / 下载 → 资产原子落盘 → 缓存与本地链接渲染
后台刷新 → 完整目录快照 → 比较并发布
```

## 技术栈与取舍

| 技术 | 用途与取舍 |
| --- | --- |
| Rust 2024，最低版本见 [`Cargo.toml`](../Cargo.toml) | 原生文件系统实现；开发 shell 与发布共用 `rust-toolchain.toml` |
| FSK `0.0.9` | 共用文件系统接口，接入 Linux FUSE；平台差异仍在本项目处理 |
| OpenLark `0.20.0` | 复用鉴权和 Transport；精确固定 SDK 版本，缺失字段 / 接口局部补齐，不引入 CLI 依赖 |
| Tokio | 承载异步 SDK 请求；在同步 `RemoteSource` 边界桥接，不把异步运行时扩散到文件系统接口 |
| roxmltree + Markdown 渲染 | 按 XML 节点顺序转换，不依赖文本锚点或模糊对齐；不能转换的结构保留原文 |
| clap / tracing | 参数与环境变量解析；终端摘要和文件诊断分离 |

具体依赖以 [`Cargo.toml`](../Cargo.toml) 为准。Docx 使用 Docs AI Fetch 读取 XML；导出任务虽使用 POST，但不修改源文档。项目没有云端文档写 API。

[`devenv.nix`](../devenv.nix) 唯一定义开发环境、常规检查与 binary 编译；inputs 以 `flake.nix` / `flake.lock` 为单源，`devenv.yaml` 通过 `self: path:.` 加 `follows: self/<input>` 继承，`devenv.lock` 由 `devenv update` 从 `flake.lock` 派生。[`flake.nix`](../flake.nix) 通过 devenv 原生 `lib.mkConfig` 求值同一模块，仅暴露 Linux package / app，不构建开发 shell 或执行 hooks。两者共用 `rust-toolchain.toml` 与同一组锁定 inputs。Nix 输出 binary 及依赖 / 运行时许可声明，CI 通过 devenv 执行 hooks、冒烟和编译，在 workflow 的 `run:` 中打包与生成校验文件，再由 `gh-release` 上传。只支持 x86_64 Linux 构建主机与运行平台。命令及检查范围见[贡献指南](../CONTRIBUTING.md)。

## 代码地图

| 模块 | 职责 |
| --- | --- |
| `main.rs` | CLI、连接、挂载与退出 |
| `export.rs` | CLI 的一次性文件复制；复用 FSK 读取接口，正文后再复制资产，单文件原子替换 |
| `link.rs` | 官方链接解析及 API 域名选择 |
| `remote.rs` | `RemoteSource` 边界、远端模型、空间遍历与子表清单校验 |
| `openlark.rs` / `sdk_error.rs` | SDK 适配、XML 获取、文件 / 画板下载、CSV 导出及错误链 |
| `docx.rs` / `markdown.rs` / `csv.rs` | XML 转 Markdown、共用本地链接解析、CSV 单元格保格式改写及资产引用 |
| `tree.rs` | `FsState`、目录映射、文件名消歧、inode 与链接索引 |
| `assets.rs` | 挂载外的内部磁盘缓存、按域名 / 空间 / token 隔离及原子落盘；不作为文档链接目标 |
| `filesystem.rs` / `refresh.rs` | FSK 接口、内容缓存、获取去重、目录发布、并发槽与退避 |
| `logging.rs` | 日志出口、过滤与启动轮转 |

`RemoteSource` 隔离真实 API 与文件系统，也是离线测试注入假数据的边界；不为每个模块再建接口。

## 关键不变量

- 启动后固定 `space_id`；刷新不重新依赖入口文档。
- 目录及子表清单完整后才发布；清单失败或 ID 非法 / 重复时保留旧快照。相同快照不增加修订或全量失效投影。
- inode 在单次挂载内不复用；正文身份不因叶子 / 分支转换改变，CSV 身份由节点与子表 ID 决定。
- 网络 I/O 不持有全局发布锁；发布前复核内容身份，丢弃已删除或已变更节点的旧响应。
- 同一内容身份的获取去重，附件原入口和资产入口映射到同一缓存 inode；不同内容默认最多并发 4 份。这是并发上限，不是 API 限流或跨进程配额协调。
- 资产磁盘 I/O 不持有全局发布锁；完整临时文件写入后原子替换旧副本，不向只读挂载自身写入。磁盘保留与跨挂载边界见[资产落盘](PRODUCT.md#资产落盘)。
- Markdown 与 CSV 缓存均保留原始内容与渲染结果，目录变化后从原文重算相对链接，无需重新下载或导出。CSV 只替换已知 URL 单元格，不重排其他字段或归一化格式。
- 退出时先停止刷新，再销毁挂载会话。

正文与资产的失败恢复不是已冻结的事务合同；不要从 `ContentBundle` 的成功路径推导额外保证。未决项见[路线图](ROADMAP.md#尚未定案)。
