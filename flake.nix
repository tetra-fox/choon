{
  description = "choon, which tells whether an audio file is what its format claims";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = lib.genAttrs systems;
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          # the oldest Rust the manifest's rust-version promises, which rust-overlay names in full
          rustVersion = (lib.importTOML ./Cargo.toml).package.rust-version;
          msrv =
            pkgs.rust-bin.stable.${
              if lib.length (lib.splitString "." rustVersion) == 2 then "${rustVersion}.0" else rustVersion
            }.minimal;
          # cargo-fuzz builds with -Zsanitizer, which only nightly accepts
          nightly = pkgs.rust-bin.selectLatestNightlyWith (
            toolchain: toolchain.minimal.override { extensions = [ "rust-src" ]; }
          );
          # wasm-bindgen's program writes JavaScript for the wasm-bindgen crate of its own version
          # alone, so it is the one web/Cargo.toml pins
          wasmBindgen =
            pkgs."wasm-bindgen-cli_${
              lib.replaceStrings [ "=" "." ] [ "" "_" ]
                (lib.importTOML ./web/Cargo.toml).dependencies.wasm-bindgen
            }";
          encoders = with pkgs; [
            ffmpeg
            lame
            opus-tools
            vorbis-tools
            fdk-aac-encoder
          ];
        in
        {
          # the test fixtures build LAME's C source with the shell's compiler and make, and
          # libopus's with CMake
          default = pkgs.mkShell {
            packages = with pkgs; [
              (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              cargo-nextest
              cmake
              just
              git
            ];

            env.RUST_BACKTRACE = "1";
          };

          msrv = pkgs.mkShell { packages = [ msrv ]; };

          # the encoders samples/make.sh runs
          samples = pkgs.mkShell { packages = encoders; };

          # what torture/make.sh runs beside them: the lossless formats' own programs
          torture = pkgs.mkShell {
            packages =
              encoders
              ++ (with pkgs; [
                flac
                wavpack
                monkeysAudio
                # torture/run.py
                python3
              ]);
          };

          # what corpus/corpus runs: the encoders and decoders its files are made with, and
          # cargo for the reader it builds
          corpus = pkgs.mkShell {
            packages = with pkgs; [
              (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              ffmpeg-full
              lame
              sox
              mpg123
              faad2
              python3
              git
            ];
          };

          fuzz = pkgs.mkShell {
            packages = [
              nightly
              pkgs.cargo-fuzz
              pkgs.just
            ];
          };

          # what `just web` and `just web-test` run: binaryen for wasm-opt, node to run the
          # wasm, and CMake for the encoders the test's fixtures build
          web = pkgs.mkShell {
            packages = with pkgs; [
              (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              wasmBindgen
              binaryen
              nodejs
              cmake
              just
            ];
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);
    };
}
