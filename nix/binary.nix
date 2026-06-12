{ pkgs ? import <nixpkgs> {}, pname ? "nixcache-proxy" }:

let
  binaries = builtins.fromJSON (builtins.readFile ./binaries.json);
  system = pkgs.system;
  
  hasPname = builtins.hasAttr pname binaries;
  pnameBinaries = if hasPname then binaries.${pname} else throw "Unsupported binary package: ${pname}";

  hasBinary = builtins.hasAttr system pnameBinaries;
  target = if hasBinary then pnameBinaries.${system} else throw "Unsupported system for pre-compiled binary ${pname}: ${system}";

  src = pkgs.fetchurl {
    url = target.url;
    hash = target.hash;
  };
in
pkgs.stdenv.mkDerivation {
  pname = "${pname}-bin";
  version = binaries.version;

  inherit src;

  dontUnpack = true;

  installPhase = ''
    mkdir -p $out/bin
    cp $src $out/bin/${pname}
    chmod +x $out/bin/${pname}
  '';

  meta = with pkgs.lib; {
    description = "Pre-compiled ${pname} binary";
    homepage = "https://github.com/shaogme/nixcache-oci";
    license = licenses.mit;
    platforms = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
  };
}
