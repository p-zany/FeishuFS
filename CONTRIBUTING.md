# 贡献指南

先读[架构说明](docs/ARCHITECTURE.md)。修改用户行为前，对照[产品与边界](docs/PRODUCT.md)和[路线图](docs/ROADMAP.md)。

## 开发环境

推荐在 x86_64 Linux 使用独立的 [devenv CLI](https://devenv.sh/getting-started/)：

```bash
devenv shell
cargo build --locked
```

开发环境与 binary 编译由 [`devenv.nix`](devenv.nix) 管理。inputs 以 [`flake.nix`](flake.nix) / [`flake.lock`](flake.lock) 为单源：[`devenv.yaml`](devenv.yaml) 通过 `self: path:.` 与 `follows: self/<input>` 从 flake 继承 nixpkgs、devenv 模块、rust-overlay 和 git-hooks，[`devenv.lock`](devenv.lock) 只是 `devenv update` 从 `flake.lock` 派生的锁，版本不再两处维护。工具链与两个交叉目标统一定义在 [`rust-toolchain.toml`](rust-toolchain.toml)。[`flake.nix`](flake.nix) 通过 devenv 原生 `lib.mkConfig` 求值同一模块，只暴露 Linux 应用的 package / app，不管理开发 shell，也不执行 hooks；Nix 用户无需安装 devenv CLI 或使用 `--impure`。升级时先 `nix flake update`（可用 `nix flake update <input>` 只升级指定 input），再运行 `devenv update` 重新派生 devenv.lock 并检查；`flake.nix` 与 `flake.lock` 必须提交，devenv CLI 和远端 Nix 用户都依赖它们。

更新 `rust-toolchain.toml` 后，退出旧的 `devenv shell` 再重新进入，并用 `rustc --version` / `cargo --version` 确认版本；已打开的 shell 不会自动切换工具链。

Shell 包含 devenv 自带的 [SecretSpec CLI](https://devenv.sh/integrations/secretspec/)，但进入 shell、常规检查和构建均不加载凭证。[`secretspec.toml`](secretspec.toml) 仅声明所需变量并将其路由到 dotenv；在仓库根目录将公开模板复制到被忽略的 `.env`，自行填入本地凭证：

```bash
cp .env.example .env
# 编辑 .env，填写 FEISHU_APP_ID 和 FEISHU_APP_SECRET
cargo build --locked
secretspec run --provider dotenv -- ./target/debug/feishufs inspect '<link>'
```

`FEISHU_USER_ACCESS_TOKEN` 和 `FEISHU_API_BASE_URL` 可不填；不设置 API 地址时保留飞书 / Lark 链接的自动选择。SecretSpec 将 `.env` 解析为 dotenv 数据，而不是执行 Bash；仅在启动所需子进程时注入声明的变量，不改变父 shell。不要用凭证包裹 `devenv` 或构建命令，也不要将凭证映射到 Nix `env` / store。

系统挂载条件见 README 的 [Linux](README.md#linux) / [Windows](README.md#windows) 用法；不用 Nix 时，参照 [Linux 源码构建](README.md#从源码构建linux)准备工具链。常规测试无需凭证或 FUSE 设备。

## 常规检查

```bash
devenv test
```

与 CI 一致：由 devenv 执行 hooks、Nix 格式、workflow 语法、Rust fmt / clippy 和全部常规 Rust 测试，不带 `--ignored`。也可用 `devenv tasks run …` 分别运行检查。两平台 binary 编译使用下述 `devenv build`；Windows 构建在同一封闭 SDK 下编译检查全部 targets / features，包括 Windows 专用单元测试和集成测试，**不运行 Windows 测试**。

也可在已配置的 Rust 环境中分别运行：

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

### 平台与真实 API 测试

| 测试 | 入口 | 边界 |
| --- | --- | --- |
| Linux FUSE 冒烟 | `devenv tasks run feishufs:smoke-linux` | 需要 `/dev/fuse` 和挂载权限；使用假数据，不访问飞书 |
| Windows 编译覆盖 | `devenv build outputs.feishufsWindows` | 同一封闭 SDK 下 cargo-xwin `--all-targets --all-features` 编译检查后构建 binary；不执行 Windows 测试或验证原生行为 |
| Windows 原生 | Windows 上运行常规测试并手动挂载 | 修改通知、投影刷新和占用文件仍需实际验证 |
| 真实内容读取 | `cargo test --test docs_ai_live -- --ignored --nocapture` | 明确授权后运行，所需变量和样例断言见 `tests/docs_ai_live.rs` |

环境缺少 setuid fusermount 但允许 sudo 时，可用 `FEISHUFS_FUSE_SUDO=1 devenv tasks run feishufs:smoke-linux`。

真实挂载和真实 API 测试均为 `ignored`，不会被常规测试触发。统一 [CI / release workflow](.github/workflows/ci.yml) 在 push、PR 和 tag 上执行同一组 Linux 检查与两平台交叉构建；没有原生 Windows job。

## 发布 binary

编译唯一定义在 [devenv.nix](devenv.nix) 中，`outputs` 只输出 binary，不生成压缩包或校验文件；[flake.nix](flake.nix) 通过 `lib.mkConfig` 复用其中的 Linux 输出，默认不构建 Windows SDK。CI 使用独立 devenv CLI 执行检查与编译，直接在 workflow 的 `run:` 中打包，再交给 `softprops/action-gh-release` 发布，不另设打包脚本或 Nix 模块。只支持 x86_64 Linux 构建主机，Windows 为交叉目标。应用 derivation 不依赖本地 `.env` 或工作区状态。固定 `rust-toolchain.toml` 指定的工具链与 `Cargo.lock`，应用构建仅使用清单、锁文件和 `src/` Rust 源码；Windows 的全部目标编译检查额外加入公开的 `tests/**/*.rs`（含 `tests/support`），显式排除整个 `tests/fixtures/` 子树，不包含私有样例或工作区内容。MSVC SDK / CRT 使用锁定 nixpkgs 的 `windows.sdk`，其 manifest 和下载 SHA-256 由 nixpkgs 固定，实际 Windows 编译离线完成；首次构建需下载工具链、锁定依赖和 SDK。

Microsoft SDK / CRT 是非自由的构建依赖，适用 [Microsoft Software License Terms](https://visualstudio.microsoft.com/license-terms/mt644918/)，不属于本项目的 MIT 授权。发布配置仅在局部 nixpkgs 包集中接受该许可，并放行 `win-sdk` 与其下载器 `xwin-fetch-msvc`，不全局允许其他非自由软件。运行 Windows 构建前请阅读并确认同意这些条款；binary 输出不含 SDK。

本地编译两平台 binary：

```bash
devenv build outputs.feishufsLinux outputs.feishufsWindows
# stdout JSON 分别给出两个 store 路径，产物为 bin/feishufs 和 bin/feishufs.exe；也可单独构建：
devenv build outputs.feishufsLinux
devenv build outputs.feishufsWindows
# flake package / app 复用 Linux 静态 binary：
nix build
nix run .# -- --help
```

Linux 使用静态 musl；Windows 在同一 SDK / CRT cache 中先运行 `cargo xwin check --frozen --all-targets --all-features`，再构建 MSVC PE，不在 Linux 执行 Windows binary 或测试。CI 验证 Linux 无 ELF interpreter / 动态依赖、版本及三个 CLI help，并检查 Windows 架构和 CRT 导入。CI 的 `run:` 将 Linux binary 制作为 `.tar.gz`、Windows binary 制作为 `.zip`，各附同名 `.sha256` 文件；压缩包内只有对应 binary、`LICENSE` 和 `.env.example`，并验证精确成员与 SHA-256。校验文件引用压缩包 basename，可在下载目录直接校验。

统一 workflow 仅在推送 `v<版本>` tag 且与该提交的 `Cargo.toml` package version 完全一致时发布；PR（包括 forks）、分支及其他 tag 不进入发布 job。共同的 devenv 检查、两个 binary 编译、验证及打包全部成功后，压缩包与校验文件以 artifact 传到独立发布 job，再由 `softprops/action-gh-release` 上传；构建 job 只有只读权限，发布 job 只有下载与 API 操作，不 checkout、构建或执行仓库代码。**Windows 平台尚未测试**，交叉编译不验证原生测试、CLI 运行或挂载行为。

确认发布提交已推送、版本正确且 `origin` 已配置为发布仓库后，创建并推送对应 tag（以下以 `0.1.0` 为例）：

```bash
git tag -a v0.1.0 -m 'Release v0.1.0'
git push origin v0.1.0
```

版本含 `-rc.1` 等 prerelease 后缀时，发布为预发布且不标为 Latest。Workflow 在发布前确认远端 tag 仍指向触发提交、release 尚不存在，不移动 tag、不覆盖已有 release 或同名附件；失败后先检查 Actions 日志与 Releases 状态，没有 release 时可重跑，已有 release 时停止并人工检查，不移动已发布 tag。运行依赖和下载用法见 [README](README.md#使用-binary)。

## 提交要求

- 修复附复现步骤和回归测试；转换逻辑用小型、脱敏的输入样例。
- 未定案的行为先讨论，不用实现或测试替产品决策；用户行为变更同步更新对应文档。
- 应用自身的 CLI、日志、注释和错误标签用英文；云端文本与 Unicode 测试样例保留原文。
- 不提交凭证、私有云端内容、日志或本机配置。

Git hooks 依赖开发环境中的 `cc`、`cargo` 等工具。提交时使用 `devenv shell -- git commit`；环境外出现 `linker cc not found` 时，在 shell 内重试，不跳过检查。

## 文档维护

- README 负责产品介绍与用法，其他文档分工见 [文档索引](README.md#文档)。一个事实只保留一个主位置，其他地方用链接引用。
- 写当前能力、限制和理由，不保留“本轮”“本次已授权”等过程叙述。路线图区分候选、待验证和未决策略。
- 构建、测试、关键不变量和长期技术决策随代码公开；易变的函数细节优先放代码注释与测试。
- 临时实现笔记、调试记录和私有样例放 `docs/local/`，该目录已被 Git 忽略。忽略规则不保护已跟踪或强制添加的文件；需要共享的私密记录应使用独立私有存储。

组织方式参考 GitHub 的 [README](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-readmes) / [CONTRIBUTING](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/setting-guidelines-for-repository-contributors) 惯例、[Diátaxis 职责拆分](https://diataxis.fr/)和[精简架构说明](https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html)。
