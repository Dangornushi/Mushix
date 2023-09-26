#!/bin/bash
set -xue

# qemu-system-riscv32のパス (Ubuntuの場合は CC=clang)
QEMU=/usr/local/bin/qemu-system-riscv32
# clangのパス (Ubuntuの場合は CC=clang)
CC=/usr/local/opt/llvm/bin/clang
# llvm-objcopyのパス
OBJCOPY=/usr/local/opt/llvm/bin/llvm-objcopy

CFLAGS="-std=c11 -O2 -g3 -Wall -Wextra --target=riscv32 -ffreestanding -nostdlib"

# シェルをビルド
$CC $CFLAGS -Wl,-Tuser.ld -Wl,-Map=shell.map -o shell.elf shell.c user.c common.c
$OBJCOPY --set-section-flags .bss=alloc,contents -O binary shell.elf shell.bin
$OBJCOPY -Ibinary -Oelf32-littleriscv shell.bin shell.bin.o


# カーネルをビルド
$CC $CFLAGS -Wl,-Tkernel.ld -Wl,-Map=kernel.map -o kernel.elf \
    kernel.c virtq.c fileSystem.c common.c shell.bin.o

# (cd disk && tar cf ../disk.tar --format=ustar ./*.txt)

$QEMU -machine virt -bios default -nographic -serial mon:stdio --no-reboot \
    -drive id=drive0,file=disk.tar,format=raw \
    -device virtio-blk-device,drive=drive0,bus=virtio-mmio-bus.0 \
    -kernel kernel.elf
