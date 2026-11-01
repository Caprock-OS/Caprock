{
  description = "Caprock — capability-based Rust microkernel";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];

      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in {
      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              # rustup honors rust-toolchain.toml (`rustup run nightly …`); the
              # nightly toolchain (with rust-src for `-Z build-std`) is provisioned
              # in shellHook below.
              rustup
              # QEMU system emulators for both guest targets (test-qemu*.sh,
              # run-qemu.sh): qemu-system-x86_64 and qemu-system-aarch64.
              qemu
              # Signing tools need `cryptography` (gen_trusted_key.py, …).
              (python3.withPackages (p: [ p.cryptography ]))
              # objcopy/objdump/strip for image conversion (*.mb32) and introspection.
              binutils
              gnumake
              curl
              unzip
              file
            ];

            shellHook = ''
              # Toolchain + cargo caches live inside the repo (portable, gitignored).
              export RUSTUP_HOME="$PWD/.nix/rustup"
              export CARGO_HOME="$PWD/.nix/cargo"
              mkdir -p "$RUSTUP_HOME" "$CARGO_HOME"

              # Pin per rust-toolchain.toml: nightly + rust-src, rustfmt, llvm-tools.
              # Idempotent: rustup reuses an already-installed toolchain.
              rustup toolchain install nightly --profile minimal \
                --component rust-src,rustfmt,llvm-tools
            '';
          };
        });
    };
}
