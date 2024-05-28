{
  description = "A nix development shell and build environment for galileo";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        flake-utils.follows = "flake-utils";
      };
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs = { nixpkgs.follows = "nixpkgs"; };
    };
  };

  outputs = { nixpkgs, flake-utils, rust-overlay, crane, ... }:
    flake-utils.lib.eachDefaultSystem
      (system:
        let
          # Set up for Rust builds, pinned to the Rust toolchain version in the Penumbra repository
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs { inherit system overlays; };
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          src = craneLib.cleanCargoSource (craneLib.path ./.);
          # Important environment variables so that the build can find the necessary libraries
          PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig";
          LIBCLANG_PATH="${pkgs.libclang.lib}/lib";
          ROCKSDB_LIB_DIR="${pkgs.rocksdb.out}/lib";
        in with pkgs; with pkgs.lib; let
	galileo = (craneLib.buildPackage {
            inherit src system PKG_CONFIG_PATH LIBCLANG_PATH ROCKSDB_LIB_DIR;
            pname = "galileo";
            version = "0.1.0";
            nativeBuildInputs = [ pkg-config ];
            buildInputs = [ clang openssl rocksdb ];
	    # A lockfile is not used in this repository.
            cargoVendorDir = null;
        }).overrideAttrs (_: { doCheck = false; }); # Disable tests to improve build times
	in {
	  inherit galileo;
	  devShells.default = craneLib.devShell {
            inherit LIBCLANG_PATH ROCKSDB_LIB_DIR;
            inputsFrom = [ galileo ];
            packages = [ cargo-watch cargo-nextest rocksdb ];
            shellHook = ''
              export LIBCLANG_PATH=${LIBCLANG_PATH}
              export RUST_SRC_PATH=${pkgs.rustPlatform.rustLibSrc} # Required for rust-analyzer
              export ROCKSDB_LIB_DIR=${ROCKSDB_LIB_DIR}
            '';
	  };
	}
      );
}
