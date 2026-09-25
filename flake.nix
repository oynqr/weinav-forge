{
  description = "Build GNSS assistance data for Huawei watches and Gadgetbridge";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
      ...
    }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems = lib.genAttrs systems;

      projectFor =
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          musl = pkgs.pkgsStatic;
          target = musl.stdenv.hostPlatform.rust.rustcTarget;
          targetName = builtins.replaceStrings [ "-" ] [ "_" ] target;
          compiler = "${musl.stdenv.cc}/bin/${musl.stdenv.cc.targetPrefix}cc";
          compilerCxx = "${musl.stdenv.cc}/bin/${musl.stdenv.cc.targetPrefix}c++";

          toolchainFor =
            p:
            p.rust-bin.selectLatestNightlyWith (
              toolchain:
              toolchain.minimal.override {
                extensions = [
                  "clippy"
                  "rustfmt"
                  "rust-src"
                ];
                targets = [ target ];
              }
            );

          rustSource = (toolchainFor pkgs).passthru.availableComponents.rust-src;

          craneLib = (crane.mkLib pkgs).overrideToolchain toolchainFor;

          src = craneLib.cleanCargoSource ./.;

          crateArgs = {
            inherit src;

            strictDeps = true;

            cargoVendorDir = craneLib.vendorMultipleCargoDeps {
              inherit (craneLib.findCargoFiles src) cargoConfigs;
              cargoLockList = [
                ./Cargo.lock
                "${rustSource}/lib/rustlib/src/rust/library/Cargo.lock"
              ];
            };

            cargoBuildExtraArgs = "-Z build-std=std,panic_abort -Z build-std-features=optimize_for_size";

            nativeBuildInputs = [
              musl.stdenv.cc
              pkgs.cmake
              pkgs.git
            ];

            LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "--target=${target} -isystem ${lib.getDev musl.stdenv.cc.libc}/include";
            CARGO_BUILD_TARGET = target;
            CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static -C relocation-model=static -C link-arg=-lgcc";
          }
          // {
            "CARGO_TARGET_${lib.toUpper targetName}_LINKER" = compiler;
            "CC_${targetName}" = compiler;
            "CXX_${targetName}" = compilerCxx;
          };

          cargoArtifacts = craneLib.buildDepsOnly crateArgs;
        in
        {
          inherit
            pkgs
            craneLib
            crateArgs
            cargoArtifacts
            ;

          binary = craneLib.buildPackage (crateArgs // { inherit cargoArtifacts; });
        };

      project = forAllSystems projectFor;
    in
    {
      nixosModules.default =
        { lib, pkgs, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.weinav-forge.package =
            lib.mkDefault
              self.packages.${pkgs.stdenv.hostPlatform.system}.weinav-forge;
        };
      nixosModules.weinav-forge = self.nixosModules.default;

      packages = forAllSystems (system: {
        default = project.${system}.binary;
        weinav-forge = project.${system}.binary;
      });

      checks = forAllSystems (
        system:
        let
          this = project.${system};
        in
        {
          inherit (this) binary;

          clippy = this.craneLib.cargoClippy (
            this.crateArgs
            // {
              inherit (this) cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- -D warnings";
            }
          );

          format = this.craneLib.cargoFmt { inherit (this.crateArgs) src; };
          module-evaluation = import ./nix/tests/evaluation.nix {
            inherit nixpkgs system;
            inherit (this) pkgs;
            module = self.nixosModules.default;
          };
          module-vm = import ./nix/tests/vm.nix {
            inherit (this) pkgs;
            module = self.nixosModules.default;
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          this = project.${system};
        in
        {
          default = this.craneLib.devShell {
            packages = with this.pkgs; [
              clang
              cmake
              curl
              git
              jq
              perl
              pkg-config
            ];

            LIBCLANG_PATH = "${this.pkgs.libclang.lib}/lib";
            LD_LIBRARY_PATH = lib.makeLibraryPath [ this.pkgs.stdenv.cc.cc.lib ];
          };

        }
      );
    };
}
