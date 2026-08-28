{ pkgs ? import (import ../../npins).nixpkgs { } }:
pkgs.testers.nixosTest {
  name = "nixcache-proxy-service-vmtest";

  nodes.machine = { config, pkgs, ... }: {
    imports = [ ../module.nix ];

    services.nixcache-proxy = {
      enable = true;
      repo = "shaogme/nixcache-oci";
      port = 37515;
      requireSignatures = false;
    };
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    # 1. 等待 nixcache-proxy 服务启动就绪
    machine.wait_for_unit("nixcache-proxy.service")
    # 2. 验证端口监听
    machine.wait_for_open_port(37515)
    # 3. 验证 /nix-cache-info 接口响应
    output = machine.succeed("curl -fs http://127.0.0.1:37515/nix-cache-info")
    assert "StoreDir: /nix/store" in output
    # 4. 验证 DynamicUser 沙箱缓存目录权限
    machine.succeed("ls -la /var/cache/nixcache-proxy")
  '';
}
