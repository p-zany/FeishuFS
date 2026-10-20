{
  config,
  pkgs,
  lib,
  ...
}:

let
  toolchain = config.languages.rust.toolchainPackage;
  manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
  inherit (manifest.package) version;
  musl = pkgs.pkgsCross.musl64.stdenv.cc.libc;
  # Only these sources enter the build sandbox, never .env, docs/local,
  # tests/fixtures, logs, target or caches from the working tree.
  sourceFiles = lib.fileset.unions [
    ./Cargo.toml
    ./Cargo.lock
    ./LICENSE
    ./about.toml
    ./misc/third-party-licenses.hbs
    (lib.fileset.fileFilter (file: file.hasExt "rs") ./src)
  ];
  src = lib.fileset.toSource {
    root = ./.;
    fileset = sourceFiles;
  };
  muslPlatform = pkgs.makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
    stdenv = pkgs.pkgsCross.musl64.stdenv;
  };
  common = {
    pname = "feishufs";
    inherit version src;
    cargoLock.lockFile = ./Cargo.lock;
    cargoBuildFlags = [
      "--locked"
      "--bin"
      "feishufs"
    ];
    auditable = false;
    # Normal tests run through `devenv test`; never run mounts or live APIs here.
    doCheck = false;
  };
  linux = muslPlatform.buildRustPackage (
    common
    // {
      # aws-lc-sys 的构建脚本需要 cmake；musl 目标默认 crt-static，无需额外 RUSTFLAGS。
      nativeBuildInputs = [
        pkgs.cmake
        pkgs.cargo-about
      ];
      postInstall = ''
        notices="$out/share/licenses/feishufs"
        mkdir -p "$notices"
        cp LICENSE "$notices/LICENSE"
        # cargo-about can fall back after a clarification checksum mismatch.
        # Treat warnings as failures so changed notices require review.
        cargo about -L warn --color never generate --offline --locked --fail \
          misc/third-party-licenses.hbs --output-file "$notices/THIRD_PARTY_LICENSES.html" \
          2> license-diagnostics.log || { cat license-diagnostics.log >&2; exit 1; }
        if [ -s license-diagnostics.log ]; then
          cat license-diagnostics.log >&2
          exit 1
        fi
        cp ${toolchain}/share/doc/rust/COPYRIGHT-library.html "$notices/RUST-STDLIB.html"
        tar -xOf ${musl.src} musl-${musl.version}/COPYRIGHT > "$notices/MUSL-COPYRIGHT.txt"
        test -s "$notices/THIRD_PARTY_LICENSES.html"
        test -s "$notices/RUST-STDLIB.html"
        test -s "$notices/MUSL-COPYRIGHT.txt"
      '';
    }
  );

in
{
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  packages = [
    pkgs.lark-cli
    pkgs.nixfmt
    pkgs.actionlint
    pkgs.cargo-about
    pkgs.jq
    pkgs.gnutar
    pkgs.gzip
  ];

  outputs = {
    feishufsLinux = linux;
  };

  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy = {
      enable = true;
      settings = {
        allFeatures = true;
        denyWarnings = true;
        extraArgs = "--locked --all-targets";
      };
    };
    nixfmt.enable = true;
    actionlint.enable = true;
    cargo-test = {
      enable = true;
      entry = "cargo test --locked --all-targets --all-features";
      pass_filenames = false;
    };
  };

  # 常规检查全部由 git hooks 覆盖；enterTest 只保留需要真实挂载的 FUSE 冒烟。
  tasks = {
    "feishufs:smoke-linux" = {
      exec = "cargo test --locked --test fuse_smoke -- --ignored --nocapture";
      wantedBy = [ "devenv:enterTest" ];
    };
  };
}
