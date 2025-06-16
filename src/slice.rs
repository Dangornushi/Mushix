extern crate alloc;
use crate::result::Result;
use core::mem::size_of;
use core::slice;

pub unsafe trait Sliceable: Sized + Copy + Clone {
    fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
    fn copy_from_slice(data: &[u8]) -> Result<Self> {
        if size_of::<Self>() > data.len() {
            Err("data is too short")
        } else {
            Ok(unsafe { *(data.as_ptr() as *const Self) })
        }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self as *mut Self as *mut u8, size_of::<Self>()) }
    }
}
