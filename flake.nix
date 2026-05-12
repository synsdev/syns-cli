{
  description = "syns CLI — Nix flake (build syns from source via rustPlatform.buildRustPackage)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # Construct a rustPlatform whose cargo + rustc come from rust-toolchain.toml.
        # Per https://nixos.org/manual/nixpkgs/stable/#rust-section, this is the
        # canonical pattern — pkgs.rustPlatform.override does NOT propagate to
        # cargo/rustc because they are constructor inputs to makeRustPlatform,
        # not attribute-set override-points on the constructed platform.
        rustPlatform' = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        cargoMeta = (pkgs.lib.importTOML ./Cargo.toml).package;

        synsPackage = rustPlatform'.buildRustPackage {
          pname = "syns";
          version = cargoMeta.version;

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];

          meta = {
            description = cargoMeta.description;
            homepage = "https://github.com/synsdev/syns-cli";
            license = pkgs.lib.licenses.mit;
            mainProgram = "syns";
          };
        };
      in
      {
        packages.default = synsPackage;

        apps.default = {
          type = "app";
          program = "${synsPackage}/bin/syns";
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.gh
            pkgs.git
            pkgs.openssl
            pkgs.pkg-config
          ];
        };
      });
}
