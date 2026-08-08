{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };
      rust = pkgs.rust-bin.stable.latest.default.override {
        targets = [ "wasm32-unknown-unknown" ];
      };
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = [ rust pkgs.wasm-pack pkgs.python3 ];
      };

      # The site: index.html plus the wasm-bindgen output it imports from ./pkg.
      # wasm-pack is avoided on purpose — it wants to fetch its own wasm-bindgen
      # at build time, which a sandboxed build can't do. Plain cargo plus
      # nixpkgs' wasm-bindgen-cli does the same job, but the crate version in
      # Cargo.lock must match that CLI exactly or the bindings mismatch.
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "copd";
        version = "0.1.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [ rust pkgs.wasm-bindgen-cli ];

        buildPhase = ''
          runHook preBuild
          cargo build --release --target wasm32-unknown-unknown --offline
          runHook postBuild
        '';

        installPhase = ''
          runHook preInstall
          mkdir -p $out
          cp index.html $out/
          wasm-bindgen --target web --out-dir $out/pkg --no-typescript \
            target/wasm32-unknown-unknown/release/copd.wasm
          runHook postInstall
        '';

        doCheck = false;
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
