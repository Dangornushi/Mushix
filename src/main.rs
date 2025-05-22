#![feature(offset_of)]
#![no_main]
#![no_std]

use core::fmt::Write;
use core::panic::PanicInfo;
use core::writeln;
use mushix::graphics::{draw_test_pattern, fill_rect, Bitmap};
use mushix::qemu::exit_qemu;
use mushix::qemu::QemuExitCode;
use mushix::uefi::{
    exit_from_efi_boot_services, init_vram, EfiHandle, EfiMemoryType, EfiSystemTable,
    MemoryMapHolder, VramTextWriter,
};
use mushix::x86::hlt;

#[no_mangle]
fn efi_main(image_handle: EfiHandle, efi_system_table: &EfiSystemTable) {
    let mut vram = init_vram(efi_system_table).expect("failed to init vram");
    let vw = vram.width();
    let vh = vram.height();
    draw_test_pattern(&mut vram);

    let mut w = VramTextWriter::new(&mut vram);
    for i in 0..4 {
        writeln!(w, "i = {i}").unwrap();
    }

    let mut memory_map = MemoryMapHolder::new();
    let status = efi_system_table
        .boot_services()
        .get_memory_map(&mut memory_map);
    let mut total_memory_pages = 0;
    for e in memory_map.iter() {
        if e.memory_type() != EfiMemoryType::CONVENTIONAL_MEMORY {
            continue;
        }
        total_memory_pages += e.number_of_pages();
    }
    let total_memory_size_mib = total_memory_pages * 4096 / 1024 / 1024;
    writeln!(w, "Total memory size: {total_memory_size_mib} MiB").unwrap();

    exit_from_efi_boot_services(image_handle, efi_system_table, &mut memory_map);

    writeln!(w, "Hello, Non-UEFI world!").unwrap();

    loop {
        hlt();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    exit_qemu(QemuExitCode::Fail);
}
