# FeishuFS

**简体中文** | [English](README.md)

将飞书 / Lark 知识库挂载为本地只读文件系统，让 AI Agent 用 `ls`、`rg` 和文件读取工具访问文档。

- **文档变文件**：Docx 转 Markdown，表格按子表输出 CSV，旧版文档导出 DOCX，附件保留原始内容。
- **沿链接阅读**：挂载范围内的文档链接改写为相对本地路径，方便继续查阅相关内容。
- **按需获取、共用缓存**：工具和 Agent 共用同一挂载，无需飞书专用 skill，也不绑定特定模型。

当前支持 **x86_64 Linux**。输入知识库内任意文档链接，会挂载该文档所在的**整个可见空间**；只读访问，不将本地修改同步回飞书。

## 快速开始

### 1. 准备环境与授权

- 安装 Nix，并启用 `nix-command` / `flakes`。其他安装方式见[源码构建](docs/USAGE.md#从源码构建)和[发布包](docs/USAGE.md#使用发布包)。
- 系统需要可用的 `/dev/fuse`、挂载权限和 `fusermount3`。Ubuntu / Debian 可运行 `sudo apt-get install fuse3`。
- 按[应用授权指南](docs/AUTHORIZATION.md)创建自建应用、申请接口权限，并授予知识库访问权，取得 App ID 和 App Secret。

### 2. 预览并挂载

将示例凭证和链接替换为自己的值。推荐用环境变量提供凭证，不要提交到 Git；程序不自动加载 `.env`。

```bash
export FEISHU_APP_ID=cli_xxxxxxxxxxxxxxxx
export FEISHU_APP_SECRET=replace-me

nix run github:p-zany/feishufs -- inspect 'https://example.feishu.cn/wiki/wikcnxxx'
mkdir -p ./mnt
nix run github:p-zany/feishufs -- mount 'https://example.feishu.cn/wiki/wikcnxxx' ./mnt
```

挂载以前台进程运行，按 `Ctrl-C` 卸载。Lark 链接自动使用 Lark API。`inspect` 只预览目录和子表元信息，不验证所有正文 / 图片的读取权限。

### 3. 交给 Agent 或本地工具

让 Agent 从 `mnt` 浏览和读取文档。也可在另一个终端用已安装的 ripgrep 搜索：

```bash
ls ./mnt
rg -n '关键词' ./mnt
```

首次读取会按需获取内容，全文搜索可能触发大量文档下载。缓存保留到手动清理，目前没有总容量限制。

## 文档

- [应用授权](docs/AUTHORIZATION.md)：应用创建、接口权限、知识库访问权与凭证
- [使用指南](docs/USAGE.md)：安装方式、参数、缓存与排障；完整命令选项见 `--help`
- [产品与边界](docs/PRODUCT.md)：文件映射、内容转换和刷新限制
- [贡献指南](CONTRIBUTING.md) · [架构说明](docs/ARCHITECTURE.md) · [路线图](docs/ROADMAP.md)

## License

[MIT](LICENSE)。发布包同时附带第三方依赖、Rust 标准库和 musl 的许可声明，重新分发时应一并保留。
