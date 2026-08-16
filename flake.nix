{
  description = "kkagent development flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rust = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" "rustfmt" ];
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rust
            pkgs.pkg-config
            pkgs.openssl
            pkgs.cmake
            pkgs.git
            pkgs.nodePackages.nodejs
          ];
          shellHook = ''
            echo "kkagent nix shell — rustc $(rustc --version)"
          '';
        };
      });
}
