# Kairos — 一枚基于能力系统（capability-based）的微内核教学操作系统

> **Kairos**（καιρός，「恰当时刻」）是一个从零编写、可在 QEMU 中真实启动的
> x86_64 微内核教学项目。它把经典教学内核（分时调度、IPC、页表管理）与
> 实战安全机制（能力模型、最小 TCB、零拷贝共享帧）结合起来，全部代码
> 以 Rust `no_std` 编写、无标准库、无动态链接，交叉编译为裸机目标并做成
> FAT32 可引导镜像。

![status](https://img.shields.io/badge/status-boots%20to%20shell-brightgreen)
![toolchain](https://img.shields.io/badge/rust-nightly--2026--08--27-blue)
![license](https://img.shields.io/badge/license-MIT-green)

---

## 目录

- [特性](#特性)
- [技术栈与工具链](#技术栈与工具链)
- [快速开始](#快速开始)
- [运行效果](#运行效果)
- [仓库结构](#仓库结构)
- [设计文档（中文）](#设计文档中文)
- [测试与 CI](#测试与-ci)
- [许可](#许可)

---

## 特性

| 领域 | 内容 |
| --- | --- |
| **微内核结构** | 内核仅含内存、调度、IPC、能力、驱动框架；文件系统/网络作为用户态任务 |
| **能力系统** | 类 seL4 的 `CNode` + 能力（Capability）：强类型、权限只收窄不放宽（防止 confused-deputy）、`revoke` 原子回收 |
| **确定性调度** | 固定容量调度表（无分配、无锁竞争）：轮转 RR / 加权轮转 WRR / 最早截止优先 EDF，编译期 `KAIROS_SCHED_POLICY` 选择 |
| **抢占与切换** | 中断桩（asm stub）保存 15 个通用寄存器 + CPU 帧；`swapgs` 双 GS 基址；syscall 走 `syscall` 指令 + LSTAR/SFMASK |
| **IPC** | 有容量通道（环形缓冲）+ 阻塞等待队列；`send_frame/recv_frame` 共享帧零拷贝（物理页直传 + 能力传递） |
| **用户态程序** | 5 个内置 ELF（hello / echo 客户端-服务端 / counter / 实时截止任务），链接在 `0x10_0000_0000`，静态页表共享地址空间 |
| **实时演示** | EDF 策略 + 周期任务（period/budget），调度器统计 deadline miss 并在 `ps` 中展示 |
| **引导链** | SeaBIOS → `bootloader` crate 0.11（FAT32 MBR）→ long mode → 内核入口 |
| **测试** | 主机侧单元测试（kairos-core 42 例）+ 属性测试（proptest，调度/通道/分配器对照模型）+ QEMU 集成测试（内核自检 + 退出码）+ CI 覆盖率门禁 >70%（`cargo llvm-cov`） |

## 技术栈与工具链

- **语言**：Rust（`no_std`、`no_main`、自定义全局分配器，无标准库）
- **工具链**：`nightly-2026-08-27`（`rust-toolchain.toml` 固定，含 rust-src / llvm-tools / rustfmt / clippy）
- **交叉目标**：`x86_64-unknown-none`
- **引导**：`bootloader` 0.11（BIOS 路径，禁用 UEFI 子构建）
- **硬件抽象**：`x86_64` 0.15 + 手写中断/系统调用汇编桩
- **运行器**：QEMU（`-serial mon:stdio` + `isa-debug-exit` 退出码通道）
- **依赖全部锁定**：`Cargo.lock` 提交入库

> 注意：项目要求在 Windows 上即可一键构建；`rust-toolchain.toml` 保证了
> 任何平台上使用的都是同一 nightly。

## 快速开始

前置：安装 [Rust](https://rustup.rs) 与 [QEMU](https://www.qemu.org/download/)
（把 `qemu-system-x86_64` 放进 PATH，或用环境变量 `KAIROS_QEMU` 指向它的路径）。

```powershell
# 1. 获取固定 nightly（自动从 rust-toolchain.toml 读取）
rustup toolchain install nightly-2026-08-27
rustup component add rust-src llvm-tools --toolchain nightly-2026-08-27

# 2. 构建全部（内核 + 用户程序 + 引导镜像）
cargo build -p os

# 3. 在 QEMU 中启动，进入交互式 shell
cargo run -p os
```

shell 内可用命令：`help`（命令列表）、`info`（内存/调度信息）、`ps`（任务表）、
`sched`（策略信息）、`spawn hello|counter|deadline`（派生用户任务）、
`ipcdemo`（启动 echo 客户端/服务端 IPC 演示）、`crash`（故意触发页错误）、
`exit`（干净关机）。

```powershell
# 4. QEMU 集成测试：boot 后自动跑内建自检并以退出码结束
$env:KAIROS_TEST = "1"; cargo run -p os
#   退出码 0 = 全部自检通过

# 5. 主机侧单元测试与属性测试
cargo test -p kairos-core
cargo test -p fuzz --release
```

## 运行效果

以默认 WRR 策略启动后，串口输出大致如下：

```
[kairos] Kairos 0.1.0 (microkernel teaching OS)
[kairos] sched: weighted-round-robin, quantum 10 ms
[kairos] memory: 256 MiB total, 246 MiB usable, N frames free (bitmap)
[kairos] boot tasks ready
kairos> 
```

输入 `ipcdemo` 可看到两个用户态任务通过内核通道互发消息，
`ps` 展示每个任务的运行次数、被抢占次数与实时任务的 deadline miss 统计。

## 仓库结构

```
├─ Cargo.toml                工作区根（依赖统一管理）
├─ rust-toolchain.toml       固定 nightly 工具链
├─ .cargo/config.toml        启用 artifact dependencies (bindeps)
├─ Makefile                  常用构建/测试目标
├─ os/                       运行器：构建磁盘镜像 + 启动 QEMU + 退出码映射
├─ kernel/                   内核本体（下为核心模块）
│  ├─ src/main.rs            boot 编排、自检 runner、panic 处理
│  ├─ src/gdt.rs             GDT + TSS（rsp0 切换）
│  ├─ src/interrupts.rs      IDT、PIC/PIT、中断/异常桩、CpuFrame
│  ├─ src/syscall.rs         系统调用入口（LSTAR）+ 分发器 + 用户内存校验
│  ├─ src/task/              任务表、切换、等待/退出/唤醒
│  ├─ src/memory/            页表、物理帧位图、内核堆
│  ├─ src/ipc.rs             通道内核 + 能力解析 + 零拷贝帧
│  ├─ src/caps.rs            内核对象注册表（Channel/Frame/Task/SpawnAuthority）
│  ├─ src/user.rs            ELF 加载器 + 用户地址空间布局
│  └─ src/shell.rs           内核 shell（串口行编辑）
├─ kairos-core/              与硬件无关的核心：调度器、能力、IPC 模型、位图分配器
│  └─ src/{sched,caps,ipc,mem,config}.rs   全部可主机单测
├─ user/                     用户态程序 + kairos syscall ABI（no_std）
│  └─ src/bin/{hello,echo_server,echo_client,counter,deadline}.rs
├─ fuzz/                     proptest 属性测试（调度/通道/分配器对照模型）
└─ docs/                     中文设计文档（DESIGN / ARCHITECTURE / ROADMAP）
```

## 设计文档（中文）

- [docs/DESIGN.md](docs/DESIGN.md) —— 设计哲学、选题分析、系统调用 ABI、
  上下文切换协议、能力系统、内存模型、调度与实时、IPC、各阶段要求对照表
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) —— 模块地图、目录树、
  关键数据流（boot / 中断 / syscall / IPC）、地址空间布局、可运行示例
- [docs/ROADMAP.md](docs/ROADMAP.md) —— 扩展路线：驱动隔离、文件系统、
  网络栈、SMP、模糊测试扩面等

## 测试与 CI

- 单元测试：`kairos-core` 41 例（调度公平性/EDF/阻塞语义、能力权限收窄、
  通道 FIFO 回绕、位图分配器不变量）
- 属性测试：`fuzz/` 以 proptest 对照模型（调度器 vs 步进模型、通道 vs
  `VecDeque`、分配器 vs 不相交集合）验证，随机种子固定，CI 内确定性复现
- QEMU 集成测试：`KAIROS_TEST=1` 时内核在 boot 后运行 `test_runner`
  （logger/memory/task/syscall/user/ipc/shell 自检），失败即向
  `isa-debug-exit` 写 `0x11`，runner 映射为进程退出码
- GitHub Actions（`.github/workflows/ci.yml`）：主机测试 + 属性测试 +
  clippy + QEMU 集成测试 + shell 里程碑冒烟

## 许可

MIT —— 见 [LICENSE](LICENSE)。