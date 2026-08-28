{ pkgs ? import <nixpkgs> {}, pname ? "nixcache-proxy" }:

let
  binaries = builtins.fromJSON (builtins.readFile ./binaries.json);
  system = pkgs.stdenv.hostPlatform.system;
  
  hasPname = builtins.hasAttr pname binaries;
  pnameBinaries = if hasPname then binaries.${pname} else throw "Unsupported binary package: ${pname}";

  hasBinary = builtins.hasAttr system pnameBinaries;
  target = if hasBinary then pnameBinaries.${system} else throw "Unsupported system for pre-compiled binary ${pname}: ${system}";

  src = pkgs.fetchurl {
    url = target.url;
    hash = target.hash;
  };

  proxyTarget = if builtins.hasAttr "nixcache-proxy" binaries && builtins.hasAttr system binaries."nixcache-proxy"
    then binaries."nixcache-proxy".${system}
    else null;
  proxySrc = if proxyTarget != null then pkgs.fetchurl {
    url = proxyTarget.url;
    hash = proxyTarget.hash;
  } else null;
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
    ${if pname == "nixcache-builder" && proxySrc != null then ''
      cp ${proxySrc} $out/bin/nixcache-proxy
      chmod +x $out/bin/nixcache-proxy
    '' else ""}
  '';

  meta = with pkgs.lib; {
    description = "Pre-compiled ${pname} binary";
    homepage = "https://github.com/shaogme/nixcache-oci";
    license = licenses.mit;
    platforms = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
    mainProgram = pname;
  };
}
