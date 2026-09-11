//! affinity.rs — Surgical thread-to-core pinning (SetThreadAffinityMask).
//!
//! Core map (4 physical cores; degrades gracefully on smaller/odd
//! topologies by intersecting the desired mask with the process affinity):
//!   Core 0 — Win32 message loop + presentation (the winit thread).
//!   Core 1 + 3 — DOM parse, CSS cascade, layout (layout worker thread).
//!   Core 2 — Network fetch workers + SIMD adblock engine.
//!
//! Pinning kills cross-core cache bouncing and scheduler context switches
//! between the four engine subsystems; every API here is best-effort and
//! never panics — a failed pin simply leaves the thread to the OS
//! scheduler, which keeps the browser fully functional.

#![allow(dead_code)]

use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetCurrentProcessorNumber, SetThreadAffinityMask, SetThreadPriority,
    THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_HIGHEST,
};

/// Single-core affinity mask for core N (guarded against huge CPU counts).
#[inline]
pub fn core_mask(core: usize) -> usize {
    if core >= usize::BITS as usize {
        return 0;
    }
    1usize << core
}

/// Intersect a desired mask with what the process is actually allowed to
/// run on, so pinning never empties the mask on constrained systems.
pub fn sanitize_mask(desired: usize) -> usize {
    let allowed = process_affinity_mask();
    let m = desired & allowed;
    if m == 0 {
        allowed
    } else {
        m
    }
}

/// Active processor affinity mask for the current process (kernel32
/// GetProcessAffinityMask).
pub fn process_affinity_mask() -> usize {
    let mut proc_mask: usize = 0;
    let mut sys_mask: usize = 0;
    // SAFETY: both pointers are valid usize slots.
    let ok = unsafe { get_process_affinity_mask(&mut proc_mask, &mut sys_mask) };
    if ok && proc_mask != 0 {
        proc_mask
    } else {
        usize::MAX
    }
}

#[link(name = "kernel32")]
// SAFETY: kernel32 entry points with documented C ABIs; edition 2024
// requires the extern block itself to be marked unsafe.
unsafe extern "system" {
    fn GetProcessAffinityMask(
        hProcess: windows_sys::Win32::Foundation::HANDLE,
        lpProcessAffinityMask: *mut usize,
        lpSystemAffinityMask: *mut usize,
    ) -> i32;
    fn GetCurrentProcess() -> windows_sys::Win32::Foundation::HANDLE;
}

/// SAFETY: FFI with two initialized out-pointers; no invariants to uphold.
fn get_process_affinity_mask(proc_mask: &mut usize, sys_mask: &mut usize) -> bool {
    unsafe {
        let ok = GetProcessAffinityMask(GetCurrentProcess(), proc_mask, sys_mask);
        ok != 0
    }
}

/// Pin the current thread to one core. Returns the mask actually applied
/// (0 means the call was declined by the OS and the thread stays unpinned).
pub fn pin_current_thread(core: usize) -> usize {
    let mask = sanitize_mask(core_mask(core));
    if mask == 0 || mask.count_ones() != 1 {
        return 0;
    }
    // SAFETY: GetCurrentThread returns a pseudo-handle valid for this call.
    unsafe {
        let prev = SetThreadAffinityMask(GetCurrentThread(), mask);
        if prev == 0 {
            0
        } else {
            mask
        }
    }
}

/// Pin the current thread to a multi-core mask (layout engine: cores 1+3).
pub fn pin_current_thread_mask(mask: usize) -> usize {
    let m = sanitize_mask(mask);
    if m == 0 {
        return 0;
    }
    // SAFETY: pseudo-handle, same contract as pin_current_thread.
    unsafe {
        let prev = SetThreadAffinityMask(GetCurrentThread(), m);
        if prev == 0 {
            0
        } else {
            m
        }
    }
}

/// Raise/lower priority for latency-critical vs background engine threads.
pub fn set_current_priority_highest() {
    // SAFETY: pseudo-handle priority call; failure is non-fatal.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

pub fn set_current_priority_background() {
    // SAFETY: same contract as above.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Index of the core this thread is currently running on.
pub fn current_core() -> u32 {
    // SAFETY: pure informational call, no preconditions.
    unsafe { GetCurrentProcessorNumber() }
}

/// Run `f` pinned to a single core, restoring nothing (worker threads are
/// dedicated for their whole lifetime — pin-once).
pub fn spawn_pinned<F, T>(core: usize, name: &str, f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let builder = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(1024 * 1024);
    builder
        .spawn(move || {
            pin_current_thread(core);
            f()
        })
        .expect("FreeWeb: failed to spawn pinned worker")
}

/// Two-core pin (layout engine thread: cores 1 and 3).
pub fn spawn_pinned_pair<F, T>(cores: [usize; 2], name: &str, f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let mask = core_mask(cores[0]) | core_mask(cores[1]);
    let builder = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(2 * 1024 * 1024);
    builder
        .spawn(move || {
            pin_current_thread_mask(mask);
            f()
        })
        .expect("FreeWeb: failed to spawn layout worker")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_masks_are_single_bit() {
        assert_eq!(core_mask(0), 1);
        assert_eq!(core_mask(1), 2);
        assert_eq!(core_mask(3), 8);
        assert_eq!(core_mask(999), 0);
    }

    #[test]
    fn sanitize_never_returns_zero() {
        let m = sanitize_mask(usize::MAX);
        assert_ne!(m, 0);
    }

    #[test]
    fn pinning_current_thread_is_safe() {
        // In CI this may be declined; it must never panic or hang.
        let _ = pin_current_thread(0);
    }
}
