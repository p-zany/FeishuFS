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
  # Windows compile-only checks also need public integration-test Rust sources,
  # including tests/support, but never fixtures or workspace contents.
  windowsSrc = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.union sourceFiles (
      lib.fileset.difference (lib.fileset.fileFilter (file: file.hasExt "rs") ./tests) (
        lib.fileset.maybeMissing ./tests/fixtures
      )
    );
  };
  rustPlatform = pkgs.makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
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

  # Use nixpkgs' locked SDK downloader, not a project-owned one. Accept the
  # Microsoft license only in this scoped package set; the SDK is build-only.
  sdkPkgs = import pkgs.path {
    system = pkgs.stdenv.hostPlatform.system;
    config = {
      microsoftVisualStudioLicenseAccepted = true;
      allowUnfreePredicate =
        pkg:
        lib.elem (lib.getName pkg) [
          "win-sdk"
          "xwin-fetch-msvc"
        ];
    };
  };
  msvcSdk = sdkPkgs.windows.sdk;
  windows = rustPlatform.buildRustPackage (
    common
    // {
      src = windowsSrc;
      nativeBuildInputs = [
        pkgs.cargo-xwin
        pkgs.cmake
        pkgs.nasm
        pkgs.llvmPackages.clang-unwrapped
        pkgs.llvmPackages.lld
        pkgs.llvmPackages.llvm
      ];
      env = {
        XWIN_ARCH = "x86_64";
        RUSTFLAGS = "-C target-feature=+crt-static";
        CARGO_PROFILE_RELEASE_STRIP = "symbols";
      };
      buildPhase = ''
        runHook preBuild
        export XWIN_CACHE_DIR="$TMPDIR/xwin-cache"
        mkdir -p "$XWIN_CACHE_DIR/xwin"
        cp -R ${msvcSdk}/crt ${msvcSdk}/sdk "$XWIN_CACHE_DIR/xwin/"
        chmod -R u+w "$XWIN_CACHE_DIR"
        # nixpkgs preserves Microsoft's x64 names; cargo-xwin expects x86_64.
        ln -s x64 "$XWIN_CACHE_DIR/xwin/crt/lib/x86_64"
        ln -s x64 "$XWIN_CACHE_DIR/xwin/sdk/lib/um/x86_64"
        ln -s x64 "$XWIN_CACHE_DIR/xwin/sdk/lib/ucrt/x86_64"
        printf 'x86_64\n' > "$XWIN_CACHE_DIR/xwin/DONE"
        # Preserve CI's Windows-only unit/integration-test compilation coverage.
        # This uses the same sealed SDK/cache and never executes Windows tests.
        echo "Checking all Windows targets/features (compile-only)"
        cargo xwin check --frozen --target x86_64-pc-windows-msvc \
          --all-targets --all-features -j "$NIX_BUILD_CORES"
        cargo xwin build --release --frozen --target x86_64-pc-windows-msvc \
          --bin feishufs -j "$NIX_BUILD_CORES"
        runHook postBuild
      '';
      installPhase = ''
        runHook preInstall
        install -Dm755 target/x86_64-pc-windows-msvc/release/feishufs.exe "$out/bin/feishufs.exe"
        runHook postInstall
      '';
      dontStrip = true;
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
    pkgs.cargo-xwin
    pkgs.lark-cli
    pkgs.llvmPackages.llvm
    pkgs.nixfmt
    pkgs.actionlint
    pkgs.jq
    pkgs.binutils
    pkgs.gnutar
    pkgs.gzip
    pkgs.zip
    pkgs.unzip
  ];

  # Binary-only outputs, also consumed by flake.nix through devenv.lib.mkConfig.
  # Validation and release packaging live in the CI workflow, not derivations.
  outputs = lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
    feishufsLinux = linux;
    feishufsWindows = windows;
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
    "feishufs:check-windows".exec =
      "cargo xwin check --target x86_64-pc-windows-msvc --all-targets --all-features";
  };

  enterTest = ''
    nixfmt --check flake.nix devenv.nix
    actionlint .github/workflows/ci.yml
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked --all-targets --all-features
  '';
}
