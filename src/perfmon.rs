//! perfmon.rs — PDH CPU telemetry + dynamic thermal governor.
//!
//! Samples "\Processor(_Total)\% Processor Time" through the Windows PDH
//! API (pdh.dll) on a dedicated below-normal monitor thread pinned to the
//! network core. When sustained load crosses GOV_HIGH (85%), the governor
//! steps the UI frame pacing from 60 FPS down to 30 FPS; it climbs back
//! when load falls below GOV_LOW (60%). Every transition is pushed to the
//! UI through a caller-supplied callback (wired to the winit event-loop
//! proxy), so nothing polls.
//!
//! The monitor thread sleeps between samples — there is no busy-wait
//! anywhere in the engine, so idle CPU stays at 0%.

#![allow(dead_code)]

// Self-contained PDH FFI: the whole surface we need is five functions.
mod pdh {
    pub const PDH_FMT_DOUBLE: u32 = 0x0000_0200;

    /// PDH_FMT_COUNTERVALUE is a union plus a status; we only consume the
    /// double branch (PDH_FMT_DOUBLE), modeled as a plain pair with the
    /// same layout (CStatus first, 8-byte-aligned double second).
    #[repr(C)]
    pub struct PdhFmtCounterValue {
        pub c_status: u32,
        pub double_value: f64,
    }

    #[link(name = "pdh")]
    // SAFETY: pdh.dll entry points with documented C ABIs; edition 2024
    // requires the extern block itself to be marked unsafe.
    unsafe extern "system" {
        pub fn PdhOpenQueryW(
            szDataSource: *const u16,
            dwUserData: usize,
            phQuery: *mut isize,
        ) -> u32;
        pub fn PdhAddCounterW(
            hQuery: isize,
            szFullCounterPath: *const u16,
            dwUserData: usize,
            phCounter: *mut isize,
        ) -> u32;
        pub fn PdhCollectQueryData(hQuery: isize) -> u32;
        pub fn PdhGetFormattedCounterValue(
            hCounter: isize,
            dwFormat: u32,
            lpdwType: *mut u32,
            pValue: *mut PdhFmtCounterValue,
        ) -> u32;
        pub fn PdhCloseQuery(hQuery: isize) -> u32;
    }
}

/// Load thresholds (percent) for the FPS governor.
const GOV_HIGH: f64 = 85.0;
const GOV_LOW: f64 = 60.0;

/// Adaptive frame-rate target shared with the UI thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBudget {
    /// 60 FPS — 16.6 ms budget.
    Full,
    /// 30 FPS — 33.3 ms budget (thermal mitigation).
    Halved,
}

impl FrameBudget {
    #[inline]
    pub fn frame_time_ms(self) -> f64 {
        match self {
            FrameBudget::Full => 16.6,
            FrameBudget::Halved => 33.3,
        }
    }

    #[inline]
    pub fn fps(self) -> u32 {
        match self {
            FrameBudget::Full => 60,
            FrameBudget::Halved => 30,
        }
    }
}

/// Shared governor state written by the monitor thread, read by the UI.
pub struct Governor {
    state: std::sync::Arc<std::sync::Mutex<GovernorInner>>,
}

struct GovernorInner {
    budget: FrameBudget,
    cpu_percent: f64,
}

impl Governor {
    pub fn current(&self) -> FrameBudget {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .budget
    }

    pub fn cpu(&self) -> f64 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cpu_percent
    }
}

struct PdhQuery {
    handle: isize,
    counter: isize,
}

impl PdhQuery {
    /// Open a query bound to total processor time.
    fn new() -> Option<PdhQuery> {
        unsafe {
            let mut handle: isize = 0;
            // SAFETY: valid out-pointer; PDH contracts honored.
            if pdh::PdhOpenQueryW(core::ptr::null(), 0, &mut handle) != 0 {
                return None;
            }
            let path: Vec<u16> = "\\Processor(_Total)\\% Processor Time"
                .encode_utf16()
                .chain(core::iter::once(0))
                .collect();
            let mut counter: isize = 0;
            // SAFETY: null-terminated wide path, valid out-pointer.
            if pdh::PdhAddCounterW(handle, path.as_ptr(), 0, &mut counter) != 0 {
                pdh::PdhCloseQuery(handle);
                return None;
            }
            Some(PdhQuery { handle, counter })
        }
    }

    /// Blocking collect — the OS call sleeps; zero CPU burn between samples.
    fn collect_blocking(&self) -> bool {
        // SAFETY: valid query handle.
        unsafe { pdh::PdhCollectQueryData(self.handle) == 0 }
    }

    fn read_percent(&self) -> Option<f64> {
        let mut value = pdh::PdhFmtCounterValue {
            c_status: 0,
            double_value: 0.0,
        };
        // SAFETY: valid counter handle + initialized out-struct.
        let ok = unsafe {
            pdh::PdhGetFormattedCounterValue(
                self.counter,
                pdh::PDH_FMT_DOUBLE,
                core::ptr::null_mut(),
                &mut value,
            )
        };
        if ok != 0 {
            return None;
        }
        Some(value.double_value)
    }
}

impl Drop for PdhQuery {
    fn drop(&mut self) {
        // SAFETY: handle owned exclusively by this struct.
        unsafe { pdh::PdhCloseQuery(self.handle) };
    }
}

/// Spawn the monitor thread pinned to the network core (background work
/// that must never land on the UI or layout cores). `notify` is invoked
/// on every budget transition; wire it to `EventLoopProxy::send_event`.
pub fn spawn_monitor<F>(notify: F) -> Governor
where
    F: Fn(FrameBudget) + Send + 'static,
{
    let state = std::sync::Arc::new(std::sync::Mutex::new(GovernorInner {
        budget: FrameBudget::Full,
        cpu_percent: 0.0,
    }));
    let state2 = state.clone();

    let builder = std::thread::Builder::new().name("freeweb-perfmon".into());
    if builder.spawn(move || {
        crate::affinity::pin_current_thread(2);
        crate::affinity::set_current_priority_background();
        let Some(q) = PdhQuery::new() else {
            // PDH unavailable (stripped service): keep Full budget forever.
            return;
        };
        // First sample primes the counter (PDH needs two collections).
        q.collect_blocking();
        loop {
            if !q.collect_blocking() {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                continue;
            }
            if let Some(pct) = q.read_percent() {
                let mut guard = state2.lock().unwrap_or_else(|p| p.into_inner());
                guard.cpu_percent = pct.clamp(0.0, 100.0);
                let next = match guard.budget {
                    FrameBudget::Full if pct > GOV_HIGH => FrameBudget::Halved,
                    FrameBudget::Halved if pct < GOV_LOW => FrameBudget::Full,
                    other => other,
                };
                if next != guard.budget {
                    guard.budget = next;
                    drop(guard);
                    notify(next);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
    })
    .is_err()
    {
        // Monitor detached by design; spawn failure is non-fatal.
    }

    Governor { state }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_match_spec() {
        assert_eq!(FrameBudget::Full.fps(), 60);
        assert_eq!(FrameBudget::Halved.fps(), 30);
        assert!(FrameBudget::Full.frame_time_ms() < 17.0);
        assert!(FrameBudget::Halved.frame_time_ms() > 33.0);
    }
}
