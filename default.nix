{ pkgs ? import (import ./npins).nixpkgs { } }:

{
  cache-proxy = pkgs.rustPlatform.buildRustPackage {
    pname = "nixcache-proxy";
    version = "0.1.0";
    src = ./.;
    cargoLock = {
      lockFile = ./Cargo.lock;
    };
    buildAndTestSubdir = "crates/nixcache-proxy";
  };

  cache-builder = pkgs.rustPlatform.buildRustPackage {
    pname = "nixcache-builder";
    version = "0.1.0";
    src = ./.;
    cargoLock = {
      lockFile = ./Cargo.lock;
    };
    buildAndTestSubdir = "crates/nixcache-builder";
  };
}
