# Kairos 架构文档（中文）

> 本文档给出一张“可执行的系统地图”：模块、目录、控制流与地址空间。
> 阅读顺序：boot 流程 → 中断/系统调用数据流 → 调度与 IPC → 目录树。

---

## 1. 系统总览

```
┌────────────────────────────────────────────────────────────┐
│  用户态 (ring 3)                                            │
│  user: hello / echo_server / echo_client / counter /       │
│         deadline（静态 ELF，kairos syscall ABI）             │
└─────────────────────────────┬──────────────────────────────┘
                              │ syscall 指令 (LSTAR 入口)
                              ▼
┌────────────────────────────────────────────────────────────┐
│  内核 (ring 0)  — 最小 TCB                                 │
│                                                            │
│  caps    能力注册表（对象: Channel/Frame/Task/SpawnAuthority）│
│  ipc     通道内核 + 等待队列 + 零拷贝帧                      │
│  task    任务表 + 切换 + 阻塞/唤醒/退出                      │
│  sched   调度器（RR/WRR/EDF，kairos-core 纯逻辑）            │
│  memory  帧位图 + 页表 + 堆                                 │
│  syscall 分发器（13 个 syscall，用户指针校验）               │
│  shell   内核任务：串口行编辑 + 演示命令                     │
└────────────────────────────────────────────────────────────┘
```

## 2. 目录树

```
Cargo.toml / rust-toolchain.toml / .cargo/config.toml
Makefile / LICENSE / README.md
.github/workflows/ci.yml
docs/{DESIGN,ARCHITECTURE,ROADMAP}.md
os/          运行器（QEMU + 退出码）
kernel/      内核
  build.rs   嵌套构建 user 二进制 + include_bytes 注入
  src/main.rs
  src/{gdt,interrupts,syscall,task,memory,ipc,caps,user,shell,serial,vga,logger}.rs
kairos-core/ 无硬件依赖核心
  build.rs   从 KAIROS_* 环境变量生成编译期配置字面量
  src/{sched,caps,ipc,mem,config}.rs
user/        用户程序 + ABI
  src/lib.rs  kairos syscall ABI
  src/bin/*   5 个程序
fuzz/        proptest 属性测试（tests/{sched,ipc,mem}.rs）
```

## 3. 关键控制流

### 3.1 Boot

```
kernel_main(BootInfo)
 ├─ serial::init          串口最早可用
 ├─ memory::init          偏移映射 + 帧位图 + 堆 + VGA 映射
 ├─ logger/vga::init      日志与屏幕
 ├─ gdt::init / interrupts::init_idt / init_pic_and_timer
 ├─ task::init()          调度器（编译期策略）
 ├─ caps::init()          对象注册表
 ├─ syscall::init()       5 个 MSR（LSTAR/STAR/SFMASK/GS）
 ├─ 进入首任务（idle）    enter_task_frame
 ├─ (KAIROS_TEST=1) test_runner → exit_kernel(0x10/0x11)
 ├─ boot_tasks()          idle + (REALTIME_DEMO) deadline 双任务
 └─ shell::start()        内核 shell 任务
```

### 3.2 一次时钟抢占

```
PIT IRQ0 (vector 32)
 └─ irq_timer 桩：swapgs?(ring3) → push CpuFrame → kairos_irq_handler
     └─ pic_eoi → task::on_irq_after_eoi(frame)
         └─ SCHED.on_tick() → SchedAction::Preempt(Some(next))
             └─ 保存当前 frame → switch_to(next)
                 ├─ gdt::set_rsp0(next 内核栈)
                 ├─ syscall::gs_area().kstack = next 内核栈顶
                 ├─ set_user_ret_flag(next 是否 ring3)   → GS 区 scratch1
                 └─ 返回 next 的 save_area
 └─ 桩恢复：mov rsp,rax → pop×15 → add rsp,16 → swapgs?(flag) → iretq
```

### 3.3 一次 syscall（以 `channel send` 为例）

```
用户 send(slot, msg)
 └─ syscall 指令 → kairos_syscall_entry（LSTAR）
     ├─ swapgs；spill 15 GPR 到用户栈
     ├─ 内核栈组装 CpuFrame（vec=256，ripe=用户 rcx …）
     └─ kairos_syscall_dispatch(SYS_SEND, frame)
         ├─ slot = arg-1；用户指针 read_message 校验后拷贝
         ├─ ipc::send：能力解析（CALL 权限）→ 入队或挂起
         ├─ 挂起 → syscall_park（block + dispatch + switch_to）
         └─ 完成 → Out::Value(v)，写回 frame.rax
 └─ 恢复 frame → swapgs?(flag) → iretq
```

### 3.4 IPC 唤醒路径

```
send 任务：channel 满 → send_waiters.push(self) → block → 他人运行
recv 任务：pop 一条 → send_waiters.pop() → wake_parked(w)
          → w 以 Ready 重新入队（in_ring 防重）
```

## 4. 地址空间（单页表）

```
0x10_0000_0000  USER_BASE（hello；每程序 +16 MiB：echo_server=0x10_0100_0000 …）
0x11_0000_0000  USER_STACK_BASE（4 MiB）
0x12_0000_0000  USER_FRAME_WINDOW（128 MiB 共享帧）
0x40_0000_0000_0000  内核堆
0x80_0000_0000_0000  物理内存偏移（bootloader）
```

内核镜像由 bootloader 以 1 TiB 偏移装入（运行时地址 = ELF VMA +
`0x1_0000_0000_00`），与用户程序的 64 GiB 区域互不重叠。用户 ELF 装载分
三步：映射（可写）→ 应用 `.rela.dyn` 的 `R_X86_64_RELATIVE` 槽位 → 对
纯文本段撤销 `WRITABLE`（W^X）；`syscall` 指令依赖 `EFER.SCE`（syscall
初始化时设置）与 GS 内核栈区。

## 5. 编译期配置（kairos-core/build.rs）

| 环境变量 | 默认 | 含义 |
| --- | --- | --- |
| `KAIROS_SCHED_POLICY` | `weighted` | `rr` / `weighted` / `edf` |
| `KAIROS_QUANTUM_MS` | `10` | 调度量子（毫秒，PIT 1kHz） |
| `KAIROS_HEAP_MIB` | `64` | 内核堆大小（MiB） |
| `KAIROS_REALTIME_DEMO` | unset | boot 时自动派生 EDF 周期任务 |
| `KAIROS_TEST` | unset | boot 后运行内建自检并以退出码结束 |
| `KAIROS_LOG_LEVEL` | `info` | `error`/`warn`/`info`/`debug`/`trace` |

配置以**字面量**写入 `$OUT_DIR/kairos_cfg.rs` 再 `include!`，避免了
`const fn` 字符串解析的不稳定问题——修改环境变量、重新 build，行为即变。

## 6. syscall 表（kairos-core/config.rs 单一来源）

| # | 名称 | 参数 | 说明 |
| --- | --- | --- | --- |
| 0 | exit | – | 终止任务 |
| 1 | yield | – | 让出 CPU |
| 2 | sleep | ms | 睡眠（以 tick 计） |
| 3 | print | ptr, len | 串口输出 |
| 4 | getpid | – | 返回任务 id |
| 5 | time | – | 当前 tick |
| 6 | spawn | name, len | 需要 spawn 能力 |
| 7 | ch_create | – | 建通道，返回 slot+1 |
| 8 | ch_close | slot | 撤销通道能力 |
| 9 | send | slot, msg ptr | 发送（满则阻塞） |
| 10 | recv | slot, buf | 接收（空则阻塞） |
| 11 | send_frame | slot, size, tag | 零拷贝帧+能力 |
| 12 | recv_frame | slot, buf | 取帧能力 |

## 7. 运行器与测试映射

```
cargo run -p os
  ├─ 找 QEMU（KAIROS_QEMU 或 tools/qemu/qemu-system-x86_64.exe）
  ├─ -drive format=raw,file=<img> -serial mon:stdio -device isa-debug-exit
  └─ guest 0x10 → 0 ；0x11 → 1 ；其它 → 2
```