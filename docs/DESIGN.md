# Kairos 设计文档（中文）

> 本文档是项目的“宣言级”设计说明：它记录了选题过程（Phase 0 的五个问题）、
> 备选方案分析（Phase 1 的三个提案）与最终深度设计（Phase 2）。所有阶段
> 要求均在 [ROADMAP.md](ROADMAP.md) 中有一条对应对照，本文只讲“为什么”
> 与“怎么做”。

---

## 1. 项目定位

Kairos 是一枚**微内核教学操作系统**，运行在 x86_64 的 QEMU 虚拟机内，
从 BIOS 加电一路走到交互式 shell，全部代码可读、可断点、可扩展。
它不是 seL4 的复刻，而是把微内核的三根支柱——**小内核**、**能力授权**、
**确定性调度**——以教学可读的方式落到真实硬件上：

1. **小内核（minimal TCB）**：文件、网络、驱动全部不进内核。内核只做四件事：
   内存、调度、IPC、能力。
2. **能力授权**：用户任务之间不共享任何“身份”，只有能力（capability）。
   没有能力就没有访问权；能力只能派生收窄、不能膨胀。
3. **确定性**：调度表是固定数组，调度器不分配内存、不持有锁竞争，中断路径
   全程关中断——相同输入永远产生相同时序（EDF 下尤为可观测）。

教学内核与“玩具内核”的分界线在于**错误路径也认真对待**：越界帧索引返回
错误、能力查找失败被拒绝、用户指针在被解引用前先做整段校验。这些边角
正是真实系统与演示系统的区别。

---

## 2. Phase 0 — 方向抉择（五个问题）

### 问题一：四种方向中为何选择“微内核教学 OS”？

四个候选方向：微内核教学 OS、Unikernel、RTOS、安全隔离 OS。它们并非互斥：
Unikernel 强调“单地址空间、单应用”，RTOS 强调“硬实时、极小延迟”，安全隔离
OS 强调“隔离与形式化”。**微内核教学 OS 在四者中拥有最均匀的教学价值**：

- 它强迫作者处理真实的分界问题：用户/内核态切换、页表隔离、IPC 的性能成本、
  能力与权限模型——这些是 Unikernel 有意回避、RTOS 常常省略、安全 OS 用
  形式化工具掩盖的问题，恰好是操作系统课程的核心难点。
- 其余三个方向的“加分项”都可以作为微内核的扩展点：调度器可以做成 EDF
  （实时）、驱动可以做成隔离进程（安全）、单一应用可以直接作为引导任务
  （Unikernel）。也就是说，微内核是所有候选方向中**信息量最大、向上兼容**
  的底座。

组织层面，教学 OS 的验收标准最清晰：能启动、能跑用户程序、能展示调度与
IPC、有文档有测试。项目的完成度可以像一台“可工作的机器”一样被检验，
而不是靠论文式论证。因此选择微内核。

### 问题二：Rust 与 C 的选择，以及 `no_std` 意味着什么？

候选双轨：Rust（`no_std`、自定义分配器、无标准库）或 C（`-Wall -Wextra
-Werror -fsanitize=address`）。选择 Rust，理由有三层：

1. **内存安全作为默认不变量**。内核中 90% 的代码可以写成 safe Rust：页表
   的不可变借用、通道的容量边界、能力槽的 `Option` 类型——静态期就排除
   了空指针解引用、越界写、释放后使用。unsafe 被压缩在少量、明确注释的
   位置（中断桩、裸指针转换、MSR 写入），并且每个 unsafe 块都有理由注释。
   这正是微内核“最小 TCB”理念在实现层的投影：**unsafe 集合就是 TCB 的
   近似**。
2. **`no_std` 是真正的裸机约束**。没有 `std` 意味着没有堆、没有线程、
   没有 `Vec` 之外的魔法——我们以 `extern crate alloc` 白名单引入需要的
   部分（`Vec`、`String`、`format!`），全局分配器是我们自己写的位图/页表
   组合。学生能清楚看到“语言运行时”与“内核内存管理”的交界。
3. **工具链即文档**。Rust 的 trait 与类型把系统接口立成契约：`FrameAllocator`
   是 trait、调度器是 trait 方法集、syscall 编号只有一处定义（`kairos-core`
   的 `config.rs`），内核与用户态共享同一份 ABI 常量——C 下要靠头文件纪律
   维持的一致，Rust 下由编译器直接保证。

### 问题三：如何保证“能在 QEMU 中运行”是可持续验证的，而不是一次奇迹？

验收的关键是把“能跑”变成 CI 中的一个退出码。方案是三层验证叠加：

1. **主机侧**：与硬件无关的核心（调度、能力、IPC 模型、位图分配器）全部
   编写为可在主机直接 `cargo test` 的纯逻辑，41 个单元测试 + proptest 属性
   测试在容错、公平性、不变量上做地毯式检查。
2. **内核内自检**：`KAIROS_TEST=1` 时内核在 boot 完成后运行 `test_runner`，
   逐子系统自检（logger/memory/task/syscall/user/ipc/shell），任何失败都会
   让 panic 处理器向 QEMU 的 `isa-debug-exit` 设备写 `0x11`。
3. **运行器退出码映射**：host 侧 `os` 运行器把 guest 的 `0x10`（成功）/
   `0x11`（失败）映射为进程退出码 0/1，于是 `cargo run -p os` 在 CI 里就是
   一个布尔断言。

这使“在 QEMU 里跑通”从一次性人工操作变成了每次 push 的回归门槛。

### 问题四：教学内核最容易被“糊弄”的地方在哪里，本项目如何防备？

教学内核的常见糊弄点按杀伤力排序：

- **调度器只跑顺风车**：没有时钟抢占，全靠任务自觉 yield——这等于没有
  调度器。Kairos 用 PIT @1kHz 产生真实时钟中断，中断桩直接完成任务切换，
  `sched.rs` 的单元测试专门断言“任务不主动让出也必须被抢占”。
- **用户态是假的**：要么用户代码还在 ring0 跑，要么所有“用户程序”是内核
  函数的别名。Kairos 的用户程序是真实 `x86_64-unknown-none` 编译链接的
  ELF，CPU 以 ring3 + `USER` 页标志执行，任何越权访存都会触发页错误。
- **IPC 是全局变量**：所谓 IPC 只是直接调函数。Kairos 的 IPC 走内核通道，
  两端带能力校验，阻塞语义由调度器管理，发送方与接收方在等待队列中
  正确地相互唤醒。
- **内存只有“够用就行”**：没有正式的物理帧记账。Kairos 用位图分配器 +
  内存映射表驱动，从引导内存映射开始就遵循“先全保留、再精确释放可用区”
  的保守策略，并且有 `check_invariants()` 随时自证。

每一处防备都有对应的测试或运行时断言，防止回归。

### 问题五：项目如何平衡“完整度”与“可维护性”？

约束是：完整度要高、所用功能必须真实可用（用户明确定下的验收标准）。
我们的平衡策略是**分层完整性**：

- 第一层（必须真实可用）：引导、内存、调度、任务切换、syscall、通道 IPC、
  shell、用户程序装载——这是“开机到 shell”的主干，每个环节都在 QEMU
  里被实际执行。
- 第二层（必须能演示）：能力授权的拒绝路径（用户任务没有 spawn 能力时
  被拒绝）、EDF 周期任务的 deadline 统计、零拷贝共享帧——这些是微内核的
  “卖点”，做成可交互的 demo 命令（`spawn`、`ipcdemo`、`ps`）。
- 第三层（结构上预留、不装腔作势）：驱动框架、文件系统、网络——文档中
  说明设计意图并保留接口钩子，但不在代码里放“半成品”，以免误导。

可维护性的手段：模块边界与硬件无关核心剥离（`kairos-core` 可主机测试）、
每个 unsafe 块带理由注释、CI 全绿才合并、Cargo.lock 入库保证重现性。

---

## 3. Phase 1 — 三个备选提案的比较

### 提案 A：微内核教学 OS（Kairos）——**选定**

- 内核组件：内存（帧分配 + 页表）、调度（RR/WRR/EDF）、IPC（通道 + 零拷贝
  帧）、能力（CNode）。
- 用户态：内置 ELF 程序，ring3 执行，syscall 进入内核。
- 优点：覆盖课程全部核心主题；所有机制可被 CLI 演示；测试维度最全。
- 缺点：工程量最大（上下文切换、ELF 装载、异常路径都要求为真）。
- 风险评估：工程量集中在早期（中断/切换/装载），一旦打通主干，后续是
  “加功能”而非“重构”。

### 提案 B：Unikernel

- 单一地址空间，应用与内核同层，`syscall` 退化为函数调用。
- 优点：实现最快，无需处理用户态切换。
- 缺点：绕开了操作系统课程最重要的议题（保护、切换、权限），“教学价值”
  显著低于 A；与“能力系统”题眼几乎不兼容。
- 结论：作为 A 的未来形态（引导一个应用作为唯一任务）保留在路线图中，
  不独立立项。

### 提案 C：RTOS（裸机任务调度 + 队列）

- 优点：周期任务、抢占、优先级——实时概念的天然载体。
- 缺点：通常无内存保护、无用户态，安全属性单薄；“能力系统”无从谈起。
- 结论：以 **EDF 调度器 + 周期任务 demo** 的形式并入 A，作为实时功能的
  子集呈现——既有 RTOS 的教学点，又不牺牲微内核的完整性。

三案共同点：都基于同一套 boot 链与内存模块，因此迁移成本低，选 A 的
机会成本最小。

---

## 4. Phase 2 — 深度设计

### 4.1 引导链

```
SeaBIOS (QEMU)
  └─ MBR → FAT32 镜像（bootloader crate 0.11.17 生成）
       └─ 实模式 → protected mode → long mode（bootloader 完成分页与栈）
            └─ bootloader_api::entry_point! → kernel_main(&mut BootInfo) -> !
```

要点：

- 引导镜像由 `os/build.rs` 调用 `BiosBoot::create_disk_image` 生成，内核
  ELF 通过 **artifact dependency**（`bindeps`）注入构建脚本，路径经
  `CARGO_BIN_FILE_KERNEL_kernel` 获得——同一 workspace 内零手工拷贝。
- `bootloader` 只能开 `bios` feature（UEFI 子构建需要 host 侧未提供的
  `wcslen`，实践中禁用）。
- 内核为 `x86_64-unknown-none`，`#[no_main]`，由 `entry_point!` 宏生成
  入口，`kernel_main` 返回 `!`。

### 4.2 地址空间布局（单页表、共享内核映射）

| 范围 | 内容 |
| --- | --- |
| `0x10_0000_0000`（USER_BASE） | 用户程序映像（text/rodata/data/bss，ELF 装载） |
| `0x10_0100_0000` | `echo_server`（每程序间隔 16 MiB） |
| `0x10_0200_0000` | `echo_client` |
| `0x10_0300_0000` | `counter` |
| `0x10_0400_0000` | `deadline` |
| `0x11_0000_0000`~`0x11_0040_0000` | 用户栈（4 MiB，向下增长） |
| `0x12_0000_0000`~`0x12_0800_0000` | 零拷贝共享帧窗口（128 MiB） |
| `0x40_0000_0000_0000` | 内核堆（2^46，Linked List Allocator） |
| `0x80_0000_0000_0000` | 物理内存偏移映射（2^47，bootloader 提供） |

内核镜像由 bootloader 以 `virtual_address_offset=0x1_0000_0000_00`（1 TiB）
装入，内核符号的运行地址 = ELF VMA + 1 TiB；用户程序则链接在各自独立的
64 GiB 基址上，与内核互不重叠。

用户程序以 `-Ttext=0x10_0000_0000`（每程序 +16 MiB）静态链接，保持“无
动态链接”的朴素模型；但 Rust 代码gen 会在 `.rela.dyn` 中发出少量
`R_X86_64_RELATIVE` GOT 重定位（函数指针/动态符号槽），装载器因此分三
步完成装载：**全部段以可写方式映射并拷贝映像 → 应用 `.rela.dyn` 重定位
（把 addend 写入槽位）→ 对纯文本段撤销 `WRITABLE`（W^X）**。ELF 解析只
使用 `PT_LOAD` 段 + 节表（仅 SHT_RELA）。

### 4.3 内存管理

- **物理帧位图**：`kairos-core::mem::BitmapAllocator`。位图初始全“已用”，
  然后把固件报告的 `Usable` 区域逐段清位——BIOS 区域、MMIO、内核镜像、
  bootloader 数据天然保持保留，不需要脆弱的“手工雕刻”清单。
- **页表**：采用 bootloader 准备的页表（CR3），`OffsetPageTable` 之上提供
  `map_page / map_contiguous / update_flags / is_mapped` 四个原语；用户段
  装载时序为：`PRESENT|USER|WRITABLE` 映射 → 拷贝映像 → 清零 bss → 对纯
  文本段 `update_flags` 撤销 `WRITABLE`（W^X）。
- **内核堆**：`linked_list_allocator`，页直接来自位图分配器。

### 4.4 上下文切换协议（中断路径）

中断/异常信号进入 IDT，每个向量前置手写汇编桩（`exception_stub!` /
`irq_stub!`）：

1. 若来自 ring3，先 `swapgs` 切到内核 GS；
2. 压入 15 个 GPR + `[err][vec]` + CPU 帧，构成 `CpuFrame`（`repr(C)`，
   176 字节结构，偏移 15×8 = vec、16×8 = err）；
3. 调用 Rust 处理器，处理器返回**下一个任务的帧指针**；
4. 桩 `mov rsp, rax; pop×15; add rsp,16; [依据 GS 区标志 swapgs]; iretq`。

异常一律 panic：示例性（教学）行为，避免隐藏错误。

### 4.5 系统调用协议（syscall 路径）

`syscall` 指令入口（LSTAR）是独立的裸汇编函数，协议如下：

- 入口即 `swapgs`，把 15 个 GPR 全部 spill 到**用户栈**上；
- 从 GS 区取内核栈顶，在**内核栈**上组装 `CpuFrame`（`vec=256` 标记
  syscall、`rip=用户 rcx`、`cs=0x23`、`rflags=用户 r11`、`rsp=spill+120`、
  `ss=0x1B`）；
- `call kairos_syscall_dispatch(num, frame)` 分发；
- 返回的 frame 恢复寄存器，依据 GS 区（scratch1）标志决定是否 `swapgs`，
  最后 `iretq` 回用户态。

MSR 设置：`LSTAR=入口`、`STAR=0x08<<32|0x08<<48`（本次用 iretq 返回，
STAR 仅为将来 sysret 预留）、`SFMASK=0x200`（内核段自动清中断）、
`KERNEL_GS_BASE=GS 区`、`GS_BASE=0`。

**用户内存访问纪律**：所有用户指针必须通过 `user_ptr_ok`（范围 + 上限
校验）才能进入 `read_user_bytes / read_user_string / read_message`，杜绝
内核指针直读。这也是教学上的安全红线演示。

### 4.6 调度器

`kairos-core::sched`：固定 `MAX_TASKS=32` 槽位 + 环形就绪队列，零分配、
零锁（内核侧始终在关中断下访问）。

| 策略 | 语义 | 用途 |
| --- | --- | --- |
| RR | 固定时间片轮转 | 默认公平演示 |
| WRR | 票数按 `weight` 分配 | 展示“按需给量” |
| EDF | 最早截止优先 + budget 记账 | 实时演示（`KAIROS_REALTIME_DEMO`） |

EDF 的失败模式也被认真处理：预算耗尽、deadline 错过都会被记录为
`deadline_misses` 统计，`ps` 直接可见。

阻塞语义：`block/finish` 返回新调度结果；`bool in_ring` 防止任务在
“等 IPC 期间留有陈旧就绪队列项”时被重复入队；唤醒时只把不在队列中的
任务入队——该 bug（去重）曾由单元测试 `non_edf_tasks_run_only_when_idle`
捕获并修复。

### 4.7 能力系统

- `CNode`：64 槽固定数组，每槽 `Option<Capability>`（object id + kind +
  rights）。
- 规则：强类型（Channel 能力不能当 Task 用）；派生只收窄；`revoke`
  原子释放；`NULL_SLOT=0xFFFF` 表示空。
- 演示：`SYS_SPAWN` 要求调用者持有指向 `SPAWN_AUTHORITY` 对象、带 `CALL`
  权限的 Task 能力。用户任务默认没有该能力 → `spawn` 被拒绝并打印原因
  ——能力即授权，拒绝即演示。

### 4.8 IPC

- 通道：`ChannelCore`（16 槽环形消息队列）+ 内核侧发送/接收等待队列。
- `Message`：`repr(C)` 72 字节，内核与用户态逐字节复制（无指针泄漏）。
- 零拷贝帧：`send_frame` 分配连续物理页、映射进共享窗口、注册帧对象、
  以 `Message::capability` 传递帧能力；接收方在窗口直接读写数据。
- 阻塞语义：满则发送方入队、空则接收方入队；对端完成操作时触发唤醒
  （`SchedAction` 驱动）。`blocking_yields_cpu` 等测试保证了“阻塞必然
  让出 CPU”。

### 4.9 用户程序与 ABI

`user` crate 提供 `kairos` syscall ABI（裸 `syscall` 指令封装），五个
内置程序全部 `no_std`、`no_main`、`#[unsafe(no_mangle)] extern "C" fn
_start(arg)`：

- `hello`：打印 pid 后退出；
- `echo_server` / `echo_client`：IPC 回显演示（客户端每 1000 ms 发 ping）；
- `counter`：每 500 ms 打印计数；
- `deadline`：按 pid 奇偶取 period/budget，展示 EDF 周期行为。

`kernel/build.rs` 嵌套构建 user 目标，按程序设置各自的
`-Ttext`（`0x10_0000_0000` 起、每程序 +16 MiB）；嵌套 cargo 必须**同时**
设置 `RUSTFLAGS` 与换行编码的 `CARGO_ENCODED_RUSTFLAGS`
（如 `-Clink-arg=-Ttext=0x10_0000_0000\n`）——现代 cargo 在父 cargo 导出
编码变量时忽略裸 `RUSTFLAGS`，漏设会导致 `-Ttext` 静默失效、改用原始
VMA 链接。二进制路径以绝对路径注入 `include_bytes!`。

### 4.10 测试体系（三层）

1. 主机单元测试：`cargo test -p kairos-core`（42 例）。
2. 属性测试：`cargo test -p fuzz --release`（proptest：调度器 vs 步进模型、
   通道 vs `VecDeque`、分配器 vs 不相交集合；固定种子确定性复现）。
3. QEMU 集成：`KAIROS_TEST=1 cargo run -p os`（guest 退出码 0x10/0x11）。

---