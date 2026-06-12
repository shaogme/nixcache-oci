# nixcache-worker

`nixcache-worker` 是 `nixcache-proxy` 的无服务器（Serverless）版本，专门设计运行在 Cloudflare Workers (WebAssembly) 上。

通过部署此 Worker，你无需在本地主机上常驻运行 `nixcache-proxy` 守护进程或 Systemd 服务，而是将请求直接交给 Cloudflare 全球边缘网络节点进行低延迟代理与分发。

## 特性

- **Serverless 架构**：零服务器维护成本，按需冷启动，由 Cloudflare 边缘节点代理请求。
- **高效多级缓存**：
  - **L1 内存缓存**：在单个 Worker 实例（Isolate）的生命周期内，在内存中缓存解密后的索引清单。
  - **L2 KV 缓存**：使用 Cloudflare KV 全球同步持久化 `cache-index.json` 索引，跨区域、跨节点共享，避免高频请求 GHCR 导致触发 GitHub API 频率限制。
- **直通流式代理**：透明地从 GHCR 或配置的上游缓存（如 `cache.nixos.org`）流式分发大体积的 NAR 文件，Worker 自身不缓存大文件，最大程度节省带宽与内存。
- **支持私有仓库**：通过配置 `GITHUB_TOKEN` 密钥，可无缝读取私有 GHCR 中的 Nix 缓存包。

---

## 部署与配置步骤

### 1. 准备工作
请确保你已安装 `npm` 并配置好 Cloudflare Wrangler 命令行工具：
```bash
npm install -g wrangler
wrangler login
```

### 2. 创建 Cloudflare KV 命名空间
运行以下命令在你的 Cloudflare 账户中创建一个用于存放索引缓存的 KV 命名空间：
```bash
wrangler kv namespace create NIXCACHE_KV
```
运行后，终端会输出类似于下面的配置：
```toml
[[kv_namespaces]]
binding = "NIXCACHE_KV"
id = "1bf99e8de9b544aba41d6c85e0eeee95"
```
将该输出中的 `id` 复制并替换到 `wrangler.toml` 文件中相应的 `id` 占位符上。

### 3. 配置环境变量 (`wrangler.toml`)
打开 [wrangler.toml](file:///d:/Documents/GitHub/nixcache-oci/crates/nixcache-worker/wrangler.toml) 并根据您的需求修改环境变量：
- `NIXCACHE_REGISTRY` (选填): OCI 托管源，默认 `ghcr.io`。
- `NIXCACHE_UPSTREAM` (选填): 多个上游缓存源（如 `https://cache.nixos.org`），以空格或逗号分隔。
- `NIXCACHE_INDEX_TTL` (选填): 索引在 KV 和内存中的最大缓存时间（默认 300 秒）。

> [!IMPORTANT]
> `NIXCACHE_REPO` 为必填项。在 `wrangler.toml` 中其默认配置为占位符 `"YOUR_GITHUB_USERNAME_OR_ORG/YOUR_REPO_NAME"`，你必须修改该配置。若保持默认占位符不改动，代理服务运行时将直接报错拦截。

### 4. 设置 GitHub 授权密钥（Secret）
如果你的缓存存储在私有仓库中，或者需要避免公开 API 频率限制，需要将 `GITHUB_TOKEN` 设置为 Worker 的加密密钥（Secret）：
```bash
wrangler secret put GITHUB_TOKEN
```
根据提示输入你的 GitHub 个人访问令牌（Personal Access Token，至少需要对 package/repository 的 read 权限）。

### 5. 部署到 Cloudflare
在 `crates/nixcache-worker` 目录下运行部署命令：
```bash
wrangler deploy
```
Wrangler 将自动调用 `worker-build` 编译 Rust 项目为 WASM，并将其上传发布至 Cloudflare 边缘。

---

## 客户端配置

Worker 部署成功后，你将获得一个类似于 `https://nixcache-worker.<your-subdomain>.workers.dev` 的 URL。

你可以将该 URL 作为替代器（substituter）填入你的 Nix 客户端配置中。

### NixOS 系统配置示例

```nix
nix = {
  settings = {
    substituters = [
      "https://nixcache-worker.<your-subdomain>.workers.dev"
      "https://cache.nixos.org"
    ];
    trusted-public-keys = [
      "my-cache-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    ];
  };
};
```

---

## 集成测试 (E2E Integration Test)

本项目支持在 GitHub Actions 中自动测试已部署的 Worker 状态。

若要开启 Worker 的在线测试，请在您的 GitHub 仓库的 **Settings > Secrets and variables > Actions** 中，在 **Repository secrets** 下新建以下 Secret：
- `TEST_WORKER_URL`：您已部署的 Worker 访问地址（例如：`https://nixcache-worker.example.workers.dev`）。

当此变量存在时，E2E 测试工作流（`test/test-e2e.sh`）将会在每次构建和 CI 运行中，自动对该 Worker 的 `/nix-cache-info` 以及 `/_status` 等接口的连通性进行验证，以确保其保持正常运行。
