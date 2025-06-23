#![feature(offset_of)]
#![no_main]
#![no_std]

use core::panic::PanicInfo;
use core::time::Duration;
use mushix::error;
use mushix::executor::{sleep, spawn_global, start_global_executor};
use mushix::hpet::global_timestamp;
use mushix::info;
use mushix::init::{init_allocator, init_basic_runtime, init_display, init_hpet, init_pci};
use mushix::print::set_global_vram;
use mushix::println;
use mushix::qemu::exit_qemu;
use mushix::qemu::QemuExitCode;
use mushix::serial::SerialPort;
use mushix::uefi::{init_vram, locate_loaded_image_protocol, EfiHandle, EfiSystemTable};
use mushix::x86::init_exceptions;
use mushix::x86::trigger_debug_interrupt;

#[no_mangle]
fn efi_main(image_handle: EfiHandle, efi_system_table: &EfiSystemTable) {
    println!("[mushix] Starting up...");
    let loaded_image_protocol = locate_loaded_image_protocol(image_handle, efi_system_table)
        .expect("failed to get LoadedImageProtocol");
    let mut vram = init_vram(efi_system_table).expect("failed to init vram");
    init_display(&mut vram);
    set_global_vram(vram);
    let acpi = efi_system_table
        .acpi_table()
        .expect("failed to get ACPI table");
    let memory_map = init_basic_runtime(image_handle, efi_system_table);
    init_allocator(&memory_map);
    let (_gdt, _idt) = init_exceptions();
    init_hpet(acpi);
    init_pci(acpi);
    let serial_task = async {
        let sp = SerialPort::default();
        if let Err(e) = sp.loopback_test() {
            error!("{e:?}");
            return Err("serial loopback test failed");
        }
        info!("started to monitor serial port");
        loop {
            if let Some(v) = sp.try_read() {
                let c = char::from_u32(v as u32);
                info!("serial input: {v:#04X} = {c:?}");
            }
            sleep(Duration::from_millis(20)).await;
        }
    };
    spawn_global(serial_task);
    start_global_executor()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("panic: {info}");
    exit_qemu(QemuExitCode::Fail);
}

/*

    let t0 = global_timestamp();
    let task1 = async move {
        for i in 100..=103 {
            info!("{i} hpet.main_counter = {:?}", global_timestamp() - t0);
            sleep(Duration::from_secs(1)).await;
        }
        Ok(())
    };
    let task2 = async move {
        for i in 200..=203 {
            info!("{i} hpet.main_counter = {:?}", global_timestamp() - t0);
            sleep(Duration::from_secs(2)).await;
        }
        Ok(())
    };
*/
