#[cfg(not(any(
    target_os = "android",
    all(target_os = "linux", not(target_env = "musl"))
)))]
use libc::mmap;
#[cfg(any(
    target_os = "android",
    all(target_os = "linux", not(target_env = "musl"))
))]
use libc::{mmap64 as mmap, off64_t as off_t};

use anyhow::{anyhow, bail};
use core::slice;
use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::{fs::OpenOptions, path::PathBuf};
use std::{io, ptr};
#[derive(Debug)]
pub(crate) struct MmapFile {
    ptr: *mut libc::c_void,
    len: usize,
    file_handle: File,
}
impl Deref for MmapFile {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr as *const u8, self.len as usize) }
    }
}

impl DerefMut for MmapFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { slice::from_raw_parts_mut(self.ptr as *mut u8, self.len) }
    }
}

fn open_mmap_file(
    file_path: &PathBuf,
    read_only: bool,
    create: bool,
    max_file_size: u64,
) -> anyhow::Result<(MmapFile, bool)> {
    let fd = OpenOptions::new()
        .read(true)
        .write(!read_only)
        .create(!read_only && create)
        .open(file_path)
        .map_err(|e| anyhow!("unable to open: {:?} :{}", file_path, e))?;
    let metadata = fd
        .metadata()
        .map_err(|e| anyhow!("cannot get metadata file:{:?} :{}", file_path, e))?;
    let mut file_size = metadata.len();
    let mut is_new_file = false;
    if max_file_size > 0 && file_size == 0 {
        fd.set_len(max_file_size).map_err(|e| {
            anyhow!(
                "cannot truncate {:?} to {} : {}",
                file_path,
                max_file_size,
                e
            )
        })?;
        file_size = max_file_size;
        is_new_file = true;
    }

    let ptr = unsafe {
        let mut prot = libc::PROT_READ;
        if !read_only {
            prot |= libc::PROT_WRITE;
        }
        let flags = libc::MAP_SHARED;
        let ptr = mmap(
            ptr::null_mut(),
            file_size as libc::size_t,
            prot,
            flags,
            fd.as_raw_fd(),
            0,
        );
        if ptr == libc::MAP_FAILED {
            bail!(
                "cannot get mmap from {:?} :{}",
                file_path,
                io::Error::last_os_error()
            );
        }
        ptr
    };
    let mmap_file = MmapFile {
        ptr,
        len: file_size as usize,
        file_handle: fd,
    };

    Ok((mmap_file, is_new_file))
}
