{ pkgs ? import (import ./npins).nixpkgs { } }:

let
  packages = {
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
  };
in
{
  # Legacy & top-level package shortcuts for convenience
  inherit (packages) cache-proxy cache-builder;

  # Align with flake output structure
  inherit packages;

  apps = {
    cache-proxy = {
      type = "app";
      program = "${packages.cache-proxy}/bin/nixcache-proxy";
    };
  };

  nixosModules = {
    default = import ./nix/module.nix;
  };

  nixConfig = {
    extra-substituters = [ "http://localhost:37515" ];
    extra-trusted-public-keys = [ ];
  };
}
