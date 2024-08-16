{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/24.05";
    flake-utils.url = "github:numtide/flake-utils";
    nix-filter.url = "github:numtide/nix-filter";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      nix-filter,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell { nativeBuildInputs = [ pkgs.nix-eval-jobs ]; };
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "nix-many-build";
          version = "0.1.0";
          src = nix-filter.lib.filter {
            root = ./.;
            include = [
              "src"
              "Cargo.toml"
              "Cargo.lock"
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postFixup = ''
            wrapProgram $out/bin/nix-many-build --prefix PATH : ${
              pkgs.lib.makeBinPath [
                pkgs.nix-eval-jobs
                pkgs.nix
              ]
            }
          '';
          doCheck = false;
        };
        test = import ./test/test.nix { inherit pkgs; };
      }
    );
}
