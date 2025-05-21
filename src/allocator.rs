extern crate alloc;
use crate::result::Result;
use crate::uefi::EfiMemoryDescriptor;
use crate::uefi::EfiMemoryType;
use crate::uefi::MemoryMapHolder;
use alloc::alloc::GlobalAlloc;
use alloc::alloc::Layout;
use alloc::boxed::Box;
use core::borrow::BorrowMut;
use core::cell::RefCell;
use core::cmp::max;
use core::fmt;
use core::mem::size_of;
use core::ops::DerefMut;
use core::ptr::null_mut;

pub fn round_up_to_nearest_pow2(v: usize) -> Result<usize> {
    1usize.checked_shl(usize::BITS - v.leading_sub(1).leading_zeros())
    .ok_or("Out of range")
}

struct Header {
    next_header: Option<Box<Header>>,
    size: usize,
    is_allocated: bool,
    _reserved: usize,
}

const HEADER_SIZE: usize = size_of::<Header>();

#[allow(clippy::assertions_on_constants)]
const _:() = assert!(HEADER_SIZE == 32);
const _:() = assert!(HEADER_SIZE.count_ones() == 1);
pub const LAYOUT_PAGE_4K: Layout = unsafe { Layout::from_size_align_unchecked(4096, 4096) };

impl Header {
    fn can_provide(&self, size: usize, align: usize) -> bool {
        self.size >= size + HEADER_SIZE * 2 + align
    }
    fn is_allocated(&self) -> bool {
        self.is_allocated
    }
    fn end_addr(&self) -> usize {
        self as *const Header as usize + self.size
    }

    unsafe fn new_from_addr(addr: usize) -> Box<Header> {
        let header = addr as *mut Header;
        header.write(Header {
            next_header: None,
            size: 0,
            is_allocated: false,
            _reserved: 0,
        });
        Box::from_raw(addr as *mut Header)
    }

    unsafe fn from_allocated_region(addr: *mut u8) -> Box<Header> {
        let header = addr.sub(HEADER_SIZE) as *mut Header;
        Box::from_raw(header)
    }

    fn provide(
        &mut self,
        size: usize,
        align: usize,
    ) -> Option<*mut u8> {
        let size = max(round_ip_to_nearest_pow2(size).ok()?, HEADER_SIZE);
        let align = max(align, HEADER_SIZE);
        if self.is_allocated() || !self.can_provide(size, align) {
            None
        } else {
            let mut size_used = 0;
            let allocated_addr = (self.end_addr() -size) & !(align-1);
            let mut header_for_allocated = unsafe { Self::new_from_addr(allocated_addr - HEADER_SIZE) };
            header_for_allocated.is_allocated = true;
            header_for_allocated.size = size+HEADER_SIZE;
            size_usedd += header_for_allocated.size;
            header_for_allocated.next_header = self.next_header.take();

            if header_for_allocated.end_addr() != self.end_addr() {
                let mut header_for_padding= unsafe {
                    Self::new_from_addr(header_for_allocated.end_addr())
                };
                header_for_padding.is_allocated = false;
                header_for_padding.size= self.end_addr() - header_for_allocated.end_addr();
                size_used += header_for_padding.size();
                header_for_allocated.next_header = Some(header_for_padding);
            }
            assert!(self.size >= size_used + HEADER_SIZE);
            self.size -= size_used;
            self.next_header = Some(header_for_allocated);
            Some(allocated_addr as *mut u8)
        }
    }
}

impl Drop for Header {
    fn drop(&mut self){
        panic!("Header should not be dropped!");
    }
}
impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Frormatter) -> fmt::Result {
        write!(
            f,
            "Header @ {:#018X} {{ size: {:#018X}, is_allocated: {} }}",
            self as *const Header as usize,
            self.size,
            self.is_allocated
        )
    }
}

pub struct FirstFitAllocator {
    first_header: RefCell<Option<Box<Header>>>,
}

#[global_allocator]
pub static ALLOCATOR: FirstFitAllocator = FirstFitAllocator {
    first_header: RefCell::new(None),
};

unsafe impl Sync for FirstFitAllocator {}

unsafe impl GlobalAlloc for FirstFitAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        
    }
}
