{ pkgs ? import (import ./npins).nixpkgs { } }:

{
  nixcache-test = pkgs.writeShellScriptBin "nixcache-test" ''
    echo "Hello from nixcache-oci! Cache is working."
    echo "Built at: 2026-04-05"
  '';
}
