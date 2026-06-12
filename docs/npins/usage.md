# npins 产物使用指南

本文件详细介绍了如何在 Nix 项目中使用 `npins` 生成的产物（通常位于 `npins/` 目录下）。

## 1. 导入 npins

在 Nix 表达式中，你可以通过 `import` 包含 `default.nix` 的 `npins` 目录来加载所有的依赖项。

```nix
let
  sources = import ./npins;
in
{
  # 现在可以通过 sources.<name> 访问依赖
}
```

## 2. 访问依赖项 (Pin)

`npins` 生成的每个依赖项（Pin）都可以直接作为**路径字符串**使用。这是因为它实现了 `outPath` 属性。

### 示例：导入 NixOS 模块
如果你的依赖项（如 `disko`）是一个包含 NixOS 模块的仓库，你可以这样导入它：

```nix
{
  imports = [
    "${sources.disko}/module.nix"
  ];
}
```

### 示例：作为包 (Package) 使用
你也可以直接引用其路径，例如在 `environment.systemPackages` 中：

```nix
environment.systemPackages = [
  sources.some-custom-tool
];
```

## 3. 高级用法：函数式调用

`npins` 的每个依赖项实际上是一个**函子 (Functor)**。你可以通过传递 `pkgs` 参数来显式指定使用的 fetcher 实现。

```nix
let
  sources = import ./npins { inherit pkgs; };
in
# 此时所有的 fetcher 都会使用传入的 pkgs 中的版本
```

或者针对单个依赖：

```nix
let
  sources = import ./npins;
  mySource = sources.disko { inherit pkgs; };
in
# ...
```

> [!NOTE]
> 对于大多数情况，直接使用 `sources.name` 即可，`npins` 会自动处理下载和路径计算。

## 4. 开发调试：覆盖依赖 (Override)

在开发过程中，你可能希望使用本地的源码路径来替代 `npins` 自动下载的版本。`npins` 支持通过环境变量进行覆盖。

**环境变量格式**：`NPINS_OVERRIDE_<NAME>`

### 示例
如果你想将 `disko` 覆盖为本地路径 `/home/user/src/disko`：

```bash
export NPINS_OVERRIDE_disko=/home/user/src/disko
nixos-rebuild switch ...
```

`npins` 在运行时会检测到该变量，并打印类似如下的提示：
`trace: Overriding path of "disko" with "/home/user/src/disko" due to set "NPINS_OVERRIDE_disko"`

## 5. 项目中的实际案例

在 `hardware/disk/btrfs/default.nix` 中：

```nix
{ pkgs, ... }:
let
  # 加载 npins 依赖
  sources = import ../npins;
in
{
  nixosModule = { lib, config, pkgs, ... }: {
    # 使用 disko 依赖中的模块
    imports = [ "${sources.disko}/module.nix" ];

    # ... 其他配置
    config = lib.mkIf config.exts.hardware.disk.btrfs.enable {
      # 使用 disko 提供的配置项
      disko.devices.disk.main = {
        # ...
      };
    };
  };
}
```

## 6. 维护建议

- **提交 `sources.json`**：务必将 `npins/sources.json` 提交到 Git 仓库，它记录了确切的版本和哈希。
- **定期更新**：使用 `npins update` 保持依赖项处于最新状态。
- **不要手动修改 `default.nix`**：该文件由 `npins` 自动生成，手动修改会在下次执行 `npins init` 或升级时被覆盖。

## 7. npins 与 Flake 的语法差异

`npins` 和 `flake.nix` 在处理依赖项时的核心差异在于管理方式和引用深度：

| 特性 | Flake (`flake.nix`) | npins (`sources.json`) |
| :--- | :--- | :--- |
| **定义位置** | `inputs` 属性集 | `npins/sources.json` 文件 |
| **引用方式** | `inputs.<name>` | `sources.<name>` |
| **锁定机制** | `flake.lock` (由 Nix 自动生成和管理) | `sources.json` (由 npins 工具显式更新) |
| **环境要求** | 需要开启 Flake 实验性特性 | 兼容标准 Nix (Stable)，无需特殊配置 |
| **类型解析** | 通常返回一个复杂的属性集 (包含 `outputs`, `rev` 等) | 返回一个包含 `outPath` 的函子 (可直接作为路径字符串) |

## 8. 从 Flake 语法迁移到 npins

如果你正在将依赖管理从 Flake 迁移到 npins，主要的工作是将声明式的 `inputs` 映射到 `sources.json` 中的结构。

### 声明对比

**Flake (`flake.nix`):**
```nix
inputs.my-repo.url = "github:user/repo/revision";
```

**npins (`sources.json` 内部等效逻辑):**
```json
"my-repo": {
  "type": "Git",
  "repository": {
    "type": "GitHub",
    "owner": "user",
    "repo": "repo"
  },
  "revision": "revision",
  "hash": "sha256-..."
}
```

> [!TIP]
> 禁止手动创建或修改 `npins/sources.json`， 必须使用npins工具来管理。

### 引用迁移

在代码中使用时，通常按照以下方式替换：

- **Flake 风格**: `imports = [ inputs.disko.nixosModules.disko ];`
- **npins 风格**: `imports = [ "${sources.disko}/module.nix" ];` （直接通过文件路径导入）

> [!TIP]
> `npins` 的优势在于它不需要在顶层 `flake.nix` 中声明所有依赖，每个子目录都可以有自己的 `npins/` 目录，实现依赖的局部化管理。

## 9. 使用 Flake 桥接 npins

### 在 Flake 的 outputs 中暴露 npins 模块

当你的顶层是 `flake.nix`，但底层实现通过 `npins` 管理依赖时，你可以在 `outputs` 中导入子目录的 `default.nix`，并将提取出的模块或 Overlay 暴露出去。

**实际实现 (`flake.nix`):**

```nix
{
  outputs = { self }:
    let
      # 导入包含 npins 逻辑的入口，并提取 nixosModules
      # 这里的 pkgs = { } 仅用于获取静态的模块定义
      exts = import ./default.nix { pkgs = { }; };
    in
    {
      inherit (exts) nixosModules;

      # 暴露一个库函数，允许外部用户显式注入特定的 pkgs
      lib = {
        withPkgs = pkgs: import ./default.nix { inherit pkgs; };
      };
    };
}
```

通过这种方式，外部 Flake 用户可以直接通过 `inputs.dot-exts.nixosModules.hardware.disk.btrfs` 使用你的功能，而无需感知内部是使用 `npins` 还是其他方式管理依赖。注意 `nixosModules.default` 目前是作为占位符的空模块。
