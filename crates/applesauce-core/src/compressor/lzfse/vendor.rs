use crate::compressor::lz;
use std::ffi::c_void;
use std::ptr::NonNull;

pub struct Impl<const ULTRA: bool>;

extern "C" {
    fn applesauce_lzfse_stock_lzfse_encode_scratch_size() -> usize;
    fn applesauce_lzfse_ultra_lzfse_encode_scratch_size() -> usize;
    fn applesauce_lzfse_stock_lzfse_encode_buffer(
        dst: *mut u8,
        dst_size: usize,
        src: *const u8,
        src_size: usize,
        scratch: *mut c_void,
    ) -> usize;
    fn applesauce_lzfse_ultra_lzfse_encode_buffer(
        dst: *mut u8,
        dst_size: usize,
        src: *const u8,
        src_size: usize,
        scratch: *mut c_void,
    ) -> usize;
}

// SAFETY: Each implementation allocates enough workspace for its fixed encoder
// and the standard decoder. The size is constant for the lifetime of each type.
unsafe impl<const ULTRA: bool> lz::Impl for Impl<ULTRA> {
    fn scratch_size() -> usize {
        // Separate cache locations: statics inside a generic implementation are
        // shared by its instantiations, whereas these backends need different sizes.
        if ULTRA {
            // SAFETY: Scratch-size queries take no pointers and have no preconditions.
            lz::cached_size!(unsafe {
                applesauce_lzfse_ultra_lzfse_encode_scratch_size()
                    .max(lzfse_sys::lzfse_decode_scratch_size())
                    .max(1)
            })
        } else {
            // SAFETY: Scratch-size queries take no pointers and have no preconditions.
            lz::cached_size!(unsafe {
                applesauce_lzfse_stock_lzfse_encode_scratch_size()
                    .max(lzfse_sys::lzfse_decode_scratch_size())
                    .max(1)
            })
        }
    }

    unsafe fn encode(dst: &mut [u8], src: &[u8], scratch: NonNull<u8>) -> usize {
        let encode = if ULTRA {
            applesauce_lzfse_ultra_lzfse_encode_buffer
        } else {
            applesauce_lzfse_stock_lzfse_encode_buffer
        };
        // SAFETY: Caller supplies valid, non-overlapping buffers and scratch_size() bytes
        // of aligned workspace, sufficient for the selected preset.
        unsafe {
            encode(
                dst.as_mut_ptr(),
                dst.len(),
                src.as_ptr(),
                src.len(),
                scratch.as_ptr().cast(),
            )
        }
    }

    unsafe fn decode(dst: &mut [u8], src: &[u8], scratch: NonNull<u8>) -> usize {
        // SAFETY: Caller supplies valid buffers and workspace sized for the standard decoder.
        unsafe {
            lzfse_sys::lzfse_decode_buffer(
                dst.as_mut_ptr(),
                dst.len(),
                src.as_ptr(),
                src.len(),
                scratch.as_ptr().cast(),
            )
        }
    }
}
