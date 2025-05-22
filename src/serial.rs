use crate::x86::busy_loop_hint;
use crate::x86::read_io_port_u8;
use crate::x86::write_io_port_u8;
use core::fmt;

pub struct SerialPort {
    base: u16,
}
impl SerialPort {
    pub fn new(base: u16) -> Self {
        Self { base }
    }
    pub fn new_for_com1() -> Self {
        Self::new(0x3F8)
    }
    pub fn init(&mut self) {
        write_io_port_u8(self.base + 1, 0x00);
        write_io_port_u8(self.base + 3, 0x80);
        const BAUD_DIVISOR: u16 = 0x0001;
        write_io_port_u8(self.base, (BAUD_DIVISOR & 0xff) as u8);
        write_io_port_u8(self.base + 1, (BAUD_DIVISOR >> 8) as u8);
        write_io_port_u8(self.base + 3, 0x03);
        write_io_port_u8(self.base + 2, 0xC7);
        write_io_port_u8(self.base + 4, 0x0B);
    }
    pub fn send_char(&self, c: char) {
        while (read_io_port_u8(self.base + 5) & 0x20) == 0 {
            busy_loop_hint();
        }
        write_io_port_u8(self.base, c as u8);
    }
    pub fn send_str(&self, s: &str) {
        let mut sc = s.chars();
        let slen = s.chars().count();
        for _ in 0..slen {
            self.send_char(sc.next().unwrap());
        }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let serial = Self::default();
        serial.send_str(s);
        Ok(())
    }
}
impl Default for SerialPort {
    fn default() -> Self {
        Self::new_for_com1()
    }
}
