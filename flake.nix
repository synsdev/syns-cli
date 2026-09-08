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

        # PD-3: hermetic source closure. An allowlist via lib.fileset.toSource
        # keeps target/ (multi-GB), .git/, .idea/, .github/, dist-workspace.toml,
        # LICENSE, README.md, tests/ out of the /nix/store source-hash input.
        # Without this, any `cargo build` outside Nix would invalidate the hash
        # and force a multi-gigabyte source recopy on the next `nix build`.
        # tests/ is intentionally excluded — doCheck = false below means cargo
        # test never runs inside the sandbox; a future SPEC change that re-enables
        # the test phase would also need to add ./tests to this allowlist.
        synsSrc = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./build.rs
            ./rust-toolchain.toml
            ./src
          ];
        };

        synsPackage = rustPlatform'.buildRustPackage {
          pname = "syns";
          version = cargoMeta.version;

          src = synsSrc;

          # PD-3 addendum: vendor through `fetchCargoVendor` (cargoHash)
          # rather than `cargoLock` (importCargoLock).
          #
          # `importCargoLock` fetches every crate with one `fetchurl` per
          # crate, from `https://crates.io/api/v1/crates/<name>/<version>/download`.
          # crates.io answers that endpoint with 403 for a request whose
          # User-Agent is curl's default, which is what the fetchurl
          # builder sends — so `nix build` failed on whichever crate was
          # not already in the store, on every push, regardless of what
          # the commit changed. `NIX_CURL_FLAGS` is the documented lever
          # for that builder, but it is an impure env var read from the
          # nix DAEMON's environment, so a workflow-level `env:` never
          # reaches it.
          #
          # `fetchCargoVendor` runs `cargo vendor` inside one fixed-output
          # derivation instead: cargo fetches through the sparse index and
          # `static.crates.io` under its own User-Agent, which crates.io
          # serves. One FOD also replaces ~200 per-crate ones.
          #
          # `cargoHash` covers Cargo.lock's whole closure, so it changes
          # whenever the lockfile does; `nix build` prints the new value.
          cargoHash = "sha256-78fNUSvv1Ac7xYXU+x0nuuCHo0o/8JEzESoxtBTomR0=";

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];

          # PD-4 / SPEC § 2 Out of scope: the flake-check gate is build-and-help-print
          # only. cargo test stays in u182's ci.yml `test` job. Flipping this to true
          # would run cargo test inside the Nix sandbox and gate the flake on test-suite
          # behavior the SPEC explicitly excludes ("Cross-cutting test-on-Nix work
          # would belong to a separate roadmap entry") — DO NOT flip without an
          # updated SPEC.
          doCheck = false;

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
