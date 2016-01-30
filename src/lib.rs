// Filebuffer -- Fast and simple file reading
// Copyright 2016 Ruud van Asseldonk
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Filebuffer, a library for fast and simple file reading.
//!
//! # Examples
//!
//! Map a file into memory and access it as an array of bytes. This is simple and will generally
//! outperform `Read::read_to_end()`.
//!
//! ```
//! use filebuffer::FileBuffer;
//! let fbuffer = FileBuffer::open("src/lib.rs").unwrap();
//! let buffer = fbuffer.as_slice();
//! assert_eq!(buffer[3..45], b"Filebuffer -- Fast and simple file reading"[..]);
//! ```
//!
//! TODO: More examples.

#![warn(missing_docs)]

use std::cmp;
use std::io;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr;
use std::slice;
use std::thread;

extern crate libc;

/// A memory-mapped file.
pub struct FileBuffer {
    page_size: usize,
    buffer: *const u8,
    length: usize,
}

fn map_file(file: &fs::File) -> io::Result<(*const u8, usize)> {
    let fd = file.as_raw_fd();
    let length = try!(file.metadata()).len();

    if length > usize::max_value() as u64 {
        return Err(io::Error::new(io::ErrorKind::Other, "file is larger than address space"));
    }

    // Don't try to map anything if the file is empty.
    if length == 0 {
        return Ok((ptr::null(), 0));
    }

    let result = unsafe {
        libc::mmap(ptr::null_mut(), length as usize, libc::PROT_READ, libc::MAP_PRIVATE, fd, 0)
    };

    if result == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok((result as *const u8, length as usize))
    }
}

fn unmap_file(buffer: *const u8, length: usize) {
    let result = unsafe { libc::munmap(buffer as *mut libc::c_void, length) };

    // `munmap` only fails due to incorrect usage, which is a program error, not a runtime failure.
    assert!(result == 0);
}

/// Writes whether the pages in the range starting at `buffer` with a length of `length` bytes
/// are resident in physical memory into `residency`. The size of `residency` must be at least
/// `length / page_size`. Both `buffer` and `length` must be a multiple of the page size.
fn get_resident(buffer: *const u8, length: usize, residency: &mut [bool]) {
    let result = unsafe {
        let residency_uchar = residency.as_mut_ptr() as *mut libc::c_uchar;
        libc::mincore(buffer as *mut libc::c_void, length, residency_uchar)
    };

    // Any error code except EAGAIN indicates a programming error.
    assert!(result == libc::EAGAIN || result == 0);

    // In the rare occasion that the kernel is busy, yield so we don't spam the kernel with
    // `mincore` calls, then try again.
    if result == libc::EAGAIN {
        thread::yield_now();
        get_resident(buffer, length, residency)
    }
}

/// Requests the kernel to make the specified range of bytes resident in physical memory. `buffer`
/// must be page-aligned.
fn prefetch(buffer: *const u8, length: usize) {
    let result = unsafe {
        libc::madvise(buffer as *mut libc::c_void, length, libc::MADV_WILLNEED)
    };

    // Any returned error code indicates a programming error, not a runtime error.
    assert_eq!(0, result);
}

fn get_page_size() -> usize {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };

    // Assert that the page size is a power of two, which is assumed when the page size is used.
    assert!(page_size != 0);
    assert_eq!(0, page_size & (page_size - 1));

    page_size
}

/// Rounds `size` up to the nearest multiple of `power_of_two`.
fn round_up_to(size: usize, power_of_two: usize) -> usize {
    (size + (power_of_two - 1)) & !(power_of_two - 1)
}

#[test]
fn verify_round_up_to() {
    assert_eq!(1024, round_up_to(23, 1024));
    assert_eq!(1024, round_up_to(1024, 1024));
    assert_eq!(2048, round_up_to(1025, 1024));
}

/// Rounds `size` down to the nearest multiple of `power_of_two`.
fn round_down_to(size: usize, power_of_two: usize) -> usize {
    size & !(power_of_two - 1)
}

#[test]
fn verify_round_down_to() {
    assert_eq!(0, round_down_to(23, 1024));
    assert_eq!(1024, round_down_to(1024, 1024));
    assert_eq!(1024, round_down_to(1025, 1024));
}

impl FileBuffer {
    /// Maps the file at `path` into memory.
    ///
    /// TODO: Document what happens when the file is changed after opening.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<FileBuffer> {
        // Open the `fs::File` so we get all of std's error handling for free, then use it to
        // extract the file descriptor. The file is closed again when it goes out of scope, but
        // `mmap` only requires the descriptor to be open for the `mmap` call, so this is fine.
        let file = try!(fs::File::open(path));
        let (buffer, length) = try!(map_file(&file));
        let fbuffer = FileBuffer {
            page_size: get_page_size(),
            buffer: buffer,
            length: length,
        };
        Ok(fbuffer)
    }

    /// Returns the length of the mapped file in bytes.
    pub fn len(&self) -> usize {
        self.length
    }

    /// Returns the number of bytes resident in physical memory, starting from `offset`.
    ///
    /// The slice `[offset..offset + resident_len]` can be accessed without causing page faults or
    /// disk access. Note that this is only a snapshot, and the kernel might decide to evict pages
    /// or make them resident at any time.
    ///
    /// The returned resident length is at most `length`.
    ///
    /// # Panics
    ///
    /// Panics if the specified range lies outside of the buffer.
    pub fn resident_len(&self, offset: usize, length: usize) -> usize {
        // The specified offset and length must lie within the buffer.
        assert!(offset + length <= self.length);

        // This is a no-op for empty files.
        if self.buffer == ptr::null() { return 0; }

        let aligned_offset = round_down_to(offset, self.page_size);
        let aligned_length = round_up_to(length + (offset - aligned_offset), self.page_size);
        let num_pages = aligned_length / self.page_size;

        // There is a tradeoff here: to store residency information, we need an array of booleans.
        // The requested range can potentially be very large and it is only known at runtime. We
        // could allocate a vector here, but that requires a heap allocation just to get residency
        // information (which might in turn cause a page fault). Instead, check at most 32 pages at
        // once. This means more syscalls for large ranges, but it saves us the heap allocation,
        // and for ranges up to 32 pages (128 KiB typically) there is only one syscall.
        let mut residency = [false; 32];
        let mut pages_checked = 0;
        let mut pages_resident = 0;

        while pages_checked < num_pages {
            let pages_to_check = cmp::min(32, num_pages - pages_checked);
            let check_offset = (aligned_offset + pages_checked * self.page_size) as isize;
            let check_buffer = unsafe { self.buffer.offset(check_offset) };
            let check_length = pages_to_check * self.page_size;
            get_resident(check_buffer, check_length, &mut residency);

            // Count the number of resident pages.
            match residency[..pages_to_check].iter().position(|resident| !resident) {
                Some(non_resident) => {
                    // The index of the non-resident page is the number of resident pages.
                    pages_resident += non_resident;
                    break;
                }
                None => {
                    pages_resident += pages_to_check;
                    pages_checked += pages_to_check;
                }
            }
        }

        let resident_length = pages_resident * self.page_size + aligned_offset - offset;

        // Never return more than the requested length. The resident length might be larger than
        // the length of the buffer, because it is rounded up to the page size.
        cmp::min(length, resident_length)
    }

    /// Returns a good chunk length if the file is to be processed in chunks.
    ///
    /// This just returns the page size. Multiples of this are fine too.
    pub fn chunk_len_hint(&self) -> usize {
        self.page_size
    }

    /// Returns the file contents as a slice.
    ///
    /// Accessing elements of the slice might cause a page fault, blocking until the data has been
    /// read from disk.
    ///
    /// To avoid blocking, call `prefetch()` and check whether the memory is resident with
    /// `resident_len()`. The `try_slice()` method is similar to `as_slice()`, but it returns
    /// `None` if accessing the slice would block.
    pub fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.buffer, self.length) }
    }

    /// Returns a slice if the requested range is resident in physical memory.
    ///
    /// If the slice is not resident, `prefetch()` is called, so that if the same slice is
    /// requested after a while, it likely is resident.
    ///
    /// # Panics
    ///
    /// Panics if the specified range lies outside of the buffer.
    pub fn try_slice(&self, offset: usize, length: usize) -> Option<&[u8]> {
        // TODO: Due to Rust internals, returning `Some` when `self.buffer` is null, is not
        // possible. But should this method return an option anyway?

        // The bounds check assertion is done in `resident_len()`, no need to duplicate it here.
        if self.resident_len(offset, length) < length {
            self.prefetch(offset, length);
            None
        } else {
            Some(&self.as_slice()[offset..offset + length])
        }
    }

    /// Advises the kernel to make a slice of the file resident in physical memory.
    ///
    /// This method does not block, meaning that when the function returns, the slice is not
    /// necessarily resident. After this function returns, the kernel may read the requested slice
    /// from disk and make it resident. Note that this is only an advice, the kernel need not honor
    /// it.
    ///
    /// To check whether the slice is resident at a later time, use `resident_len()`.
    ///
    /// # Panics
    ///
    /// Panics if the specified range lies outside of the buffer.
    pub fn prefetch(&self, offset: usize, length: usize) {
        // The specified offset and length must lie within the buffer.
        assert!(offset + length <= self.length);

        // This is a no-op for empty files.
        if self.buffer == ptr::null() { return; }

        let aligned_offset = round_down_to(offset, self.page_size);
        let aligned_length = round_up_to(length + (offset - aligned_offset), self.page_size);

        let buffer = unsafe { self.buffer.offset(aligned_offset as isize) };
        prefetch(buffer, aligned_length);
    }
}

impl Drop for FileBuffer {
    fn drop(&mut self) {
        if self.buffer != ptr::null() { unmap_file(self.buffer, self.length); }
    }
}

#[test]
fn open_file() {
    let fbuffer = FileBuffer::open("src/lib.rs");
    assert!(fbuffer.is_ok());
}

#[test]
fn make_resident() {
    let fbuffer = FileBuffer::open("src/lib.rs").unwrap();

    // Touch the first page to make it resident.
    assert_eq!(fbuffer.as_slice()[3..13], b"Filebuffer"[..]);

    // Now at least that part should be resident.
    assert_eq!(fbuffer.resident_len(3, 10), 10);

    // If it is resident, `try_slice` should return a slice.
    assert_eq!(fbuffer.try_slice(3, 10), Some(&b"Filebuffer"[..]));
}

#[test]
fn prefetch_is_not_harmful() {
    let fbuffer = FileBuffer::open("src/lib.rs").unwrap();

    // It is impossible to test that this actually works without root access to instruct the kernel
    // to drop its caches, but at least we can verify that calling `prefetch` is not harmful.
    fbuffer.prefetch(0, fbuffer.len());

    // Reading from the file should still work as normal.
    assert_eq!(fbuffer.as_slice()[3..13], b"Filebuffer"[..]);
}

#[test]
fn open_empty_file_is_fine() {
    FileBuffer::open("src/empty_file_for_testing.rs").unwrap();
}

#[test]
fn empty_file_prefetch_is_fine() {
    let fbuffer = FileBuffer::open("src/empty_file_for_testing.rs").unwrap();
    fbuffer.prefetch(0, 0);
}

#[test]
fn empty_file_as_slice_is_fine() {
    let fbuffer = FileBuffer::open("src/empty_file_for_testing.rs").unwrap();
    assert_eq!(fbuffer.as_slice().iter().any(|_| true), false);
}

#[test]
fn empty_file_has_zero_resident_len() {
    let fbuffer = FileBuffer::open("src/empty_file_for_testing.rs").unwrap();
    assert_eq!(fbuffer.resident_len(0, 0), 0);
}
