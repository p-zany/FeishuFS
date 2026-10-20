# Usage guide

[简体中文](USAGE.md) | **English** · [Back to README](../README.md)

## Linux mount requirements

FeishuFS supports x86_64 Linux and requires a usable `/dev/fuse`, permission to mount, and `fusermount3`. On Ubuntu / Debian:

```bash
sudo apt-get update
sudo apt-get install fuse3
```

See [Authorization](AUTHORIZATION.en.md) for app credentials and knowledge base access, and the [README quick start](../README.md#quick-start) for running with Nix.

## Use a release package

When [GitHub Releases](https://github.com/p-zany/feishufs/releases) provides a Linux archive for the version you want, download it together with its `.sha256` file, verify it, and extract it. If no release package is available yet, use Nix or build from source below.

Release binaries are statically linked with musl and do not require Nix, Rust, the Feishu CLI, glibc, or `libgcc_s.so.1`. The FUSE requirements above still apply. This example uses `v0.1.0`; use the version matching your downloaded files:

```bash
sha256sum --check feishufs-v0.1.0-x86_64-unknown-linux-musl.tar.gz.sha256 && \
  tar -xzf feishufs-v0.1.0-x86_64-unknown-linux-musl.tar.gz
```

From the extracted directory, configure credentials as shown in the [README quick start](../README.md#quick-start), then replace `nix run github:p-zany/feishufs --` with `./feishufs`:

```bash
./feishufs inspect 'https://example.feishu.cn/wiki/wikcnxxx'
mkdir -p ./mnt
./feishufs mount 'https://example.feishu.cn/wiki/wikcnxxx' ./mnt
```

The mount runs in the foreground; press `Ctrl-C` to unmount. `inspect` only reads directory and subtable metadata. It does not download document bodies or verify every document/image permission.

## Build from source

Requires Rust 1.98 or later. After obtaining the source and entering the repository root:

```bash
sudo apt-get update
sudo apt-get install build-essential pkg-config libfuse3-dev fuse3
cargo build --release --locked
```

The binary is at `target/release/feishufs`. Configure credentials as shown in the [README quick start](../README.md#quick-start), then replace `nix run github:p-zany/feishufs --` with `./target/release/feishufs`.

You can also install into Cargo's binary directory:

```bash
cargo install --path . --locked
```

Ensure `~/.cargo/bin` is in `PATH` to run `feishufs` directly. For Nix, see the [README quick start](../README.md#quick-start). For development checks and the release process, see [Contributing](../CONTRIBUTING.md) (Chinese).

## Common options

| Option | Default | Meaning |
| --- | --- | --- |
| `--refresh-interval` | `60s` | Delay after each directory refresh completes |
| `--content-ttl` | `60s` | Independent cache TTL for document bodies and attachments |
| `--content-retry-interval` | `5s` | Retry cooldown after a content fetch fails, including the first fetch |
| `--max-nodes` | `10000` | Maximum knowledge base nodes, excluding subtables |
| `--max-download-mib` | `100` | Download/export limit per file in MiB, not a total cache limit |
| `--request-timeout` | `30s` | Timeout for each OpenAPI request, not the entire export task |
| `mount --cache-dir` | `feishufs` under the user's cache directory | Internal disk cache, separate from the asset entry point; neither this directory nor the mountpoint may contain the other |

Assets are exposed under `<mountpoint>/.feishufs-assets`, for example `wiki/.feishufs-assets`; a separate directory of that name is not created in the working directory by default. Known attachment links in Markdown and CSV point to this entry point, while original attachment paths remain readable. Successful downloads are saved to the internal disk cache. Files remain until manually removed; there is no total capacity limit or cache restoration across mounts. See [asset storage](PRODUCT.md#资产落盘) (Chinese) for the default location and layout.

Durations accept values such as `60s` or `5m` and must be greater than zero. `inspect` accepts these options but does not refresh in the background. Failed directory refreshes back off to 2, 4, and 8 times the normal interval, returning to normal after success. Failed content fetches are retried on the next access after the cooldown expires.

For all options, run `--help`, `inspect --help`, or `mount --help`.

## Logging and troubleshooting

By default, logs are appended to `feishufs.log` in the current directory; its absolute path is printed at startup. Background errors appear as summaries in the terminal, with full error chains in the file. Startup failures also print their error chains in the terminal.

- Set the path with `--log-file <PATH>` or `FEISHUFS_LOG_FILE`; use `--no-log-file` to disable file logging.
- Default levels are `info` in the terminal and `debug` in the file. `-v` raises the terminal level to `debug`; `-vv` sets both to `trace`. `RUST_LOG` overrides filtering for both outputs.
- At startup only, a log larger than 8 MiB is rotated to `<filename>.1`, retaining one backup. This is not a runtime size cap. If the file cannot be opened, logging falls back to the terminal.

If the space cannot be resolved, check that the URL belongs to a knowledge base. For permission errors, check both API permissions and resource access. If the directory or content is stale, check refresh/fetch errors, wait for the retry interval, and reopen the file.

When reporting a problem, include the platform, version, sanitized command, and relevant logs. Remove credentials, document tokens, private titles, and document contents.
