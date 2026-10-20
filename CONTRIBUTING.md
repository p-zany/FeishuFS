# 贡献指南

先读[架构说明](docs/ARCHITECTURE.md)。修改用户行为前，对照[产品与边界](docs/PRODUCT.md)和[路线图](docs/ROADMAP.md)。

## 开发环境

推荐在 x86_64 Linux 使用独立的 [devenv CLI](https://devenv.sh/getting-started/) 2.4.0（与 CI 一致；任务配置要求至少 2.3.2）：

```bash
devenv shell
cargo build --locked
```

开发环境与 binary 编译由 [`devenv.nix`](devenv.nix) 管理。inputs 以 [`flake.nix`](flake.nix) / [`flake.lock`](flake.lock) 为单源：[`devenv.yaml`](devenv.yaml) 通过 `self: path:.` 与 `follows: self/<input>` 从 flake 继承 nixpkgs、devenv 模块、rust-overlay 和 git-hooks，[`devenv.lock`](devenv.lock) 只是 `devenv update` 从 `flake.lock` 派生的锁，版本不再两处维护。工具链与 musl 目标统一定义在 [`rust-toolchain.toml`](rust-toolchain.toml)。[`flake.nix`](flake.nix) 通过 devenv 原生 `lib.mkConfig` 求值同一模块，只暴露 Linux 应用的 package / app，不管理开发 shell，也不执行 hooks；Nix 用户无需安装 devenv CLI 或使用 `--impure`。升级时先 `nix flake update`（可用 `nix flake update <input>` 只升级指定 input），再运行 `devenv update` 重新派生 devenv.lock 并检查；`flake.nix` 与 `flake.lock` 必须提交，devenv CLI 和远端 Nix 用户都依赖它们。

更新 `rust-toolchain.toml` 后，退出旧的 `devenv shell` 再重新进入，并用 `rustc --version` / `cargo --version` 确认版本；已打开的 shell 不会自动切换工具链。

Shell 包含 devenv 自带的 [SecretSpec CLI](https://devenv.sh/integrations/secretspec/)，但进入 shell、常规检查和构建均不加载凭证。[`secretspec.toml`](secretspec.toml) 仅声明所需变量并将其路由到 dotenv；在仓库根目录将公开模板复制到被忽略的 `.env`，自行填入本地凭证：

```bash
cp .env.example .env
# 编辑 .env，填写 FEISHU_APP_ID 和 FEISHU_APP_SECRET
cargo build --locked
secretspec run --provider dotenv -- ./target/debug/feishufs inspect '<link>'
```

`FEISHU_USER_ACCESS_TOKEN` 和 `FEISHU_API_BASE_URL` 可不填；不设置 API 地址时保留飞书 / Lark 链接的自动选择。SecretSpec 将 `.env` 解析为 dotenv 数据，而不是执行 Bash；仅在启动所需子进程时注入声明的变量，不改变父 shell。不要用凭证包裹 `devenv` 或构建命令，也不要将凭证映射到 Nix `env` / store。

系统挂载条件见 README 的 [Linux](README.md#linux) 用法；不用 Nix 时，参照 [Linux 源码构建](README.md#从源码构建linux)准备工具链。常规测试无需凭证或 FUSE 设备。

## 常规检查

```bash
devenv test
```

本地与 CI 均运行 `devenv test`：devenv 原生的 `devenv:git-hooks:run` 任务执行全文件 hooks（rustfmt / clippy / nixfmt / actionlint / cargo test），并运行需要真实挂载的 FUSE 冒烟测试。检查命令只在 `devenv.nix` 的 hooks 中维护，workflow 不另抄一套。无 FUSE 设备时可用 `devenv tasks run devenv:git-hooks:run` 单独运行常规检查；挂载测试可单独运行 `devenv tasks run feishufs:smoke-linux`。binary 编译使用下述 `devenv build`。

也可在已配置的 Rust 环境中分别运行：

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

### 平台与真实 API 测试

| 测试 | 入口 | 边界 |
| --- | --- | --- |
| Linux FUSE 冒烟 | `devenv test` 或 `devenv tasks run feishufs:smoke-linux` | 需要 `/dev/fuse` 和挂载权限；使用假数据，不访问飞书 |
| 真实内容读取 | `cargo test --test docs_ai_live -- --ignored --nocapture` | 明确授权后运行，所需变量和样例断言见 `tests/docs_ai_live.rs` |

真实 API 和 FUSE 测试均标记为 `ignored`，不会被普通 `cargo test` 触发。`devenv test` 只显式运行其中的 FUSE 冒烟，不调用真实 API。统一 [CI / release workflow](.github/workflows/ci.yml) 在 push、PR 和 tag 上执行 `devenv test` 与 binary 构建打包，因此提交者是否安装本地 hooks 不影响 CI 的检查范围。

## 发布 binary

编译唯一定义在 [devenv.nix](devenv.nix) 中，`outputs` 输出 binary 及随附的许可声明，不生成压缩包或校验文件；[flake.nix](flake.nix) 通过 `lib.mkConfig` 复用其中的 Linux 输出。CI 使用独立 devenv CLI 执行 hooks、冒烟与编译，直接在 workflow 的 `run:` 中打包，再交给 `softprops/action-gh-release` 发布，不另设打包脚本或 Nix 模块。只支持 x86_64 Linux 构建主机与运行平台。应用 derivation 不依赖本地 `.env` 或工作区状态。固定 `rust-toolchain.toml` 指定的工具链与 `Cargo.lock`，应用构建仅使用清单、锁文件、`src/` Rust 源码及许可文件、生成配置 / 模板。

本地编译 binary：

```bash
devenv build outputs.feishufsLinux
# stdout JSON 给出 store 路径，产物为 bin/feishufs 和 share/licenses/feishufs/
# flake package / app 复用同一静态 binary：
nix build
nix run .# -- --help
```

Linux 使用静态 musl。CI 验证 binary 版本及三个 CLI help，并将 binary 制作为 `.tar.gz` 与同名 `.sha256` 文件；压缩包内包含 binary、`LICENSE`、`.env.example`、`THIRD_PARTY_LICENSES.html`、`RUST-STDLIB.html` 和 `MUSL-COPYRIGHT.txt`，并验证精确成员与 SHA-256。校验文件引用压缩包 basename，可在下载目录直接校验。

第三方许可在 Nix 构建中通过锁定版本的 `cargo-about` 离线生成，配置见 [`about.toml`](about.toml)，模板见 [`misc/third-party-licenses.hbs`](misc/third-party-licenses.hbs)。清单包含 Linux musl 目标的传递依赖；AWS-LC 的复合许可证保留原始声明，校验和变化时构建失败，升级依赖后须重新核对。Rust 标准库的版权文档来自锁定工具链，musl 的声明来自锁定 libc 源码。不要以仅含本项目 MIT 许可的自制包替代这些分发文件。

在开发 shell 中可单独检查 Cargo 依赖许可（首次可能下载依赖）：

```bash
mkdir -p .cache
cargo about generate --locked --fail misc/third-party-licenses.hbs \
  --output-file .cache/THIRD_PARTY_LICENSES.html
```

统一 workflow 仅在推送 `v<版本>` tag 时发布；PR（包括 forks）、分支及其他 tag 不进入发布 job。CI 在构建前检查 tag 必须与该提交的 `Cargo.toml` 版本完全一致，不依赖本地 pre-push hook。hooks、FUSE 冒烟、binary 编译、验证及打包全部成功后，压缩包与校验文件以 artifact 传到独立发布 job，再由 `softprops/action-gh-release` 上传；构建 job 只有只读权限，发布 job 只有下载与 API 操作，不 checkout、构建或执行仓库代码。

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
- 临时实现笔记、调试记录和私有样例放 `docs/local/`；本机诊断脚本放 `scripts/`。两处均被 Git 忽略，不作为公开构建或检查的依赖。忽略规则不保护已跟踪或强制添加的文件；需要共享的私密记录应使用独立私有存储。

组织方式参考 GitHub 的 [README](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-readmes) / [CONTRIBUTING](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/setting-guidelines-for-repository-contributors) 惯例、[Diátaxis 职责拆分](https://diataxis.fr/)和[精简架构说明](https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html)。
