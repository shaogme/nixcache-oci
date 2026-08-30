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
      preBuild = ''
        export HOME=$(mktemp -d)
      '';
      nativeCheckInputs = [ pkgs.cacert ];
      SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    };

    cache-builder = pkgs.rustPlatform.buildRustPackage {
      pname = "nixcache-builder";
      version = "0.1.0";
      src = ./.;
      cargoLock = {
        lockFile = ./Cargo.lock;
      };
      buildAndTestSubdir = "crates/nixcache-builder";
      preBuild = ''
        export HOME=$(mktemp -d)
      '';
      nativeCheckInputs = [ pkgs.cacert ];
      SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      postInstall = ''
        ln -s ${packages.cache-proxy}/bin/nixcache-proxy $out/bin/nixcache-proxy
      '';
    };

    cache-proxy-bin = import ./nix/binary.nix { inherit pkgs; pname = "nixcache-proxy"; };
    cache-builder-bin = import ./nix/binary.nix { inherit pkgs; pname = "nixcache-builder"; };
  };
in
{
  # Legacy & top-level package shortcuts for convenience
  inherit (packages) cache-proxy cache-builder cache-proxy-bin cache-builder-bin;

  # Align with flake output structure
  inherit packages;

  apps = {
    cache-proxy = {
      type = "app";
      program = "${packages.cache-proxy}/bin/nixcache-proxy";
    };
    cache-builder = {
      type = "app";
      program = "${packages.cache-builder}/bin/nixcache-builder";
    };
    cache-proxy-bin = {
      type = "app";
      program = "${packages.cache-proxy-bin}/bin/nixcache-proxy";
    };
    cache-builder-bin = {
      type = "app";
      program = "${packages.cache-builder-bin}/bin/nixcache-builder";
    };
  };

  nixosModules = {
    default = import ./nix/module.nix;
  };

  tests = import ./nix/tests { inherit pkgs; };
  checks = import ./nix/tests { inherit pkgs; };

  nixConfig = {
    extra-substituters = [ "http://localhost:37515" ];
    extra-trusted-public-keys = [ ];
  };
}
