extern crate alloc;

use crate::acpi::AcpiRsdpStruct;
use crate::allocator::ALLOCATOR;
use crate::graphics::{draw_test_pattern, fill_rect, Bitmap};
use crate::hpet::set_global_hpet;
use crate::hpet::Hpet;
use crate::info;
use crate::pci::Pci;
use crate::uefi::exit_from_efi_boot_services;
use crate::uefi::EfiHandle;
use crate::uefi::EfiMemoryType;
use crate::uefi::EfiMemoryType::*;
use crate::uefi::EfiSystemTable;
use crate::uefi::MemoryMapHolder;
use crate::uefi::VramBufferInfo;
use crate::x86::write_cr3;
use crate::x86::PageAttr;
use crate::x86::Table;
use crate::x86::PAGE_SIZE;
use crate::x86::PML4;
use alloc::boxed::Box;
use core::cmp::max;

pub fn init_basic_runtime(
    image_handle: EfiHandle,
    efi_system_table: &EfiSystemTable,
) -> MemoryMapHolder {
    let mut memory_map = MemoryMapHolder::new();
    exit_from_efi_boot_services(image_handle, efi_system_table, &mut memory_map);
    ALLOCATOR.init_with_mmap(&memory_map);
    memory_map
}

pub fn init_pading(memory_map: &MemoryMapHolder) {
    let mut table = PML4::new();
    let mut end_of_mem = 0x1_0000_0000u64;
    for e in memory_map.iter() {
        match e.memory_type() {
            CONVENTIONAL_MEMORY | LOADER_CODE | LOADER_DATA => {
                end_of_mem = max(
                    end_of_mem,
                    e.physical_start() + e.number_of_pages() * (PAGE_SIZE as u64),
                );
            }
            _ => (),
        }
    }
    table
        .create_mapping(0, end_of_mem, 0, PageAttr::ReadWriteKernel)
        .expect("failed to create initial page mapping");
    table
        .create_mapping(0, 4096, 0, PageAttr::NotPresent)
        .expect("Failed to unmap page 0");
    unsafe { write_cr3(Box::into_raw(table)) }
}

pub fn get_page_pointer(
    memory_map: &MemoryMapHolder,
) -> *mut Table<4, Table<3, Table<2, Table<1, [u8; 4096]>>>> {
    let mut table = PML4::new();
    let mut end_of_mem = 0x1_0000_0000u64;
    for e in memory_map.iter() {
        match e.memory_type() {
            CONVENTIONAL_MEMORY | LOADER_CODE | LOADER_DATA => {
                end_of_mem = max(
                    end_of_mem,
                    e.physical_start() + e.number_of_pages() * (PAGE_SIZE as u64),
                );
            }
            _ => (),
        }
    }
    &mut *table
}

pub fn init_hpet(acpi: &AcpiRsdpStruct) {
    let hpet = acpi.hpet().expect("failed to get HPET table");
    let hpet = hpet
        .base_address()
        .expect("HPET base address is not in memory space");
    info!("HPET is at {hpet:#p}");
    let hpet = Hpet::new(hpet);
    set_global_hpet(hpet);
}

pub fn init_allocator(memory_map: &MemoryMapHolder) {
    let mut total_memory_pages = 0;
    for e in memory_map.iter() {
        if e.memory_type() != EfiMemoryType::CONVENTIONAL_MEMORY {
            continue;
        }
        total_memory_pages += e.number_of_pages();
    }
    let total_memory_size_mib = total_memory_pages * 4096 / 1024 / 1024;
    info!("Total memory size: {total_memory_size_mib} MiB");
}

pub fn init_display(vram: &mut VramBufferInfo) {
    let vw = vram.width();
    let vh = vram.height();
    fill_rect(vram, 0x000000, 0, 0, vw, vh).expect("fill_rect failed");
    draw_test_pattern(vram);
}

pub fn init_pci(acpi: &AcpiRsdpStruct) {
    if let Some(mcfg) = acpi.mcfg() {
        for i in 0..mcfg.num_of_entries() {
            if let Some(e) = mcfg.entry(i) {
                info!("{}", e);
            }
        }
        let pci = Pci::new(mcfg);
        pci.probe_devices();
    }
}
