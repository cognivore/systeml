{
  description = "SystemL — systemd-compatible user-mode service manager for macOS";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        rust = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "clippy"
            "rustfmt"
          ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rust
          ]
          ++ (with pkgs; [
            pkg-config
            cargo-nextest
            cargo-watch
            nixfmt-rfc-style
          ]);
          nativeBuildInputs = with pkgs; [ pkg-config ];
        };

        packages = {
          default = self.packages.${system}.systeml;
          systeml = pkgs.callPackage ./nix/package.nix { };
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    )
    // {
      homeManagerModules.default = import ./nix/home-manager-module.nix;
      homeManagerModules.systeml = self.homeManagerModules.default;
    };
}
