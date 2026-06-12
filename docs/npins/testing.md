# npins 项目测试指南

本指南介绍了如何为使用 `npins` 管理依赖的项目编写和运行自动化测试。项目采用了多层次的测试策略，确保配置在评估期、构建期和运行期都是正确的。

## 1. 测试层次

项目测试通常分为以下三个层次：

### 1.1 静态检查 (Static/Evaluation Check)
**目的**：验证 Nix 表达式是否能正确评估，以及模块的选项是否按预期应用到了 `config` 中。
**特点**：速度极快，不涉及实际编译，仅检查配置逻辑。
**实现方式**：
- 使用 `lib.evalConfig` 评估系统。
- 在 `runCommand` 中使用 `builtins.trace` 或 shell 断言检查 `config` 属性。

**代码示例** (`tests/static.nix`):
```nix
let
  eval = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    modules = [ ./my-module.nix { exts.feature.enable = true; } ];
    inherit pkgs;
  };
in
pkgs.runCommand "static-check" {} ''
  if [[ "${eval.config.some.option}" == "expected" ]]; then
    touch $out
  else
    exit 1
  fi
''
```

### 1.2 构建测试 (Build Test)
**目的**：确保所有的依赖包都能正确下载，且系统组件（如 `toplevel`）能够成功构建。
**特点**：耗时较长，会触发实际的包下载和编译。
**实现方式**：直接引用 `testSystem.config.system.build.toplevel`。

### 1.3 虚拟机测试 (VM Test / vmtest)
**目的**：在真实的虚拟机环境中运行配置，验证内核启动、驱动加载、服务运行状态等。
**特点**：最全面的检查，能捕捉到运行时错误（如内核 Panic、服务启动失败）。
**实现方式**：使用 `pkgs.testers.nixosTest`。

---

## 2. 如何实现 VM Test

VM Test 是基于 NixOS 测试框架实现的。它会启动一个或多个 QEMU 虚拟机并运行 Python 脚本进行自动化交互。

### 核心组件
1. **`nodes`**: 定义虚拟机的配置。你可以像写 `configuration.nix` 一样定义它。
2. **`testScript`**: 一个 Python 脚本，用于控制虚拟机并检查状态。

### 示例代码 (`tests/vmtest.nix`)
```nix
pkgs.testers.nixosTest {
  name = "my-feature-test";
  
  # 1. 定义测试节点
  nodes.machine = { config, pkgs, ... }: {
    imports = [ ../default.nix ];
    exts.my-feature.enable = true;
  };

  # 2. 编写测试脚本 (Python)
  testScript = ''
    machine.wait_for_unit("multi-user.target")
    # 检查内核模块是否加载
    machine.succeed("lsmod | grep my_module")
    # 检查命令输出
    output = machine.succeed("my-command --version")
    assert "1.0.0" in output
  '';
}
```

---

## 3. 如何运行测试

测试任务通常在各模块的 `tests/default.nix` 中聚合。

### 运行特定模块的所有测试
在对应的测试目录下运行：
```bash
nix-build
```
或者在项目根目录下，通过指定属性路径（如果 `default.nix` 已暴露）：
```bash
nix-build -A kernel.cachyos.tests
```

### 交互式调试 VM Test
如果你想进入虚拟机手动调试，可以使用：
```bash
nix-build tests/vmtest.nix -A driver
./result/bin/nixos-test-driver
# 在交互式 shell 中输入：
# start_all()
# machine.shell()
```

---

## 4. 与 npins 的集成

在测试中引用 `npins` 依赖时，应当遵循与主程序一致的路径：

1. **测试内部引用**：测试脚本应当通过模块的 `default.nix` 进入，从而自动继承该模块下的 `npins` 资源。
2. **环境变量覆盖**：在本地开发测试时，可以配合 `NPINS_OVERRIDE_<NAME>` 环境变量，直接测试本地尚未提交的依赖代码。

例如，在运行 `vmtest` 时测试本地的 `disko` 修改：
```bash
export NPINS_OVERRIDE_disko=/path/to/local/disko
nix-build hardware/disk/btrfs/tests/default.nix
```
