{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, rust-overlay, crane, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };
      rust = pkgs.rust-bin.stable.latest.default.override {
        targets = [ "wasm32-unknown-unknown" ];
      };
      craneLib = (crane.mkLib pkgs).overrideToolchain rust;
      commonArgs = {
        src = self;
        strictDeps = true;
        doCheck = false;
      };

      # Everything Cargo.lock pins, compiled once: the wasm crates, the native
      # bindgen in xtask, and the native test build. It only changes with
      # Cargo.lock, so the binary cache serves it to local `nix build`s too.
      deps = craneLib.buildDepsOnly (commonArgs // {
        buildPhaseCargoCommand = ''
          cargo build --release --target wasm32-unknown-unknown -p copd
          cargo build -p xtask
          cargo test --release -p copd --no-run
        '';
      });
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = [ rust pkgs.wasm-pack pkgs.python3 ];
      };

      # The site: index.html plus the wasm-bindgen output it imports from ./pkg.
      packages.${system} = {
        inherit deps;
        default = craneLib.mkCargoDerivation (commonArgs // {
          cargoArtifacts = deps;
          doInstallCargoArtifacts = false;
          buildPhaseCargoCommand = ''
            cargo build --release --target wasm32-unknown-unknown -p copd
            cargo run -p xtask -- target/wasm32-unknown-unknown/release/copd.wasm pkg
          '';
          installPhaseCommand = ''
            mkdir -p $out
            cp -r index.html pkg $out/
          '';
        });
      };

      # `nix flake check` builds the site and runs the host-side unit tests. The
      # wasm build can't run tests (no wasm runtime in the sandbox), so the tests
      # run natively.
      checks.${system} = {
        build = self.packages.${system}.default;
        tests = craneLib.cargoTest (commonArgs // {
          cargoArtifacts = deps;
          cargoTestExtraArgs = "-p copd";
        });
      };

      # Serving lives here rather than in the consuming config, so the allowlist
      # sits next to the installPhase that decides which files exist. "/" is
      # index.html via file_server's index resolution.
      nixosModules.default =
        { config, lib, ... }:
        let
          cfg = config.services.copd;
          served = [ "/" "/pkg/copd.js" "/pkg/copd_bg.wasm" ];
        in
        {
          options.services.copd = {
            enable = lib.mkEnableOption "the copd static site";
            hostName = lib.mkOption {
              type = lib.types.str;
              description = "Domain Caddy serves the site on.";
            };
          };

          config = lib.mkIf cfg.enable {
            services.caddy.enable = true;
            services.caddy.virtualHosts.${cfg.hostName}.extraConfig = ''
              root * ${self.packages.${system}.default}

              @allowed path ${lib.concatStringsSep " " served}
              handle @allowed {
                file_server
              }
              handle {
                respond 404
              }
            '';
          };
        };
    };
}
