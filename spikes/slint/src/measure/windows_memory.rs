//! Opt-in end-of-run classification of resident pages; no paths or addresses
//! escape this module. Not a peak classifier and not used during UI timings.
use std::{ffi::c_void, io, mem::size_of};
#[repr(C)]
#[derive(Default)]
struct Region {
    base: usize,
    allocation_base: usize,
    allocation_protect: u32,
    #[cfg(target_pointer_width = "64")]
    partition: u16,
    size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "GetCurrentProcess"]
    fn current_process() -> *mut c_void;
    #[link_name = "K32QueryWorkingSet"]
    fn working_set(process: *mut c_void, output: *mut c_void, bytes: u32) -> i32;
    #[link_name = "VirtualQuery"]
    fn query(address: *const c_void, output: *mut Region, bytes: usize) -> usize;
}
pub(super) fn json() -> io::Result<String> {
    // PSAPI_WORKING_SET_INFORMATION: count followed by pointer-sized flags.
    // Bound the diagnostic buffer to 8 MiB; fail rather than truncate.
    let mut words = vec![0usize; 4096];
    loop {
        let result = unsafe {
            working_set(
                current_process(),
                words.as_mut_ptr().cast(),
                (words.len() * size_of::<usize>()) as u32,
            )
        };
        if result != 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(24) {
            return Err(error);
        }
        let needed = words[0]
            .checked_add(1025)
            .filter(|&n| n <= 1024 * 1024)
            .ok_or_else(|| io::Error::other("Working-set diagnostic buffer limit"))?;
        if needed <= words.len() {
            return Err(io::Error::other("Invalid working-set size"));
        }
        words.resize(needed, 0);
    }
    let count = words[0];
    if count >= words.len() {
        return Err(io::Error::other("Invalid working-set count"));
    }
    let (mut image, mut mapped, mut private, mut other, mut sharable) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for &flags in &words[1..count + 1] {
        // Documented PSAPI_WORKING_SET_BLOCK: 12 low flag bits, then VirtualPage.
        let address = flags & !0xfff;
        let mut region = Region::default();
        let size = unsafe { query(address as *const c_void, &mut region, size_of::<Region>()) };
        if size != size_of::<Region>() {
            return Err(io::Error::last_os_error());
        }
        match region.kind {
            0x1000000 => image += 4096,
            0x40000 => mapped += 4096,
            0x20000 => private += 4096,
            _ => other += 4096,
        }
        if flags & (1 << 8) != 0 {
            sharable += 4096;
        }
    }
    Ok(format!(
        "{{\"resident_bytes\":{},\"image_bytes\":{image},\"mapped_bytes\":{mapped},\"private_bytes\":{private},\"other_bytes\":{other},\"sharable_bytes\":{sharable}}}",
        count * 4096
    ))
}
