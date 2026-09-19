use std::borrow::Cow;
use std::ffi::{c_char, c_void, CStr, CString};
use crate::PokeAByteError;

pub struct PokeAByteSharedMemory {
    memory: &'static mut [u8],
    token: *mut c_void
}

/// The largest shared-memory block a single SETUP may request.
///
/// Only a `usize` overflow was checked before, so an unauthenticated UDP SETUP could request up to
/// ~8 GiB of shared memory. The largest emulated address space (NDS) is about 4.1 MiB; 16 MiB gives
/// headroom for sparse layouts elsewhere while keeping the under-lock zero-fill to milliseconds.
/// macOS mmap is limited to 4 MiB (shm_open fails above that), so the cap is lower there; 4 MiB is
/// still sufficient even for NDS games.
pub(crate) const MAX_SHARED_MEMORY_LENGTH: usize = if cfg!(target_os = "macos") { 4 << 20 } else { 16 << 20 };

unsafe extern "C" {
    fn supershuckie_pokeabyte_try_create_shared_memory(name: *const c_char, len: usize, error: *mut *const c_char, memory: *mut *mut u8) -> *mut c_void;
    fn supershuckie_pokeabyte_close_shared_memory(token: *mut c_void);
}

/// The name of the shared-memory block Poke-A-Byte reads for the server on `port`.
///
/// The default port keeps the name Poke-A-Byte has always used, so an unmodified Poke-A-Byte keeps
/// working with a Super Shuckie on the default port; every other port gets a name of its own, so
/// several servers (one per game) can share a machine. Poke-A-Byte derives the same name from the
/// port it connects to (`SharedConstants.MemoryMappedFileName(port)`).
pub fn shared_memory_name(port: u16) -> String {
    if port == crate::DEFAULT_PORT {
        String::from("EDPS_MemoryData.bin")
    }
    else {
        format!("EDPS_MemoryData_{port}.bin")
    }
}

impl PokeAByteSharedMemory {
    /// # Safety
    ///
    /// The memory returned is not guaranteed to be initialized and must be zero-initialized
    /// manually.
    pub(crate) unsafe fn new(name: &str, len: usize) -> Result<PokeAByteSharedMemory, PokeAByteError> {
        // Defence in depth: `protocol.rs` should already have rejected this, but never hand an
        // out-of-range or zero length to the C shim.
        if len == 0 || len > MAX_SHARED_MEMORY_LENGTH {
            return Err(PokeAByteError::SharedMemoryFailure {
                explanation: Cow::Owned(format!("refusing to share {len} bytes (must be 1..={MAX_SHARED_MEMORY_LENGTH})"))
            })
        }

        let name = CString::new(name).map_err(|_| PokeAByteError::SharedMemoryFailure {
            explanation: Cow::Borrowed("the shared memory name contains a NUL byte")
        })?;

        // A valid pointer up front: the C side may fail before it ever sets `error` (or may not be
        // able to set it at all), and reading through a null/dangling pointer is undefined behavior.
        let mut error: *const c_char = c"unknown error".as_ptr();
        let mut ram: *mut u8 = std::ptr::null_mut();
        let (memory, token) = unsafe {
            let token = supershuckie_pokeabyte_try_create_shared_memory(name.as_ptr(), len, &mut error, &mut ram);
            if token.is_null() || ram.is_null() {
                let message: Cow<str> = if error.is_null() {
                    Cow::Borrowed("unknown error")
                }
                else {
                    CStr::from_ptr(error).to_string_lossy()
                };
                if !token.is_null() {
                    supershuckie_pokeabyte_close_shared_memory(token);
                }
                return Err(PokeAByteError::SharedMemoryFailure { explanation: Cow::Owned(format!("Error sharing {len} bytes as {}: {message}", name.to_string_lossy())) })
            }
            (std::slice::from_raw_parts_mut(ram, len), token)
        };

        Ok(Self {
            memory,
            token
        })
    }

    /// # Safety
    ///
    /// There is no protection against data races from other processes. It is not recommended to use
    /// this for anything except reading bytes.
    #[inline]
    pub unsafe fn get_memory(&self) -> &[u8] {
        self.memory
    }

    /// # Safety
    ///
    /// There is no protection against data races from other processes. It is not recommended to use
    /// this for anything except reading and writing bytes.
    #[inline]
    pub unsafe fn get_memory_mut(&mut self) -> &mut [u8] {
        self.memory
    }
}

impl Drop for PokeAByteSharedMemory {
    fn drop(&mut self) {
        unsafe { supershuckie_pokeabyte_close_shared_memory(self.token) };
    }
}


unsafe impl Sync for PokeAByteSharedMemory {}
unsafe impl Send for PokeAByteSharedMemory {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_port() {
        assert_eq!(shared_memory_name(crate::DEFAULT_PORT), "EDPS_MemoryData.bin");
        assert_eq!(shared_memory_name(55357), "EDPS_MemoryData_55357.bin");
    }

    #[test]
    fn several_mappings_coexist() {
        // Two servers in one process (one per game) each need a mapping of their own; the shims
        // used to hold a single static handle, which made the second one fail.
        let mut a = unsafe { PokeAByteSharedMemory::new("supershuckie-test-shm-a", 4096) }.expect("first mapping");
        let mut b = unsafe { PokeAByteSharedMemory::new("supershuckie-test-shm-b", 8192) }.expect("second mapping");
        unsafe { a.get_memory_mut() }.fill(0xAA);
        unsafe { b.get_memory_mut() }.fill(0xBB);
        assert_eq!(unsafe { a.get_memory() }.len(), 4096);
        assert_eq!(unsafe { b.get_memory() }.len(), 8192);
        assert!(unsafe { a.get_memory() }.iter().all(|&byte| byte == 0xAA));
        assert!(unsafe { b.get_memory() }.iter().all(|&byte| byte == 0xBB));
        drop(a);
        // The second mapping is untouched by closing the first.
        assert!(unsafe { b.get_memory() }.iter().all(|&byte| byte == 0xBB));
    }

    #[test]
    fn bad_lengths_are_refused() {
        assert!(unsafe { PokeAByteSharedMemory::new("supershuckie-test-shm-zero", 0) }.is_err());
        assert!(unsafe { PokeAByteSharedMemory::new("supershuckie-test-shm-huge", MAX_SHARED_MEMORY_LENGTH + 1) }.is_err());
        assert!(unsafe { PokeAByteSharedMemory::new("", 16) }.is_err());
    }
}
