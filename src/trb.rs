use core::{
    mem::{size_of, transmute},
    pin::Pin,
};

#[derive(Copy, Clone)]
#[repr(C, align(16))]
pub struct NormalTrb {
    data_buffer_pointer: u64,
    trb_transfer_length: u32,
    control: u32,
}

impl NormalTrb {
    pub fn new_out<T: Sized>(buf: Pin<&mut [T]>) -> Self {
        Self {
            data_buffer_pointer: buf.as_ptr() as u64,
            trb_transfer_length: (buf.len() * size_of::<T>()) as u32,
            control: (TrbType::Normal as u32) << 10
                | GenericTrbEntry::CTRL_BIT_INTERRUPT_ON_COMPLETION,
            // OUT方向なのでDATA_DIR_INビットは設定しない
        }
    }
}

impl From<NormalTrb> for GenericTrbEntry {
    fn from(trb: NormalTrb) -> GenericTrbEntry {
        unsafe { transmute(trb) }
    }
}
