use core::arch::asm;

pub fn hlt() {
    unsafe {asm!("hlt")}
}

pub fn write_io_port_u8(port: u16, value: u8) {
    unsafe {
        asm!(
            "out dx, al",
            in("al") value,
            in("dx") port,
        )
    }
}