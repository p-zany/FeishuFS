# FeishuFS

[简体中文](README.zh-CN.md) | **English**

Mount a Feishu / Lark knowledge base as a local, read-only filesystem so AI agents can access documents with `ls`, `rg`, and their existing file tools.

- **Documents become files**: Docx documents become Markdown, each spreadsheet or Base table becomes a CSV file, legacy documents export as DOCX, and attachments keep their original contents.
- **Follow document links**: Links to documents within the mount become relative local paths, making related content easy to navigate.
- **Fetch on demand and share a cache**: Tools and agents use the same mount, with no Feishu-specific skill or particular model required.

Currently supports **x86_64 Linux**. A document link selects its **entire visible knowledge space**, not just that document. Access is read-only; local changes are never synced back to Feishu.

## Quick start

### 1. Prepare the environment and access

- Install Nix with `nix-command` / `flakes` enabled. For other installation methods, see [building from source](docs/USAGE.en.md#build-from-source) or [release packages](docs/USAGE.en.md#use-a-release-package).
- The system needs a usable `/dev/fuse`, permission to mount, and `fusermount3`. On Ubuntu / Debian, run `sudo apt-get install fuse3`.
- Follow the [authorization guide](docs/AUTHORIZATION.en.md) to create a custom app, enable API permissions, grant access to the knowledge base, and obtain an App ID and App Secret.

### 2. Preview and mount

Replace the example credentials and URL with your own. Supply credentials through environment variables and keep them out of Git. The program does not load `.env` automatically.

```bash
export FEISHU_APP_ID=cli_xxxxxxxxxxxxxxxx
export FEISHU_APP_SECRET=replace-me

nix run github:p-zany/feishufs -- inspect 'https://example.feishu.cn/wiki/wikcnxxx'
mkdir -p ./mnt
nix run github:p-zany/feishufs -- mount 'https://example.feishu.cn/wiki/wikcnxxx' ./mnt
```

The mount runs in the foreground; press `Ctrl-C` to unmount. Lark URLs automatically select the Lark API. `inspect` previews directory and subtable metadata only; it does not verify access to every document body or image.

### 3. Use your agent or local tools

Point your agent at `mnt` to browse and read documents. You can also search from another terminal with ripgrep installed:

```bash
ls ./mnt
rg -n 'keyword' ./mnt
```

Content is fetched on first access, so a full-text search may download many documents. Cached files remain until manually removed; there is currently no total cache size limit.

## Documentation

- [Authorization](docs/AUTHORIZATION.en.md): App setup, API permissions, knowledge base access, and credentials
- [Usage guide](docs/USAGE.en.md): Installation methods, options, caching, and troubleshooting; run `--help` for all CLI options
- [Product and limitations](docs/PRODUCT.md) (Chinese): File mapping, content conversion, and refresh behavior
- [Contributing](CONTRIBUTING.md) · [Architecture](docs/ARCHITECTURE.md) · [Roadmap](docs/ROADMAP.md) (Chinese)

## License

[MIT](LICENSE). Release packages also include notices for third-party dependencies, the Rust standard library, and musl. Keep these notices when redistributing a package.
