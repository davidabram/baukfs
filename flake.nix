{
  description = "BaukFS development and validation";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      crane,
      rust-overlay,
      ...
    }:
    let
      systems = [
        "aarch64-linux"
        "x86_64-linux"
        "aarch64-darwin"
      ];
      perSystem = nixpkgs.lib.genAttrs systems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          inherit (pkgs) lib;
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
          src = craneLib.cleanCargoSource ./.;
          architectureSrc = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              (lib.fileset.fileFilter (file: file.hasExt "rs" || file.name == "Cargo.toml") ./.)
              (lib.fileset.maybeMissing ./.cargo)
              ./architecture
            ];
          };
          commonArgs = {
            inherit src;
            pname = "baukfs";
            version = "0.1.0";
            strictDeps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          baukfs = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "--locked -p baukfs-cli";
              doCheck = false;
              doInstallCheck = true;
              installCheckPhase = ''
                runHook preInstallCheck
                "$out/bin/baukfs-cli" --help
                runHook postInstallCheck
              '';
            }
          );
        in
        {
          packages.default = baukfs;
          apps.default = {
            type = "app";
            program = "${baukfs}/bin/baukfs-cli";
            meta.description = "BaukFS CLI";
          };
          checks = {
            build = baukfs;
            fmt = craneLib.cargoFmt { inherit (commonArgs) src pname version; };
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
              }
            );
            test = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });
            architecture =
              pkgs.runCommand "baukfs-architecture"
                {
                  nativeBuildInputs = [
                    pkgs.bash
                    pkgs.ripgrep
                  ];
                }
                ''
                  bash ${architectureSrc}/architecture/self-test.sh || exit 1
                  bash ${architectureSrc}/architecture/check.sh ${architectureSrc} || exit 1
                  touch "$out"
                '';
          };
          devShells.default = craneLib.devShell {
            packages = [
              pkgs.bash
              pkgs.ripgrep
              pkgs.nixfmt
            ];
          };
          formatter = pkgs.nixfmt;
        }
      );
    in
    {
      packages = nixpkgs.lib.mapAttrs (_: value: value.packages) perSystem;
      apps = nixpkgs.lib.mapAttrs (_: value: value.apps) perSystem;
      checks = nixpkgs.lib.mapAttrs (_: value: value.checks) perSystem;
      devShells = nixpkgs.lib.mapAttrs (_: value: value.devShells) perSystem;
      formatter = nixpkgs.lib.mapAttrs (_: value: value.formatter) perSystem;
    };
}
