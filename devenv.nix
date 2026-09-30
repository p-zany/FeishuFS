{
  pkgs,
  lib,
  ...
}:

{
  languages.rust = {
    enable = true;
    channel = "stable";
    version = "1.88.0";
    targets = [ "x86_64-pc-windows-msvc" ];
  };

  packages = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
    pkgs.cargo-xwin
    pkgs.fuse3.bin
    pkgs.llvmPackages.llvm
  ];

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
    "feishufs:fmt".exec = "cargo fmt --all -- --check";
    "feishufs:clippy".exec = "cargo clippy --all-targets --all-features -- -D warnings";
    "feishufs:test".exec = "cargo test --all-targets --all-features";
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
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets --all-features
  '';
}
