{ pkgs ? import (import ../../npins).nixpkgs { } }:
let
  lib = pkgs.lib;
  evalModule = config:
    import (pkgs.path + "/nixos/lib/eval-config.nix") {
      inherit pkgs;
      modules = [
        ../module.nix
        config
      ];
    };

  evalDefault = evalModule { };
  evalEnabled = evalModule {
    services.nixcache-proxy = {
      enable = true;
      repo = "test-owner/test-repo";
      port = 38000;
      listenAddress = "0.0.0.0";
      publicKey = "test-key:AAAA=";
      requireSignatures = true;
    };
  };
  evalNoSignatures = evalModule {
    services.nixcache-proxy = {
      enable = true;
      requireSignatures = false;
    };
  };
in
pkgs.runCommand "nixcache-module-static-check" { } ''
  # 1. 验证默认配置处于关闭状态
  [[ "${lib.boolToString evalDefault.config.services.nixcache-proxy.enable}" == "false" ]] || exit 1

  # 2. 验证开启后的端口与环境参数传递
  [[ "${evalEnabled.config.systemd.services.nixcache-proxy.environment.NIXCACHE_REPO}" == "test-owner/test-repo" ]] || exit 1
  [[ "${evalEnabled.config.systemd.services.nixcache-proxy.environment.NIXCACHE_PORT}" == "38000" ]] || exit 1
  [[ "${evalEnabled.config.systemd.services.nixcache-proxy.environment.NIXCACHE_LISTEN}" == "0.0.0.0" ]] || exit 1
  [[ "${evalEnabled.config.systemd.services.nixcache-proxy.environment.NIXCACHE_INDEX_DIR}" == "/var/cache/nixcache-proxy" ]] || exit 1
  [[ "${builtins.head evalEnabled.config.nix.settings.extra-trusted-public-keys}" == "test-key:AAAA=" ]] || exit 1
  [[ "${builtins.head evalEnabled.config.nix.settings.extra-substituters}" == "http://localhost:38000" ]] || exit 1

  # 3. 验证关闭签名验证配置
  [[ "${lib.boolToString evalNoSignatures.config.nix.settings.require-sigs}" == "false" ]] || exit 1

  touch $out
''
