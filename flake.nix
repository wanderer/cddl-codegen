{
  description = "cddl-codegen: Generate Rust, WASM and JSON code from CDDL specifications";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          overlays = [(import rust-overlay)];
          inherit system;
        };

        rustToolchain = pkgs.rust-bin.nightly.latest.default;

        nativeBuildInputs = with pkgs; [
          rustToolchain
          pkg-config
          which
          rustfmt
          makeWrapper
        ];
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = nativeBuildInputs;
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "cddl-codegen";
          version = "0.1.0";
          src = ./.;

          inherit nativeBuildInputs;

          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          # the regular tests don't run yet
          checkPhase = ''
            cargo test comment_ast
          '';

          postInstall = ''
            wrapProgram $out/bin/cddl-codegen \
              --set PATH ${pkgs.lib.makeBinPath [pkgs.rustfmt pkgs.which]} \
              --add-flags "--static-dir ${./static}"
          '';

          meta = with pkgs.lib; {
            description = "Codegen serialization logic for CBOR automatically from a CDDL specification";
            homepage = "https://github.com/dcSpark/cddl-codegen";
            license = licenses.mit;
            maintainers = [];
          };
        };
      }
    );
}
