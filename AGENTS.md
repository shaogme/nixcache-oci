# Coding Guidelines and Instructions for Agents

**IMPORTANT:** Always use Simplified Chinese (简体中文) when communicating and providing explanations.

## Rust 规范要求

- **目录结构与模块系统**: 
    - **禁止使用 `mod.rs`**: 严格遵循 Rust Edition 2018 推荐的目录结构（即项目应使用 `name.rs` 配合 `name/` 文件夹的形式，而非 `name/mod.rs`）。
- **版本与特性**:
    - 本项目采用 **Rust Edition 2024** 和 **Rust 1.90+**。
    - 在编写代码时，请充分利用新特性并严格遵循 Edition 的相关要求。

## 代码质量要求

- **质量与测试**: 注重代码质量、可测试性和测试覆盖。
- **编码规范**:
    - **禁止长路径**: 禁止在代码中使用全限定命名空间（尤其是以 `crate::` 开头的路径）超过 15 个字符。必须通过 `use` 语句导入后再调用。
    - **合并相同前缀的use语句**: 当有多个`use`语句具有相同前缀时，应合并为一条`use`语句，例如：
    ```rust
    //Bad
    use crate::nix::build;
    use crate::nix::store;
    use crate::nix::path;
    use crate::nix::refpath;
    //Good
    use crate::nix::{
        build,
        store,
        path,
        refpath,
    };
    ```

### 核心规则
- **优先使用 npins**：除非有极特殊理由，否则所有外部 Git 仓库、Nix Channels、PyPi 包或 Tarball 依赖都应通过 `npins` 管理。
- **强制阅读文档**：在任何涉及 `npins` 的操作（添加、更新、维护、代码引入）之前，必须阅读并遵循以下文档：
    - [npins CLI 详细文档](docs/npins/cli.md)：了解如何使用命令行工具管理依赖。
    - [npins 产物使用指南](docs/npins/usage.md)：了解如何在 Nix 代码中正确引用和覆盖依赖。
    - [项目测试指南](docs/npins/testing.md)：了解如何编写和运行静态检查及 VM 测试。

## 开发工作流

### 添加新依赖
1. 确认依赖类型（GitHub, Git, PyPi 等）。
2. 使用 `npins add <type> ...` 命令添加。
3. 检查 `npins/sources.json` 是否已正确更新。
4. 在 Nix 代码中通过 `import <npins-path>` 引入。

### 更新依赖
1. 定期运行 `npins update` 保持依赖项最新。
2. 运行 `npins verify` 确保哈希值正确。

### 调试与覆盖
- 如果需要修改依赖项代码进行调试，请利用 `NPINS_OVERRIDE_<NAME>` 环境变量切换到本地路径，切勿直接修改 `sources.json` 中的哈希值指向不稳定的版本。

## 代码提交流程
- **提交 `sources.json`**：确保 `npins/sources.json` 的更改随代码一同提交。
- **不要提交 `default.nix` 修改**：该文件由工具自动生成，不应手动修改。

## 测试与验证

在提交任何修改（尤其是涉及内核参数、磁盘分区或底层服务时），必须执行相关测试以确保稳定性。

### 强制测试要求
1. **执行静态检查**：通过 `evalConfig` 验证配置项是否正确应用。
2. **运行 VM 测试**：涉及运行时逻辑（如驱动加载、服务状态）时，必须通过 `nixosTest` 验证。
3. **构建验证**：确保 `toplevel` 或相关包能够成功构建，无下载或评估错误。

详细测试方法和实现参考请阅读：[项目测试指南](docs/npins/testing.md)。
