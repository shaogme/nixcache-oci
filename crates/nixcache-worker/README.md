# nixcache-worker

`nixcache-worker` 是 `nixcache-proxy` 的无服务器（Serverless）版本，专门设计运行在 Cloudflare Workers (WebAssembly) 上。

通过部署此 Worker，你无需在本地主机上常驻运行 `nixcache-proxy` 守护进程或 Systemd 服务，而是将请求直接交给 Cloudflare 全球边缘网络节点进行低延迟代理与分发。

## 特性

- **Serverless 架构**：零服务器维护成本，按需冷启动，由 Cloudflare 边缘节点代理请求。
- **高效多级缓存**：
  - **L1 内存缓存**：在单个 Worker 实例（Isolate）的生命周期内，在内存中缓存解密后的索引清单。
  - **L2 KV 缓存**：使用 Cloudflare KV 全球同步持久化配置的 baseline tag 索引，跨区域、跨节点共享，避免高频请求 GHCR 导致触发 GitHub API 频率限制。
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
id = "your-kv-namespace-id"
```
将该输出中的 `id` 复制并替换到 `wrangler.toml` 文件中相应的 `id` 占位符上。

### 3. 配置环境变量 (`wrangler.toml`)
打开 [wrangler.toml](./wrangler.toml) 并根据您的需求修改环境变量：
- `NIXCACHE_REGISTRY` (选填): OCI 托管源，默认 `ghcr.io`。
- `NIXCACHE_UPSTREAM` (选填): 多个上游缓存源（如 `https://cache.nixos.org`），以空格或逗号分隔。
- `NIXCACHE_INDEX_TTL` (选填): 索引在 KV 和内存中的最大缓存时间（默认 300 秒）。
- `NIXCACHE_BASELINE_TAG` (选填): OCI baseline tag，生产默认 `cache-index`。

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

仓库中的 [`wrangler.toml.example`](./wrangler.toml.example) 是生产 Worker 模板，读取 `cache-index`；[`wrangler.e2e.toml.example`](./wrangler.e2e.toml.example) 是 CI 专用 E2E 模板，使用独立 Worker、独立 KV 和 `cache-index-e2e`。E2E KV 的 ID 只能通过 CI secret 注入。由 Wrangler 生成的本地 `wrangler.toml` 已被 Git 忽略。

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

## 管理与状态端点

Worker 提供了与本地代理一致的管理端点：

| 端点路径 | HTTP 方法 | 说明 |
|---|---|---|
| `/_status` | GET | 查看远端连接状态 (`remote_connected`)、错误诊断 (`remote_error`)、`baseline_tag`、索引条目数、上游列表等 |
| `/_refresh` | POST | 请求当前 Worker 实例刷新 KV/内存索引缓存；不构成跨 edge/isolate 的强一致性屏障 |
| `/public-key` | GET | 获取配置的公钥 |

```bash
# 查看 Worker 运行状态与远程连接情况
curl https://nixcache-worker.<your-subdomain>.workers.dev/_status

# 请求当前实例拉取最新的 baseline tag（仅用于运维观测，不保证其它 edge 立即可见）
curl -X POST https://nixcache-worker.<your-subdomain>.workers.dev/_refresh
```

---

## 集成测试 (E2E Integration Test)

本项目支持在 GitHub Actions 中自动测试专用 E2E Worker。CI 不会把测试数据发布到生产 `cache-index`。

请在 GitHub 仓库的 **Settings > Secrets and variables > Actions** 中配置以下 Repository secrets：

- `TEST_WORKER_URL`：`nixcache-worker-e2e` 的访问地址。
- `CLOUDFLARE_E2E_KV_ID`：专用 E2E KV namespace ID；不能复用生产 KV。

`test/test-worker.sh` 会先断言 `/_status` 返回的 `baseline_tag` 为 `cache-index-e2e`，再显式向该 tag promote。它不调用 `/_refresh` 作为收敛门禁，而是直接通过 Worker 执行真实的 `nix-store --realise`；默认最多尝试 24 次、每次间隔 5 秒，可用 `WORKER_SUBSTITUTION_ATTEMPTS` 和 `WORKER_SUBSTITUTION_DELAY_SECONDS` 覆盖。只有所有 substitution 尝试失败后，脚本才执行一次诊断 curl，并打印 HTTP 状态、edge/digest/self-healing/shard 响应头和 `StorePath` 命中情况。由于 Worker、KV、edge 和 GHCR 是最终一致的，`/_refresh` 或单次 narinfo 200 都不能替代这条真实 substitution 门禁。

固定的 `cache-index-e2e` 由 CI 的部署和测试 job 串行保护；生产 Worker 使用 `cache-index`，应通过仅手动触发的 [`deploy-worker.yml`](../../.github/workflows/deploy-worker.yml) 或在生产模板上手动执行 `wrangler deploy`，不由 PR E2E job 部署。
