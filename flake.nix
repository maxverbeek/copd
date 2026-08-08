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
    };
}
