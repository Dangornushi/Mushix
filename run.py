import subprocess


shell_script= ""

def which_lib(lib_name):
    run = subprocess.run(["which", lib_name], capture_output=True, text=True)
    return run.stdout.replace('\n', '')


def shell_build(CC, CFLAGS, OBJCOPY):
    # シェルをビルド
    return CC + CFLAGS +" -Wl,-Tuser.ld -Wl,-Map=shell.map -o shell.elf shell.c user.c common.c\n" + OBJCOPY + " --set-section-flags .bss=alloc,contents -O binary shell.elf shell.bin\n" + OBJCOPY + " -Ibinary -Oelf32-littleriscv shell.bin shell.bin.o\n"


def kernel_build(CC, CFLAGS):
    # カーネルをビルド
    return CC + CFLAGS + " -Wl,-Tkernel.ld -Wl,-Map=kernel.map -o kernel.elf kernel.c virtq.c fileSystem.c common.c shell.bin.o"


def qemu_run(QEMU):
    # qemuを実行
    return QEMU + " -machine virt -bios default -nographic -serial mon:stdio --no-reboot -drive id=drive0,file=disk.tar,format=raw -device virtio-blk-device,drive=drive0,bus=virtio-mmio-bus.0 -kernel kernel.elf"


QEMU = which_lib("qemu-system-riscv32")  # qemu-system-riscv32のパス
CC = which_lib("clang")  # clangのパス (Ubuntuの場合は CC=clang)
OBJCOPY = which_lib("llvm-objcopy")  # llvm-objcopyのパス
CFLAGS = "-std=c11 -O2 -g3 -Wall -Wextra --target=riscv32 -ffreestanding -nostdlib"
shell_script = shell_build(CC, CFLAGS, OBJCOPY)
shell_script += kernel_build(CC, CFLAGS)
shell_script += qemu_run(QEMU)
print(shell_script)
