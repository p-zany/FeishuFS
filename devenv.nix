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
  # Only these sources enter the build sandbox, never .env, docs/local,
  # tests/fixtures, logs, target or caches from the working tree.
  sourceFiles = lib.fileset.unions [
    ./Cargo.toml
    ./Cargo.lock
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
      nativeBuildInputs = [ pkgs.cmake ];
      env.CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "-C target-feature=+crt-static";
    }
  );

in
{
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  # The CLI bundled with devenv loads dotenv credentials only for the app:
  # secretspec run --provider dotenv -- ./target/debug/feishufs inspect '<link>'
  # Keep normal shells, checks and builds credential-free.

  packages = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
    pkgs.lark-cli
    pkgs.nixfmt
    pkgs.actionlint
    pkgs.jq
    pkgs.binutils
    pkgs.gnutar
    pkgs.gzip
  ];

  # Binary-only outputs, also consumed by flake.nix through devenv.lib.mkConfig.
  # Validation and release packaging live in the CI workflow, not derivations.
  outputs = lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
    feishufsLinux = linux;
  };

  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy = {
      enable = true;
      settings = {
        allFeatures = true;
        denyWarnings = true;
      };
    };
    nixfmt.enable = true;
  };

  tasks = {
    "feishufs:nix-format".exec = "nixfmt --check flake.nix devenv.nix";
    "feishufs:workflow".exec = "actionlint .github/workflows/ci.yml";
    "feishufs:fmt".exec = "cargo fmt --all -- --check";
    "feishufs:clippy".exec = "cargo clippy --locked --all-targets --all-features -- -D warnings";
    "feishufs:test".exec = "cargo test --locked --all-targets --all-features";
    "feishufs:smoke-linux".exec = ''
      if [ "''${FEISHUFS_FUSE_SUDO:-0}" = 1 ]; then
        cargo test --test fuse_smoke --no-run
        test_binary="$(${pkgs.findutils}/bin/find target/debug/deps -maxdepth 1 -type f -name 'fuse_smoke-*' -perm -u+x -printf '%T@ %p\n' | sort -nr | head -1 | cut -d' ' -f2-)"
        sudo env PATH="$PATH" "$test_binary" --ignored --nocapture
      else
        cargo test --test fuse_smoke -- --ignored --nocapture
      fi
    '';
  };

  enterTest = ''
    nixfmt --check flake.nix devenv.nix
    actionlint .github/workflows/ci.yml
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked --all-targets --all-features
  '';
}
