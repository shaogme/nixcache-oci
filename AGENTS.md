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
