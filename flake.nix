{
  description = "FeishuFS: read-only Feishu/Lark filesystem";

  inputs = {
    nixpkgs.url = "github:cachix/devenv-nixpkgs/rolling";
    devenv = {
      url = "github:cachix/devenv";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.git-hooks.follows = "git-hooks";
      inputs.rust-overlay.follows = "rust-overlay";
    };
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      devenv,
      ...
    }@inputs:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      project = devenv.lib.mkConfig {
        inherit inputs pkgs;
        modules = [
          ./devenv.nix
          # A source-store root keeps package evaluation pure and portable.
          # No shell, hooks or local workspace configuration is executed here.
          { devenv.root = toString self.outPath; }
        ];
      };
      application = project.outputs.feishufsLinux;
      app = {
        type = "app";
        program = "${application}/bin/feishufs";
        meta.description = "FeishuFS Linux static binary";
      };
    in
    {
      packages.${system} = {
        default = application;
        feishufs = application;
      };
      apps.${system} = {
        default = app;
        feishufs = app;
      };
    };
}
