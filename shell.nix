{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    pkg-config
    npins
  ];

  buildInputs = with pkgs; [
    openssl
  ];
}
