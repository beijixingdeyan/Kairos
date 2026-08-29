# Kairos 路线图（中文）

> 本文档把原任务书的分阶段要求逐条对照到本仓库的实现，并给出后续扩展
> 路线。状态图例：✅ 已完成 · 🚧 进行中 · ⬜ 规划中。

---

## 1. 阶段要求对照表

| 任务书阶段 | 要求 | 对应实现 | 状态 |
| --- | --- | --- | --- |
| Phase 0 | 回答 5 个方向性问题（每题 ≥250 字） | `docs/DESIGN.md` 第 2 节 | ✅ |
| Phase 1 | 提交 3 个备选方案并分析 | `docs/DESIGN.md` 第 3 节 | ✅ |
| Phase 2 | 深度架构 + 实现（bootloader/入口、分配器、调度器、IPC、驱动框架、构建脚本、路线图） | 本章 §2 | ✅ |
| 语言约束 | Rust `no_std` + 自定义分配器，或 C 双告警 + ASan | Rust `no_std` + `linked_list_allocator`/位图，详见 DESIGN §2 | ✅ |
| 交叉编译 | 必须交叉编译到裸机目标 | `x86_64-unknown-none`，`rust-toolchain.toml` 固定 nightly | ✅ |
| QEMU 运行 | 必须在 QEMU 中真实启动 | `cargo run -p os` → SeaBIOS → shell | ✅ |
| 模块化 | 内核按子系统模块化 | `kernel/src/{task,memory,ipc,caps,syscall,user,shell,…}` | ✅ |
| 最小 TCB | 内核只含必要机制 | 文件/网络/驱动均不在内核；unsafe 集合即 TCB 近似 | ✅ |
| 驱动隔离 | 驱动不进内核（结构上隔离） | shell 中 `ipcdemo` 由用户态任务承担；驱动框架见 §3 | 🚧 |
| 能力系统 | 基于能力而非身份 | `kairos-core::caps` + 内核 `caps.rs` 对象注册表 + `spawn` 拒绝演示 | ✅ |
| 单元测试 | 覆盖核心逻辑 | `kairos-core` 41 例 + 内核内自检 | ✅ |
| QEMU 集成测试 | 在 QEMU 里跑集成测试 | `KAIROS_TEST=1 cargo run -p os` → 退出码 | ✅ |
| 模糊测试 | 对核心逻辑做模糊/属性测试 | `fuzz/` proptest（调度/通道/分配器） | ✅ |
| 覆盖率 >70% | 指标 | 核心纯逻辑目标 >70%；CI 中留 `llvm-cov` 输出点 | 🚧 |
| CI | GitHub Actions | `.github/workflows/ci.yml`（host + QEMU） | ✅ |
| 文档 | 中文设计文档 + 阶段说明 | `docs/` 三件套 + README | ✅ |
| 隐私 | 上传 GitHub 无隐私信息 | `os_project_prompt.txt`、`tools/`、`target/` 全部 gitignore | ✅ |

## 2. Phase 2 交付清单

- ✅ 引导链：`bootloader` 0.11 BIOS 镜像 + `entry_point!`
- ✅ 内核入口：`kernel_main` 线性初始化脚本
- ✅ 内存：帧位图（`kairos-core::mem`）+ 页表 + 堆
- ✅ 调度器：RR / WRR / EDF + 固定容量表 + 公平/EDF 测试
- ✅ 上下文切换：中断桩 + `CpuFrame` + `swapgs` 双 GS
- ✅ 系统调用：LSTAR 入口 + 13 个 syscall + 用户指针校验
- ✅ IPC：通道 + 阻塞语义 + 零拷贝帧
- ✅ 能力：CNode + 派生/撤销 + spawn 授权演示
- ✅ 用户程序：5 个 ELF + syscall ABI
- ✅ 构建脚本：`kernel/build.rs`（user 嵌套构建）、`os/build.rs`（镜像）
- ✅ QEMU 集成测试与退出码映射
- ✅ CI / Makefile / 文档 / LICENSE

## 3. 后续路线（按价值排序）

### 3.1 驱动隔离（🚧 → ⬜）

- 设计：把串口/键盘做成“驱动任务”，用户态持有 IRQ 能力（`Irq` 对象已在
  `kairos-core::caps::ObjectKind` 中预留），中断 → 通知端点 → 驱动任务
  响应——教科书式的微内核驱动模型。
- 现状：内核内驱动仍以静态模块存在（`serial.rs`、`vga.rs`），为教学保留
  简洁可直接读；隔离版留给扩展。

### 3.2 文件系统（⬜）

- 在用户态实现 FAT 读取器（bootloader 已给 FAT32 基础）；先做内存盘
  （ramdisk）打通“打开/读/关闭”，再做 mmap 版共享帧映射。
- 验证：`os` 镜像内附带 RAMDisk（bootloader 支持 ramdisk 参数）。

### 3.3 网络栈（⬜）

- 用户态 e1000 驱动 + 最小 UDP，QEMU `-netdev user` 与宿主机互通。

### 3.4 调度器进阶（🚧）

- `KAIROS_SCHED_POLICY=edf` + `deadline` 程序已可演示周期任务与 miss 统计；
- 下一步：MPSC 优先级继承（经典优先级反转演示）、多核（见 §3.6）。

### 3.5 过程内模糊/覆盖率（🚧）

- CI 中运行 `llvm-cov` 统计 `kairos-core` 覆盖率并断言 >70%；
- `fuzz` 增加：能力派生序列、IPC 协议 with 损坏消息、位图碎片化序列。

### 3.6 SMP（⬜）

- 当前为单核（`-smp 1`）。扩展：AP 启动（IPI）、每 CPU 调度器、自旋锁
  语义、中断亲和。教学上建议在单核路径稳定后再做。

### 3.7 调试与文档增强（⬜）

- 内核 `-d int` QEMU 日志模式的教学笔记；
- 每个子系统配一张“一页纸”图（并入 `docs/`）。

## 4. 质量控制基线

合并到 main 的门槛：

1. `cargo build -p os` 零错误零警告；
2. `cargo test -p kairos-core` 全绿；
3. `cargo test -p fuzz --release` 全绿；
4. QEMU 集成测试退出码 0（`KAIROS_TEST=1 cargo run -p os`）。

上述四条已全部由 `.github/workflows/ci.yml` 自动执行。

---

*文档维护：任何架构调整（ABI、地址布局、调度语义）都必须同步更新
DESIGN/ARCHITECTURE 两文与《阶段要求对照表》。*