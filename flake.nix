{
  description = "vec — semantic file search. locate finds files by name, vec finds files by meaning.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "vec";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # The binary is in the vec-cli workspace member.
          cargoBuildFlags = [ "-p" "vec-cli" ];
          cargoTestFlags = [ "--workspace" ];

          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = with pkgs; [ sqlite ];

          meta = with pkgs.lib; {
            description = "Semantic file search — locate finds files by name, vec finds files by meaning";
            license = licenses.mit;
            mainProgram = "vec";
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [ rust-analyzer clippy rustfmt ];
        };
      }
    );
}
