{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.treefmt-nix = {
    url = "github:numtide/treefmt-nix";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  inputs.nix-darwin = {
    url = "github:nix-darwin/nix-darwin";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      treefmt-nix,
      nix-darwin,
      ...
    }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.callPackage ./nix/package.nix { };
        }
      );

      formatter = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        (import ./nix/treefmt.nix { inherit pkgs treefmt-nix; }).config.build.wrapper
      );

      nixosModules.default = ./nix/module.nix;
      darwinModules.default = ./nix/darwin-module.nix;

      # Activated on a macOS runner; see the darwin-module workflow.
      darwinConfigurations.ci = import ./nix/darwin-test.nix {
        system = "aarch64-darwin";
        darwin = nix-darwin;
        darwinModule = self.darwinModules.default;
        activate = true;
      };

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          proptest = pkgs.callPackage ./nix/proptest.nix { };
          treefmt = (import ./nix/treefmt.nix { inherit pkgs treefmt-nix; }).config.build.check ./.;
          # Realize the flake inputs so the binary cache holds their
          # source trees; later runs fetch from the cache instead of
          # re-downloading.
          flake-inputs = pkgs.linkFarm "flake-inputs" (builtins.removeAttrs inputs [ "self" ]);
        }
        // nixpkgs.lib.mapAttrs' (n: nixpkgs.lib.nameValuePair "package-${n}") self.packages.${system}
        // nixpkgs.lib.mapAttrs' (n: nixpkgs.lib.nameValuePair "devshell-${n}") self.devShells.${system}
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          nixos-test = import ./nix/nixos-test.nix { inherit pkgs; };
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.isDarwin {
          darwin-module =
            (import ./nix/darwin-test.nix {
              inherit system;
              darwin = nix-darwin;
              darwinModule = self.darwinModules.default;
            }).system;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.callPackage ./nix/shell.nix { };
        }
      );
    };
}
