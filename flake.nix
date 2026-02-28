{
  description = "EmmyLua Language Server";

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
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ rust-overlay.overlays.default ];
        pkgs = import nixpkgs { inherit system overlays; };
      in
      {
        packages =
          let
            packages = import ./nix/packages.nix;
          in
          (builtins.mapAttrs (name: value: pkgs.callPackage value { }) packages)
          // {
            default = self.packages.${system}.glua_ls;
          };

        devShells.default = pkgs.mkShell {
          buildInputs = (with pkgs; [
            rust-analyzer
            clippy
            rustfmt
          ])
          ++ self.packages.${system}.default.buildInputs
          ++ self.packages.${system}.default.nativeBuildInputs;
        };
      }
    );
}
