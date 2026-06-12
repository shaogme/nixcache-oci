{ config, pkgs, lib, ... }:
let cfg = config.services.nixcache-proxy;
in {
  options.services.nixcache-proxy = {
    enable = lib.mkEnableOption "nixcache-proxy OCI substituter bridge";

    package = lib.mkOption {
      type = lib.types.package;
      default = (import ../default.nix { inherit pkgs; }).cache-proxy;
      defaultText = lib.literalExpression "(import ./default.nix { inherit pkgs; }).cache-proxy";
      description = "The nixcache-proxy package to use.";
    };

    repo = lib.mkOption {
      type = lib.types.str;
      default = "shaogme/nixcache-oci";
      description = "GitHub owner/repo hosting the OCI cache.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 37515;
      description = "Port the proxy listens on.";
    };

    listenAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      example = "0.0.0.0";
      description = ''
        Address the proxy binds to.
        Use "127.0.0.1" for local-only access (default).
        Use "0.0.0.0" to serve the cache to other machines on your network.
      '';
    };

    publicKey = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "my-cache-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
      description = ''
        Public key for verifying cache signatures.
        Generate with: nix-store --generate-binary-cache-key my-cache-1 secret.key public.key
        The proxy also exposes the key at http://localhost:PORT/public-key
        Leave empty and set requireSignatures = false to use without signing.
      '';
    };

    requireSignatures = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Whether to require valid signatures from this cache.
        Set to false if you haven't configured a signing key.
        When true, you must also set publicKey.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.nixcache-proxy = {
      description = "Nix binary cache proxy for GHCR";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      environment = {
        NIXCACHE_REPO = cfg.repo;
        NIXCACHE_PORT = toString cfg.port;
        NIXCACHE_LISTEN = cfg.listenAddress;
        # DynamicUser has no writable $HOME, so the proxy's default
        # Path.home()/.cache path resolves to /.cache on a read-only
        # root fs. Every request thread crashes mkdir'ing there. Point
        # at systemd's CacheDirectory = "nixcache-proxy" instead.
        NIXCACHE_INDEX_DIR = "/var/cache/nixcache-proxy";
      };
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/nixcache-proxy";
        Restart = "on-failure";
        DynamicUser = true;
        CacheDirectory = "nixcache-proxy";
        # Belt-and-suspenders: if the proxy ever stalls during
        # shutdown, don't make rebuilds wait 90s for SIGKILL.
        TimeoutStopSec = "10s";
      };
    };
    nix.settings = {
      extra-substituters = [ "http://localhost:${toString cfg.port}" ];
      extra-trusted-substituters = [ "http://localhost:${toString cfg.port}" ];
      extra-trusted-public-keys = lib.mkIf (cfg.publicKey != "") [ cfg.publicKey ];
      require-sigs = lib.mkIf (!cfg.requireSignatures) false;
    };
  };
}
