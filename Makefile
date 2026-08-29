# Kairos — 微内核教学操作系统
#
# 常用目标:
#   make build       编译内核 + 用户程序 + 生成可引导磁盘镜像
#   make run         在 QEMU 中启动（交互式 shell）
#   make test        QEMU 集成测试（KAIROS_TEST=1，内核自检后以退出码报告）
#   make test-host   主机侧单元测试（kairos-core）
#   make fuzz        proptest 确定性回归
#   make clean       清理 target

.PHONY: build run test test-host fuzz clean

build:
	cargo build -p os

run: build
	cargo run -p os

# KAIROS_TEST=1：内核 boot 完成后直接运行内建自检，以 isa-debug-exit 退出码
# 结束；runner 把 0x10 → 0（成功）、0x11 → 1（失败）映射回来。
test: build
	pwsh -NoProfile -Command "$$env:KAIROS_TEST='1'; cargo run -p os"

test-host:
	cargo test -p kairos-core

fuzz:
	cargo test -p fuzz --release -- --test-threads=1

clean:
	cargo clean