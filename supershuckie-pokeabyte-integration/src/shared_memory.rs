use std::borrow::Cow;
use std::ffi::{c_char, CStr};
use crate::PokeAByteError;

pub struct PokeAByteSharedMemory {
    memory: &'static mut [u8]
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
    fn supershuckie_pokeabyte_try_create_shared_memory(len: usize, error: *mut *const c_char) -> *mut u8;
    fn supershuckie_pokeabyte_close_shared_memory();
}

impl PokeAByteSharedMemory {
    /// # Safety
    ///
    /// The memory returned is not guaranteed to be initialized and must be zero-initialized
    /// manually.
    pub(crate) unsafe fn new(len: usize) -> Result<PokeAByteSharedMemory, PokeAByteError> {
        // Defence in depth: `protocol.rs` should already have rejected this, but never hand an
        // out-of-range or zero length to the C shim.
        if len == 0 || len > MAX_SHARED_MEMORY_LENGTH {
            return Err(PokeAByteError::SharedMemoryFailure {
                explanation: Cow::Owned(format!("refusing to share {len} bytes (must be 1..={MAX_SHARED_MEMORY_LENGTH})"))
            })
        }

        // A valid pointer up front: the C side may fail before it ever sets `error` (or may not be
        // able to set it at all), and reading through a null/dangling pointer is undefined behavior.
        let mut error: *const c_char = c"unknown error".as_ptr();
        let memory = unsafe {
            let ram = supershuckie_pokeabyte_try_create_shared_memory(len, &mut error);
            if ram.is_null() {
                let message: Cow<str> = if error.is_null() {
                    Cow::Borrowed("unknown error")
                }
                else {
                    CStr::from_ptr(error).to_string_lossy()
                };
                return Err(PokeAByteError::SharedMemoryFailure { explanation: Cow::Owned(format!("Error sharing {len} bytes: {message}")) })
            }
            std::slice::from_raw_parts_mut(ram, len)
        };

        Ok(Self {
            memory
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
        unsafe { supershuckie_pokeabyte_close_shared_memory() };
    }
}


unsafe impl Sync for PokeAByteSharedMemory {}
unsafe impl Send for PokeAByteSharedMemory {}
