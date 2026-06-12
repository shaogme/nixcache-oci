{
  description = "nixcache-oci: Self-hosted Nix binary cache via GHCR (OCI registry)";

  nixConfig = {
    extra-substituters = [ "http://localhost:37515" ];
    extra-trusted-public-keys = [ ];
  };

  outputs = { self }:
    let
      sources = import ./npins;
      systems = [ "x86_64-linux" "aarch64-linux" ];
      
      lib = import "${sources.nixpkgs}/lib";
      
      forAllSystems = f: lib.genAttrs systems (system: f system);

      defaultForSystem = system:
        let
          pkgs = import sources.nixpkgs { inherit system; };
        in
        import ./default.nix { inherit pkgs; };
    in {
      packages = forAllSystems (system: (defaultForSystem system).packages);

      apps = forAllSystems (system: (defaultForSystem system).apps);

      nixosModules = (import ./default.nix { }).nixosModules;
    };
}
