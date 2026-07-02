//! Address-space reservation (SPEC §7.1, S1 subset): one `mmap` reservation,
//! `PROT_NONE`, committed lazily with `mprotect`. S1 commits only eden at
//! offset 0; the `from`/`to`/old layout inside the reservation is S7's
//! concern.

use std::ffi::c_void;

/// A single `mmap` address-space reservation. Uncommitted pages are
/// `PROT_NONE` and touching them faults; [`commit`](Reservation::commit)
/// makes a sub-range read/write.
pub struct Reservation {
    base: usize,
    size: usize,
}

impl Reservation {
    /// Reserve `size` bytes of address space (`PROT_NONE`,
    /// `MAP_PRIVATE|MAP_ANON`). `size` is rounded up to the system page size.
    /// Exits the process (not a panic — an environment limit, not a VM bug)
    /// if the OS refuses the reservation.
    pub fn reserve(size: usize) -> Reservation {
        let size = round_up(size, page_size());
        // SAFETY: a fixed-shape mmap call with a null hint address (the
        // kernel picks the base) and no file descriptor; the result is
        // checked for MAP_FAILED before use.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            eprintln!(
                "macvm: failed to reserve {size} bytes: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(71);
        }
        Reservation {
            base: ptr as usize,
            size,
        }
    }

    /// Make `[off, off+len)` (page-rounded outward) readable and writable.
    /// Exits the process if `mprotect` fails.
    pub fn commit(&self, off: usize, len: usize) {
        let page = page_size();
        let start = round_down(off, page);
        let end = round_up(off.checked_add(len).expect("commit range overflow"), page);
        debug_assert!(
            end <= self.size,
            "commit range [{start},{end}) exceeds reservation of {} bytes",
            self.size
        );
        let ptr = (self.base + start) as *mut c_void;
        // SAFETY: `ptr` and `end - start` are within this reservation
        // (checked above); mprotect only changes protection, no memory is
        // read or written by this call itself.
        let rc = unsafe { libc::mprotect(ptr, end - start, libc::PROT_READ | libc::PROT_WRITE) };
        if rc != 0 {
            eprintln!(
                "macvm: failed to commit {len} bytes at offset {off}: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(71);
        }
    }

    #[inline]
    pub fn base(&self) -> usize {
        self.base
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // SAFETY: `self.base`/`self.size` describe exactly the mapping
        // created in `reserve`; nothing else can alias it (Reservation is
        // not Clone).
        unsafe {
            libc::munmap(self.base as *mut c_void, self.size);
        }
    }
}

fn page_size() -> usize {
    // SAFETY: sysconf with a valid, static name; no memory access.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

fn round_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

fn round_down(v: usize, align: usize) -> usize {
    v & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_commit_rw() {
        let r = Reservation::reserve(64 << 20);
        r.commit(0, 1 << 20);

        let p0 = r.base() as *mut u64;
        let p_last = (r.base() + (1 << 20) - 8) as *mut u64;
        unsafe {
            p0.write(0xDEAD_BEEF_CAFE_0000);
            assert_eq!(p0.read(), 0xDEAD_BEEF_CAFE_0000);
            p_last.write(0x1234_5678_9ABC_DEF0);
            assert_eq!(p_last.read(), 0x1234_5678_9ABC_DEF0);
        }
    }

    #[test]
    fn commit_page_rounding() {
        let r = Reservation::reserve(16 << 20);
        r.commit(0, 4097);
        let p = (r.base() + 8100) as *mut u8;
        unsafe {
            p.write(42);
            assert_eq!(p.read(), 42);
        }
    }

    #[test]
    fn round_trip_helpers() {
        assert_eq!(round_up(0, 16), 0);
        assert_eq!(round_up(1, 16), 16);
        assert_eq!(round_up(16, 16), 16);
        assert_eq!(round_down(17, 16), 16);
        assert_eq!(round_down(16, 16), 16);
    }
}
