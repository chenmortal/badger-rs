
use core::slice;
use std::alloc::{alloc, dealloc, Layout};
use std::sync::Arc;

use std::ptr::{self, drop_in_place, NonNull, Unique};
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicUsize;

pub fn add(left: usize, right: usize) -> usize {
    Arc::new(5);
    left + right
}
const CHUNK_ALIGN: usize = 16;
const PAGE_CUTOFF: usize = 4096;
const DEFAULT_ALIGN: usize = 8;
struct ArenaSlice<T> {
    ptr: AtomicPtr<T>,
    len: usize,
}
impl<T> ArenaSlice<T> {
    fn get(&self) -> &[T] {
        let ptr_raw = self.ptr.load(std::sync::atomic::Ordering::SeqCst);
        unsafe { slice::from_raw_parts(ptr_raw, self.len) }
    }
    fn get_mut(&self) -> &mut [T] {
        let ptr_raw = self.ptr.load(std::sync::atomic::Ordering::SeqCst);
        unsafe { slice::from_raw_parts_mut(ptr_raw, self.len) }
    }
}

struct Arena {
    start: Unique<u8>,
    ptr: AtomicPtr<u8>,
    end: Unique<u8>,
    // size: usize,
    layout: Layout,
    // allocated_bytes: AtomicUsize,
}
impl Arena {
    fn new(size: usize) -> Arena {
        let chunk_align = CHUNK_ALIGN;
        let mut request_size = Self::round_up_to(size, chunk_align).unwrap();
        if request_size >= PAGE_CUTOFF {
            request_size = Self::round_up_to(request_size, PAGE_CUTOFF).unwrap();
        }
        // debug_assert_eq!(chunk_align % CHUNK_ALIGN, 0);
        debug_assert_eq!(request_size % CHUNK_ALIGN, 0);
        let layout = Layout::from_size_align(request_size, chunk_align).unwrap();
        let (data, end) = unsafe {
            let data_ptr = alloc(layout);
            let data = Unique::new(data_ptr).unwrap();
            let end_ptr = data.as_ptr().add(layout.size());
            let end = Unique::new(end_ptr).unwrap();
            (data, end)
        };
        debug_assert_eq!((data.as_ptr() as usize) % layout.align(), 0);
        debug_assert_eq!((end.as_ptr() as usize) % CHUNK_ALIGN, 0);
        let ptr = AtomicPtr::new(NonNull::new(data.as_ptr()).unwrap().as_ptr());
        Self {
            start: data,
            ptr,
            end,
            // size: layout.size(),
            layout,
        }
    }
    fn alloc<T>(&self, value: T) -> &mut T {
        self.alloc_with(|| value)
    }
    fn alloc_with<F, T>(&self, f: F) -> &mut T
    where
        F: FnOnce() -> T,
    {
        #[inline(always)]
        unsafe fn inner_write<T, F>(dst: *mut T, f: F)
        where
            F: FnOnce() -> T,
        {
            ptr::write(dst, f())
        }
        let layout = Layout::new::<T>();
        let p = self.alloc_layout(layout);
        let dst = p.as_ptr() as *mut T;
        unsafe {
            inner_write(dst, f);
            &mut *dst
        }
    }
    fn alloc_layout(&self, layout: Layout) -> NonNull<u8> {
        debug_assert!(DEFAULT_ALIGN.is_power_of_two());
        let layout = layout.align_to(DEFAULT_ALIGN).unwrap();
        let end_ptr = self.end.as_ptr();
        let start_ptr = self.start.as_ptr();
        let alloc_size = Self::round_up_to(layout.size(), layout.align()).unwrap();
        let old_ptr = self
            .ptr
            .fetch_ptr_add(alloc_size, std::sync::atomic::Ordering::SeqCst);
        debug_assert_eq!(old_ptr as usize % 8, 0);
        unsafe {
            let new_ptr = old_ptr.add(alloc_size);
            if new_ptr > end_ptr {
                let new_total = new_ptr.sub_ptr(start_ptr);
                panic!(
                    "Arena too small, toWrite:{}, newTotal:{}, limit:{}",
                    layout.size(),
                    new_total,
                    self.layout.size()
                );
            }
            NonNull::new_unchecked(old_ptr)
        }
    }
    #[inline(always)]
    fn alloc_slice_copy<T: Copy>(&self, src: &[T]) -> ArenaSlice<T> {
        let layout = Layout::for_value(src);
        let dst = self.alloc_layout(layout).cast::<T>();
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), dst.as_ptr(), src.len());
        }
        let atomic = AtomicPtr::new(dst.as_ptr());
        ArenaSlice {
            ptr: atomic,
            len: src.len(),
        }
    }
    #[inline(always)]
    fn alloc_slice_clone<T: Clone>(&self, src: &[T]) -> ArenaSlice<T> {
        let layout = Layout::for_value(src);
        let dst = self.alloc_layout(layout).cast::<T>();
        unsafe {
            for (i, val) in src.iter().cloned().enumerate() {
                ptr::write(dst.as_ptr().add(i), val);
            }
        }
        let atomic = AtomicPtr::new(dst.as_ptr());
        ArenaSlice {
            ptr: atomic,
            len: src.len(),
        }
    }
    #[inline(always)]
    fn round_up_to(n: usize, divisor: usize) -> Option<usize> {
        debug_assert!(divisor > 0);
        debug_assert!(divisor.is_power_of_two());
        Some(n.checked_add(divisor - 1)? & !(divisor - 1))
    }
}
impl Drop for Arena {
    fn drop(&mut self) {
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                self.start.as_ptr(),
                self.layout.size(),
            ));
            dealloc(self.start.as_ptr(), self.layout);
        }
    }
}
#[cfg(test)]
mod tests {
    use std::mem;

    use super::*;
    #[derive(Debug)]
    struct Node {
        a: u64,
        b: u16,
        c: u32,
    }
    #[test]
    fn test_round() {
        let p = &b"hello world"[..];
        let k = &b"rust nb"[..];
        let arena = Arena::new(100);
        let slice = arena.alloc_slice_copy(p);
        let slice_a = arena.alloc_slice_copy(k);
        let node = arena.alloc(Node { a: 1, b: 2, c: 3 });
        dbg!(String::from_utf8_lossy(slice.get()));
        dbg!(String::from_utf8_lossy(slice_a.get()));
        dbg!(node);
        // let k = Layout::for_value(p).align_to(8).unwrap();
        // dbg!(k);
        // let (size, align) = dbg!((mem::size_of_val(p), mem::align_of_val(p)));
        // dbg!(Arena::round_up_to(1000, 8));
    }
    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
