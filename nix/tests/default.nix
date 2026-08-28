{ pkgs ? import (import ../../npins).nixpkgs { } }:
{
  static = import ./static.nix { inherit pkgs; };
  vmtest = import ./vmtest.nix { inherit pkgs; };
}
