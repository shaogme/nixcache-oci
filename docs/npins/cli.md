# npins CLI 详细文档

`npins` 是一个用于 Nix 项目的简单依赖管理器，它可以帮助你轻松地管理和更新非 Flake 的依赖项（Pin）。

## 基本用法

```bash
npins [选项] <命令>
```

### 全局选项

- `-d, --directory <FOLDER>`: 指定 `sources.json` 和 `default.nix` 所在的文件夹（环境变量: `NPINS_DIRECTORY`，默认值: `npins`）。
- `--lock-file <LOCK_FILE>`: 指定 `sources.json` 的路径并激活锁定文件模式。在该模式下，不会生成 `default.nix`，且 `--directory` 选项会被忽略。
- `-v, --verbose`: 打印调试信息。
- `-h, --help`: 打印帮助信息。
- `-V, --version`: 打印版本信息。

---

## 命令详解

### 1. `init` - 初始化

初始化 npins 目录。多次运行此命令会还原或升级 `default.nix`，但绝不会触动你的 `sources.json`。

**用法：**
```bash
npins init [选项]
```

**选项：**
- `--bare`: 不添加初始的 `nixpkgs` 条目。

---

### 2. `add` - 添加依赖

向项目中添加一个新的 Pin。

**用法：**
```bash
npins add [选项] <子命令>
```

**通用选项：**
- `--name <NAME>`: 使用自定义名称添加。如果名称已存在，则会覆盖。
- `--frozen`: 以“冻结”状态添加。被冻结的依赖在执行 `npins update` 时默认会被忽略。
- `-n, --dry-run`: 预览更改，但不实际执行。

#### `add` 子命令：

- **`channel`**: 追踪一个 Nix Channel。
  - `npins add channel [选项] <CHANNEL_NAME>`
- **`github`**: 追踪一个 GitHub 仓库。
  - `npins add github [选项] <OWNER> <REPOSITORY>`
  - **选项**:
    - `-b, --branch <BRANCH>`: 追踪指定分支而非发布版本。
    - `--at <tag_or_rev>`: 使用特定的提交或发布版本（标签名或 Git 修订号）。
    - `--pre-releases`: 同时追踪预发布版本（与 `--branch` 冲突）。
    - `--upper-bound <version>`: 限制版本解析上限（例如 "2" 限制在 1.X 版本）。
    - `--release-prefix <PREFIX>`: 仅考虑以该前缀开头的发布标签（例如 "release/"）。
    - `--submodules`: 同时获取子模块。
- **`forgejo`**: 追踪一个 Forgejo 仓库。
  - `npins add forgejo [选项] <SERVER> <OWNER> <REPOSITORY>`
  - 选项与 `github` 类似。
- **`gitlab`**: 追踪一个 GitLab 仓库。
  - `npins add gitlab [选项] <REPO_PATH>...`
  - **选项**:
    - `--server <url>`: 使用自建 GitLab 实例（默认: `https://gitlab.com/`）。
    - `--private-token <token>`: 使用私有令牌访问。
    - 其他选项（`--branch`, `--at`, `--submodules` 等）与 `github` 一致。
- **`git`**: 追踪任意 Git 仓库。
  - `npins add git [选项] <URL>`
  - **选项**:
    - `--forge <FORGE>`: 指定 Forge 类型（none, auto, gitlab, github, forgejo）。
    - 其他选项（`--branch`, `--at`, `--submodules` 等）与 `github` 一致。
- **`pypi`**: 追踪 PyPi 上的包。
  - `npins add pypi [选项] <PACKAGE_NAME>`
  - **选项**:
    - `--at <version>`: 使用特定版本而非最新版。
    - `--upper-bound <version>`: 限制版本解析上限。
- **`container`**: 追踪一个 OCI 容器镜像。
  - `npins add container [选项] <IMAGE_NAME> <IMAGE_TAG>`
- **`tarball`**: 追踪一个 Tarball (压缩包) URL。
  - `npins add tarball [选项] <URL>`

---

### 3. `show` - 显示列表

列出当前所有的 Pin 条目。

**用法：**
```bash
npins show
```

---

### 4. `update` - 更新依赖

将所有或指定的 Pin 更新到最新版本。

**用法：**
```bash
npins update [选项] [NAMES]...
```

**参数：**
- `[NAMES]...`: 仅更新指定的 Pin。

**选项：**
- `-p, --partial`: 不更新版本，仅重新获取哈希值 (Hash)。
- `-f, --full`: 即使版本未变也重新获取哈希值。
- `-n, --dry-run`: 打印差异 (diff)，但不写入更改。
- `--frozen`: 允许更新那些通常被忽略的冻结项。
- `--max-concurrent-downloads <NUM>`: 最大并发下载数（默认 5）。

---

### 5. `verify` - 校验

验证所有或指定的 Pin 哈希值是否正确。这类似于执行 `update --partial --dry-run` 并检查差异是否为空。

**用法：**
```bash
npins verify [选项] [NAMES]...
```

---

### 6. `freeze` / `unfreeze` - 冻结与解冻

- **`freeze`**: 冻结一个或多个 Pin。冻结后的 Pin 在常规更新时会被跳过。
  - `npins freeze <NAMES>...`
- **`unfreeze`**: 解冻一个或多个 Pin。
  - `npins unfreeze <NAMES>...`

---

### 7. `remove` - 移除

移除一个 Pin 条目。

**用法：**
```bash
npins remove <NAME>
```

---

### 8. `get-path` - 获取路径

计算 Pin 在 Nix Store 中的路径，必要时会进行获取。

**用法：**
```bash
npins get-path <NAME>
```

---

### 9. `import-niv` / `import-flake` - 导入

- **`import-niv`**: 尝试从 Niv 导入条目。
  - `npins import-niv [PATH]` (默认路径: `nix/sources.json`)
- **`import-flake`**: 尝试从 `flake.lock` 导入条目。
  - `npins import-flake [PATH]` (默认路径: `flake.lock`)

---

### 10. `upgrade` - 升级格式

将 `sources.json` 和 `default.nix` 升级到最新的格式版本。这可能会偶尔导致 Nix 求值中断，请谨慎使用。

**用法：**
```bash
npins upgrade
```
