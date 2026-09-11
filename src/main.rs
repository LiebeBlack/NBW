//! main.rs — FreeWeb 2.0: ultra-light native Windows browser.
//!
//! Threading / affinity map (SetThreadAffinityMask, 4 physical cores):
//!   Core 0 — Win32 message loop + presentation (the winit thread).
//!   Core 1 + 3 — DOM parse, CSS cascade and layout (dedicated worker).
//!   Core 2 — Network fetch workers + SIMD adblock + PDH perf monitor.
//!
//! Zero-Crash Engine: main() runs the whole session inside
//! std::panic::catch_unwind; a panic on the UI thread is contained and the
//! session restarts once with a fresh event loop instead of aborting.
//! Layout and fetch run inside their own catch_unwind guards on their
//! worker threads.
//!
//! Zero-CPU idle: ControlFlow::Wait parks the thread in the OS message
//! pump. Adaptive pacing (60 FPS → 30 FPS under thermal load) is driven by
//! WaitUntil deadlines computed from the PDH governor budget — there is no
//! busy-wait and no sleep loop anywhere.
//!
//! Rendering and networking are 100% native: GDI DIB framebuffer, Schannel
//! TLS, own HTML/CSS engine. No WebView2, no Chromium, no CEF, no Electron.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adblock;
mod affinity;
mod css;
mod dom;
mod font8x8;
mod http;
mod perfmon;
mod renderer;
mod simd_hash;

use adblock::{AdBlocker, Verdict};
use css::{parse_stylesheet, Color, Stylesheet};
use dom::{parse_html, Dom, NodeType};
use perfmon::FrameBudget;
use renderer::{
    draw_text, hit_test, layout, paint, resolve_url, text_width, Frame, LayoutResult,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, Key, NamedKey, PhysicalKey};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorIcon, Window, WindowId};

// ---------------------------------------------------------------------------
// GDI software surface (CreateDIBSection + SetDIBitsToDevice)
// ---------------------------------------------------------------------------
mod gdi {
    #![allow(non_snake_case)]
    use windows_sys::Win32::Foundation::{HANDLE, HWND};
    use windows_sys::Win32::Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, HBITMAP, HDC, HGDIOBJ, RGBQUAD,
    };

    #[link(name = "gdi32")]
    // SAFETY: standard Win32 GDI entry points with documented ABIs;
    // edition 2024 requires the extern block itself to be marked unsafe.
    unsafe extern "system" {
        pub fn CreateCompatibleDC(hdc: HDC) -> HDC;
        pub fn DeleteDC(hdc: HDC) -> i32;
        pub fn DeleteObject(ho: *mut core::ffi::c_void) -> i32;
        pub fn CreateDIBSection(
            hdc: HDC,
            pbmi: *const BITMAPINFO,
            iUsage: u32,
            ppvBits: *mut *mut core::ffi::c_void,
            hSection: *mut core::ffi::c_void,
            dwOffset: u32,
        ) -> HBITMAP;
        pub fn SelectObject(hdc: HDC, h: HGDIOBJ) -> HGDIOBJ;
        pub fn SetDIBitsToDevice(
            hdc: HDC,
            xDest: i32,
            yDest: i32,
            width: u32,
            height: u32,
            xSrc: i32,
            ySrc: i32,
            StartScan: u32,
            NumScans: u32,
            lpBits: *const core::ffi::c_void,
            lpBitsInfo: *const BITMAPINFO,
            iUsage: u32,
        ) -> i32;
    }

    // GetDC/ReleaseDC live in user32, not gdi32.
    #[link(name = "user32")]
    // SAFETY: standard Win32 GDI entry points with documented ABIs;
    // edition 2024 requires the extern block itself to be marked unsafe.
    unsafe extern "system" {
        pub fn GetDC(hwnd: HWND) -> HDC;
        pub fn ReleaseDC(hwnd: HWND, hdc: HDC) -> i32;
    }

    // Clipboard plumbing for Ctrl+C (copy URL) / Ctrl+V (paste and go).
    #[link(name = "user32")]
    // SAFETY: standard Win32 clipboard entry points with documented ABIs.
    unsafe extern "system" {
        pub fn OpenClipboard(hWndNewOwner: HWND) -> i32;
        pub fn CloseClipboard() -> i32;
        pub fn EmptyClipboard() -> i32;
        pub fn SetClipboardData(uFormat: u32, hMem: HANDLE) -> HANDLE;
        pub fn GetClipboardData(uFormat: u32) -> HANDLE;
    }

    #[link(name = "kernel32")]
    // SAFETY: standard Win32 global-heap entry points with documented ABIs.
    unsafe extern "system" {
        pub fn GlobalAlloc(uFlags: u32, dwBytes: usize) -> HANDLE;
        pub fn GlobalLock(hMem: HANDLE) -> *mut core::ffi::c_void;
        pub fn GlobalUnlock(hMem: HANDLE) -> i32;
        pub fn GlobalFree(hMem: HANDLE) -> HANDLE;
    }

    pub const CF_UNICODETEXT: u32 = 13;
    pub const GMEM_MOVEABLE: u32 = 0x0002;

    pub const DIB_RGB_COLORS: u32 = 0;
    pub const BI_RGB: u32 = 0;

    fn top_down_info(w: i32, h: i32) -> BITMAPINFO {
        BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }],
        }
    }

    /// A CPU framebuffer bound to an HWND through a memory DC.
    pub struct DibSurface {
        hwnd: isize,
        hdc_window: HDC,
        hdc_mem: HDC,
        bitmap: HBITMAP,
        old_bitmap: HGDIOBJ,
        width: i32,
        height: i32,
    }

    impl DibSurface {
        pub fn new(hwnd: isize, w: i32, h: i32) -> Result<DibSurface, String> {
            if w <= 0 || h <= 0 {
                return Err("surface size must be positive".into());
            }
            // SAFETY: GDI handles and the BITMAPINFO describe a single
            // 32bpp top-down DIB; every handle is released in Drop.
            unsafe {
                let hdc_window = GetDC(hwnd as HWND);
                if hdc_window.is_null() {
                    return Err("GetDC failed".into());
                }
                let hdc_mem = CreateCompatibleDC(hdc_window);
                if hdc_mem.is_null() {
                    ReleaseDC(hwnd as HWND, hdc_window);
                    return Err("CreateCompatibleDC failed".into());
                }
                let bi = top_down_info(w, h);
                let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
                let bitmap = CreateDIBSection(
                    hdc_mem,
                    &bi,
                    DIB_RGB_COLORS,
                    &mut bits,
                    core::ptr::null_mut(),
                    0,
                );
                if bitmap.is_null() || bits.is_null() {
                    DeleteDC(hdc_mem);
                    ReleaseDC(hwnd as HWND, hdc_window);
                    return Err("CreateDIBSection failed".into());
                }
                let old_bitmap = SelectObject(hdc_mem, bitmap);
                core::ptr::write_bytes(bits as *mut u8, 255, (w * h * 4) as usize);
                Ok(DibSurface {
                    hwnd,
                    hdc_window,
                    hdc_mem,
                    bitmap,
                    old_bitmap,
                    width: w,
                    height: h,
                })
            }
        }

        /// Copy an RGBA8 top-down buffer of identical dimensions to screen.
        pub fn present(&self, pixels: &[u8]) {
            let bi = top_down_info(self.width, self.height);
            // SAFETY: pixel buffer length is width*height*4 by construction.
            unsafe {
                SetDIBitsToDevice(
                    self.hdc_window,
                    0,
                    0,
                    self.width as u32,
                    self.height as u32,
                    0,
                    0,
                    0,
                    self.height as u32,
                    pixels.as_ptr() as *const core::ffi::c_void,
                    &bi,
                    DIB_RGB_COLORS,
                );
            }
        }
    }

    impl Drop for DibSurface {
        fn drop(&mut self) {
            // SAFETY: handles created in new() and not yet released.
            unsafe {
                SelectObject(self.hdc_mem, self.old_bitmap);
                DeleteObject(self.bitmap as *mut core::ffi::c_void);
                DeleteDC(self.hdc_mem);
                ReleaseDC(self.hwnd as HWND, self.hdc_window);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Events and app state
// ---------------------------------------------------------------------------
enum UserEvent {
    /// Layout result computed on cores 1+3 for UI generation `generation`.
    LayoutReady {
        generation: u64,
        result: Box<LayoutResult>,
    },
    PageReady {
        url: String,
        title: String,
        dom: Dom,
        sheet: Stylesheet,
        error: Option<String>,
        blocked: bool,
    },
    LoadFailed(String),
    /// Thermal governor transition (60 ↔ 30 FPS).
    FrameBudgetChanged(FrameBudget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Page,
    UrlEdit,
}

struct LoadedPage {
    url: String,
    dom: Dom,
    sheet: Stylesheet,
}

struct FreeWeb {
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Window>,
    surface: Option<gdi::DibSurface>,
    frame: Frame,
    scale: i64,
    mode: Mode,
    input: String,
    ctrl_down: bool,
    adblock: Arc<Mutex<AdBlocker>>,
    page: Option<LoadedPage>,
    layout_cache: Option<(Box<LayoutResult>, i64)>,
    /// UI generation counter: bumped on every layout request so stale
    /// worker results can be dropped on arrival. (Named `generation`: `gen`
    /// is a reserved keyword in Rust edition 2024.)
    generation: u64,
    scroll_y: i64,
    history: Vec<String>,
    hist_idx: usize,
    status: String,
    loading: bool,
    blocked_on_page: u64,
    mouse: (i64, i64),
    /// Current OS cursor shape (applied only when it changes).
    cur_icon: CursorIcon,
    /// Active scrollbar thumb drag: grab offset from the thumb top.
    scroll_drag: Option<i64>,
    alt_down: bool,
    /// Render tick driving the loading spinner.
    tick: u64,
    governor: perfmon::Governor,
    /// Mirrored from the governor for status display + pacing deadlines.
    budget: FrameBudget,
}

impl FreeWeb {
    fn new(proxy: EventLoopProxy<UserEvent>, governor: perfmon::Governor) -> Self {
        Self {
            proxy,
            window: None,
            surface: None,
            frame: Frame::new(8, 8),
            scale: 2,
            mode: Mode::Page,
            input: String::new(),
            ctrl_down: false,
            adblock: Arc::new(Mutex::new(AdBlocker::new())),
            page: None,
            layout_cache: None,
            generation: 0,
            scroll_y: 0,
            history: Vec::new(),
            hist_idx: 0,
            status: "Ready. Ctrl+L address | Ctrl+R reload | Ctrl+ +/-/0 zoom | Ctrl+C/V clipboard.".into(),
            loading: false,
            blocked_on_page: 0,
            mouse: (-1, -1),
            cur_icon: CursorIcon::Default,
            scroll_drag: None,
            alt_down: false,
            tick: 0,
            governor,
            budget: FrameBudget::Full,
        }
    }

    fn bar_h(&self) -> i64 {
        36 * self.scale
    }

    /// Work still in flight (network or layout) that justifies paced
    /// redraws instead of full idle.
    fn pending_work(&self) -> bool {
        self.loading || (self.page.is_some() && self.layout_cache.is_none())
    }

    /// Deadline for the next paced frame while a load or layout is in
    /// flight, or None for pure OS-wait idle (0% CPU). Governor budget
    /// transitions arrive as events and repaint once — an idle page never
    /// keeps ticking, even when the governor is throttled to 30 FPS.
    fn next_frame_time(&self) -> Option<Instant> {
        if !self.pending_work() {
            return None;
        }
        let budget = self.governor.current();
        Some(Instant::now() + Duration::from_secs_f64(budget.frame_time_ms() / 1000.0))
    }

    /// Dispatch layout to the dedicated cores-1+3 worker (never blocks the
    /// UI thread). Results for superseded generations are dropped.
    fn request_layout(&mut self) {
        let Some(page) = &self.page else { return };
        let vw = self.frame.width as i64;
        if vw <= 0 {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let dom = page.dom.clone();
        let sheet = page.sheet.clone();
        let proxy = self.proxy.clone();
        // Zoom (Ctrl+ +/-/0) lives in self.scale and must drive layout,
        // otherwise the viewport scale stays frozen at 2 forever.
        let layout_scale = self.scale.clamp(1, 4).max(1);
        let _ = affinity::spawn_pinned_pair([1, 3], "freeweb-layout", move || {
            let result = catch_layout_panic(dom, sheet, vw, layout_scale);
            let _ = proxy.send_event(UserEvent::LayoutReady {
                generation,
                result: Box::new(result),
            });
        });
    }

    fn max_scroll(&self) -> i64 {
        let content = self
            .layout_cache
            .as_ref()
            .map(|(l, _)| l.content_height)
            .unwrap_or(0);
        let visible = (self.frame.height as i64 - self.bar_h() - self.status_h()).max(1);
        (content - visible).max(0)
    }

    fn navigate(&mut self, raw: &str) {
        let url = normalize_input(raw);
        if url.is_empty() {
            return;
        }
        if self.history.get(self.hist_idx) == Some(&url) {
            self.start_fetch(url);
            return;
        }
        self.history.truncate(self.hist_idx + 1);
        self.history.push(url.clone());
        self.hist_idx = self.history.len() - 1;
        self.start_fetch(url);
    }

    fn start_fetch(&mut self, url: String) {
        self.loading = true;
        self.status = url.clone();
        self.mode = Mode::Page;
        self.scroll_y = 0;
        self.blocked_on_page = 0;
        self.request_redraw();

        let proxy = self.proxy.clone();
        let adblock = self.adblock.clone();
        let _ = affinity::spawn_pinned(2, "freeweb-fetch", move || {
            fetch_worker(url, adblock, proxy)
        });
    }

    fn go_back(&mut self) {
        if self.hist_idx > 0 {
            self.hist_idx -= 1;
            let url = self.history[self.hist_idx].clone();
            self.start_fetch(url);
        }
    }

    fn go_forward(&mut self) {
        if self.hist_idx + 1 < self.history.len() {
            self.hist_idx += 1;
            let url = self.history[self.hist_idx].clone();
            self.start_fetch(url);
        }
    }

    fn request_redraw(&mut self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn address_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        (
            68 * s,
            4 * s,
            (self.frame.width as i64 - 68 * s - 90 * s).max(60 * s),
            28 * s,
        )
    }

    fn go_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        (self.frame.width as i64 - 84 * s, 4 * s, 78 * s, 28 * s)
    }

    /// Dedicated bottom status strip height (never overlaps page content).
    fn status_h(&self) -> i64 {
        22 * self.scale
    }

    /// Vertical scrollbar geometry inside the content area:
    /// (track_x, track_y, track_w, track_h).
    fn scroll_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        let track_w = 10 * s;
        (
            self.frame.width as i64 - track_w,
            self.bar_h(),
            track_w,
            (self.frame.height as i64 - self.bar_h() - self.status_h()).max(0),
        )
    }

    /// Cursor icon for the current pointer position: hand over links and
    /// buttons, I-beam over the address box, default elsewhere.
    fn mouse_at_cursor(&self) -> CursorIcon {
        let (mx, my) = self.mouse;
        let s = self.scale;
        let hit_btn = |r: (i64, i64, i64, i64)| {
            mx >= r.0 && mx <= r.0 + r.2 && my >= r.1 && my <= r.1 + r.3
        };
        if self.scroll_drag.is_some() {
            return CursorIcon::Default;
        }
        if my <= self.bar_h() {
            if hit_btn((6 * s, 4 * s, 26 * s, 28 * s))
                || hit_btn((36 * s, 4 * s, 26 * s, 28 * s))
                || hit_btn(self.go_rect())
            {
                return CursorIcon::Hand;
            }
            let (ax, ay, aw, ah) = self.address_rect();
            if mx >= ax && mx <= ax + aw && my >= ay && my <= ay + ah {
                return CursorIcon::Text;
            }
            return CursorIcon::Default;
        }
        if self.max_scroll() > 0 && hit_btn(self.scroll_rect()) {
            return CursorIcon::Default;
        }
        if let Some((lay, _)) = &self.layout_cache {
            if hit_test(lay, mx, my - self.bar_h(), self.scroll_y).is_some() {
                return CursorIcon::Hand;
            }
        }
        CursorIcon::Default
    }

    // ----- painting ------------------------------------------------------
    fn render(&mut self) {
        let s = self.scale;
        let bar_h = self.bar_h();
        let w = self.frame.width as i64;
        self.tick = self.tick.wrapping_add(1);
        let (mx, my) = self.mouse;
        let hover = |r: (i64, i64, i64, i64)| {
            mx >= r.0 && mx <= r.0 + r.2 && my >= r.1 && my <= r.1 + r.3
        };

        // Page layer first; the chrome bar covers its top.
        if self.page.is_some() {
            match &self.layout_cache {
                Some((lay, _)) => paint(&mut self.frame, lay, self.scroll_y),
                None => {
                    self.frame.clear(Color::WHITE);
                    draw_text(
                        &mut self.frame,
                        "Layout...",
                        16 * s,
                        bar_h + 16 * s,
                        s.max(1),
                        Color { r: 120, g: 120, b: 130, a: 1.0 },
                    );
                }
            }
        } else {
            self.frame.clear(Color::WHITE);
            let mut y = bar_h + 24 * s;
            draw_text(&mut self.frame, "FreeWeb 2.0", 16 * s, y, 3 * s, Color::BLACK);
            y += 40 * s;
            draw_text(
                &mut self.frame,
                "Native Windows browser. No WebView2. No Chromium. No CEF. No Electron.",
                16 * s,
                y,
                s.max(1),
                Color { r: 90, g: 90, b: 100, a: 1.0 },
            );
            y += 24 * s;
            draw_text(
                &mut self.frame,
                "Own HTML5/CSS3 | Schannel TLS 1.2/1.3 | SSE4.2 adblock | core-pinned threads.",
                16 * s,
                y,
                s.max(1),
                Color { r: 90, g: 90, b: 100, a: 1.0 },
            );
            y += 24 * s;
            draw_text(
                &mut self.frame,
                "Ctrl+L address  Ctrl+R/F5 reload  Ctrl+ +/-/0 zoom  Alt+arrows history",
                16 * s,
                y,
                s.max(1),
                Color { r: 90, g: 90, b: 100, a: 1.0 },
            );
            y += 24 * s;
            draw_text(
                &mut self.frame,
                "Ctrl+C copy URL  Ctrl+V paste+go  Space/PgUp/PgDn/End scroll  draggable bar",
                16 * s,
                y,
                s.max(1),
                Color { r: 90, g: 90, b: 100, a: 1.0 },
            );
        }

        // Chrome bar.
        self.frame
            .fill_rect(0, 0, w, bar_h, Color { r: 228, g: 229, b: 233, a: 1.0 });
        self.frame
            .fill_rect(0, bar_h - s, w, s, Color { r: 200, g: 201, b: 206, a: 1.0 });

        // Back / forward: hover highlight plus a dimmed state when the
        // history entry they would reach does not exist.
        let (bx, by, bw, bh) = (6 * s, 4 * s, 26 * s, 28 * s);
        let (fx, fy, fw, fh) = (36 * s, 4 * s, 26 * s, 28 * s);
        let back_ok = self.hist_idx > 0;
        let fwd_ok = self.hist_idx + 1 < self.history.len();
        let btn_fill = |h: bool, ok: bool| match (h, ok) {
            (true, true) => Color { r: 214, g: 227, b: 245, a: 1.0 },
            (false, true) => Color { r: 244, g: 244, b: 246, a: 1.0 },
            (_, false) => Color { r: 234, g: 234, b: 236, a: 1.0 },
        };
        let btn_text = |ok: bool| {
            if ok {
                Color::BLACK
            } else {
                Color { r: 160, g: 160, b: 168, a: 1.0 }
            }
        };
        self.frame
            .fill_rect(bx, by, bw, bh, btn_fill(hover((bx, by, bw, bh)), back_ok));
        self.frame
            .stroke_rect(bx, by, bw, bh, s, Color { r: 180, g: 180, b: 190, a: 1.0 });
        draw_text(&mut self.frame, "<", bx + 9 * s, by + 7 * s, 2 * s, btn_text(back_ok));
        self.frame
            .fill_rect(fx, fy, fw, fh, btn_fill(hover((fx, fy, fw, fh)), fwd_ok));
        self.frame
            .stroke_rect(fx, fy, fw, fh, s, Color { r: 180, g: 180, b: 190, a: 1.0 });
        draw_text(&mut self.frame, ">", fx + 9 * s, fy + 7 * s, 2 * s, btn_text(fwd_ok));

        // Address box.
        let (ax, ay, aw, ah) = self.address_rect();
        self.frame.fill_rect(ax, ay, aw, ah, Color::WHITE);
        let border_c = if self.mode == Mode::UrlEdit {
            Color { r: 0, g: 120, b: 215, a: 1.0 }
        } else {
            Color { r: 170, g: 170, b: 180, a: 1.0 }
        };
        self.frame.stroke_rect(ax, ay, aw, ah, s, border_c);
        let shown = match (&self.page, self.mode) {
            (Some(p), Mode::Page) => p.url.clone(),
            _ => self.input.clone(),
        };
        let max_chars = (((aw - 8 * s) / (9 * s)).max(1)) as usize;
        let visible: String = shown
            .chars()
            .rev()
            .take(max_chars)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        draw_text(&mut self.frame, &visible, ax + 4 * s, ay + 9 * s, s, Color::BLACK);
        if self.mode == Mode::UrlEdit {
            let cx = ax + 4 * s + text_width(&visible, s) + s;
            self.frame.fill_rect(cx, ay + 5 * s, s, 18 * s, Color::BLACK);
        }

        // GO button (hover brightens).
        let (gx, gy, gw, gh) = self.go_rect();
        let go_fill = if hover((gx, gy, gw, gh)) {
            Color { r: 23, g: 138, b: 244, a: 1.0 }
        } else {
            Color { r: 0, g: 120, b: 215, a: 1.0 }
        };
        self.frame.fill_rect(gx, gy, gw, gh, go_fill);
        draw_text(&mut self.frame, "GO", gx + 26 * s, gy + 7 * s, 2 * s, Color::WHITE);

        // Scrollbar (only when the content overflows the view).
        let max_scroll = self.max_scroll();
        if max_scroll > 0 {
            let (tx, ty, tw, th) = self.scroll_rect();
            let view = (self.frame.height as i64 - bar_h - self.status_h()).max(1);
            let content = self
                .layout_cache
                .as_ref()
                .map(|(l, _)| l.content_height)
                .unwrap_or(view)
                .max(1);
            let thumb_h = ((view * view / content).max(8 * s)).min(th);
            let thumb_y = ty
                + ((th - thumb_h) as f64 * (self.scroll_y as f64 / max_scroll as f64)) as i64;
            self.frame
                .fill_rect(tx, ty, tw, th, Color { r: 238, g: 238, b: 240, a: 1.0 });
            self.frame
                .stroke_rect(tx, ty, tw, th, s, Color { r: 205, g: 205, b: 210, a: 1.0 });
            let thumb_c = if self.scroll_drag.is_some() {
                Color { r: 110, g: 110, b: 122, a: 1.0 }
            } else {
                Color { r: 172, g: 172, b: 180, a: 1.0 }
            };
            self.frame.fill_rect(
                tx + s,
                thumb_y + s,
                (tw - 2 * s).max(1),
                (thumb_h - 2 * s).max(1),
                thumb_c,
            );
        }

        // Status bar: dedicated bottom strip with state / link target on
        // the left and CPU + FPS budget + blocked counter on the right.
        let sh = self.status_h();
        let sy = (self.frame.height as i64 - sh).max(bar_h);
        self.frame
            .fill_rect(0, sy, w, sh, Color { r: 236, g: 237, b: 240, a: 1.0 });
        self.frame
            .fill_rect(0, sy, w, s, Color { r: 205, g: 206, b: 212, a: 1.0 });
        let blocked_total = self
            .adblock
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .blocked();
        let cpu = self.governor.cpu() as u32;
        let fps = self.budget.fps();
        let right = format!("CPU {cpu}% | {fps} FPS | {blocked_total} blocked");
        let left = if self.loading {
            let dots = ["", ".", "..", "..."][(self.tick % 4) as usize];
            format!("Loading{dots} | {}", self.status)
        } else if self.blocked_on_page > 0 {
            format!(
                "{} blocked on this page | {}",
                self.blocked_on_page, self.status
            )
        } else {
            self.status.clone()
        };
        let rw = text_width(&right, s.max(1));
        let max_left_chars = (((w - rw - 20 * s) / (9 * s)).max(1)) as usize;
        let left_trunc: String = left.chars().take(max_left_chars).collect();
        draw_text(
            &mut self.frame,
            &left_trunc,
            6 * s,
            sy + 5 * s,
            s.max(1),
            Color { r: 70, g: 70, b: 80, a: 1.0 },
        );
        draw_text(
            &mut self.frame,
            &right,
            (w - rw - 8 * s).max(0),
            sy + 5 * s,
            s.max(1),
            Color { r: 70, g: 70, b: 80, a: 1.0 },
        );
    }

    fn present(&self) {
        if let Some(surf) = &self.surface {
            surf.present(&self.frame.pixels);
        }
    }
}

/// Layout runs on worker cores under the Zero-Crash policy: a panic inside
/// layout is contained and reported as an empty result instead of killing
/// the process.
fn catch_layout_panic(
    dom: Dom,
    sheet: Stylesheet,
    viewport_w: i64,
    scale: i64,
) -> LayoutResult {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        layout(&dom, &sheet, &HashMap::new(), viewport_w, scale)
    }));
    match res {
        Ok(r) => r,
        Err(_) => LayoutResult {
            lines: Vec::new(),
            boxes: Vec::new(),
            content_height: 0,
            scale,
        },
    }
}

fn normalize_input(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.contains("://") {
        return t.to_string();
    }
    let first = t.split_whitespace().next().unwrap_or("");
    if first.contains('.') && !first.contains(' ') {
        format!("https://{t}")
    } else {
        format!("https://duckduckgo.com/?q={}", t.replace(' ', "+"))
    }
}

/// Network worker (core 2): adblock verdict → native HTTP GET → parse.
/// The whole body runs inside catch_unwind so no fault can abort the app.
fn fetch_worker(url: String, adblock: Arc<Mutex<AdBlocker>>, proxy: EventLoopProxy<UserEvent>) {
    let verdict = adblock
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .check(&url, "");
    if verdict == Verdict::Block {
        let _ = proxy.send_event(UserEvent::PageReady {
            url,
            title: "Blocked".into(),
            dom: Dom::new(),
            sheet: Stylesheet { rules: Vec::new() },
            error: Some(
                "Blocked before connect: signature matched (0 bytes downloaded).".into(),
            ),
            blocked: true,
        });
        return;
    }
    let fetch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| http::get(&url, &[])));
    let response = match fetch {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            let _ = proxy.send_event(UserEvent::LoadFailed(format!("{url}: {e}")));
            return;
        }
        Err(_) => {
            let _ = proxy.send_event(UserEvent::LoadFailed(format!(
                "{url}: internal fetch fault contained"
            )));
            return;
        }
    };
    if response.status != 200 {
        let _ = proxy.send_event(UserEvent::LoadFailed(format!(
            "HTTP {} from {url}",
            response.status
        )));
        return;
    }
    // Parse on the network core too (cheap); layout goes to cores 1+3.
    let parse = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let text = response.text();
        let d = parse_html(&text);
        let mut css_src = String::new();
        for id in d.iter() {
            let n = d.get(id).unwrap();
            if let NodeType::Element(el) = &n.kind {
                if el.tag == "style" {
                    for &c in &n.children {
                        if let NodeType::Text(t) = &d.get(c).unwrap().kind {
                            css_src.push_str(t);
                            css_src.push('\n');
                        }
                    }
                }
            }
        }
        let sheet = parse_stylesheet(&css_src);
        let title = d.title();
        (d, sheet, title)
    }));
    match parse {
        Ok((d, sheet, title)) => {
            let _ = proxy.send_event(UserEvent::PageReady {
                url,
                title,
                dom: d,
                sheet,
                error: None,
                blocked: false,
            });
        }
        Err(_) => {
            let _ = proxy.send_event(UserEvent::LoadFailed(format!(
                "{url}: document fault contained"
            )));
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 clipboard (Ctrl+C copy URL / Ctrl+V paste and go)
// ---------------------------------------------------------------------------
fn copy_clipboard(text: &str) -> bool {
    use gdi::*;
    if text.is_empty() {
        return false;
    }
    // SAFETY: the allocation, clipboard open/close and handle transfer
    // follow the documented CF_UNICODETEXT protocol; every error path
    // frees the allocation or closes the clipboard.
    unsafe {
        if OpenClipboard(core::ptr::null_mut()) == 0 {
            return false;
        }
        let mut ok = false;
        if EmptyClipboard() != 0 {
            let mut wide: Vec<u16> = text.encode_utf16().collect();
            wide.push(0);
            let bytes = wide.len() * 2;
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !h.is_null() {
                let p = GlobalLock(h) as *mut u16;
                if !p.is_null() {
                    core::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    GlobalUnlock(h);
                    if !SetClipboardData(CF_UNICODETEXT, h).is_null() {
                        ok = true; // ownership transferred to the clipboard
                    } else {
                        GlobalFree(h);
                    }
                } else {
                    GlobalFree(h);
                }
            }
        }
        CloseClipboard();
        ok
    }
}

fn paste_clipboard() -> Option<String> {
    use gdi::*;
    // SAFETY: clipboard opened and closed on every path; the received
    // handle is never freed (it belongs to the clipboard) and is only
    // read while locked.
    unsafe {
        if OpenClipboard(core::ptr::null_mut()) == 0 {
            return None;
        }
        let mut result: Option<String> = None;
        let h = GetClipboardData(CF_UNICODETEXT);
        if !h.is_null() {
            let p = GlobalLock(h) as *const u16;
            if !p.is_null() {
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let slice = core::slice::from_raw_parts(p, len);
                result = Some(String::from_utf16_lossy(slice));
                GlobalUnlock(h);
            }
        }
        CloseClipboard();
        result
    }
}

// ---------------------------------------------------------------------------
// ApplicationHandler
// ---------------------------------------------------------------------------
impl ApplicationHandler<UserEvent> for FreeWeb {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // UI thread stays on core 0 for the whole session.
        affinity::pin_current_thread(0);
        affinity::set_current_priority_highest();

        let attrs = Window::default_attributes()
            .with_title("FreeWeb — native lightweight browser")
            .with_inner_size(LogicalSize::new(1024.0, 700.0))
            .with_min_inner_size(LogicalSize::new(320.0, 240.0));
        match event_loop.create_window(attrs) {
            Ok(window) => {
                self.scale = window.scale_factor().round().clamp(1.0, 3.0) as i64;
                let size = window.inner_size();
                self.frame =
                    Frame::new(size.width.max(1) as usize, size.height.max(1) as usize);
                match window.window_handle() {
                    Ok(handle) => {
                        // WindowHandle::as_raw() is the non-deprecated
                        // accessor (HasRawWindowHandle is deprecated in
                        // winit 0.30) and returns the handle directly.
                        if let RawWindowHandle::Win32(h) = handle.as_raw()
                        {
                            match gdi::DibSurface::new(
                                h.hwnd.get() as isize,
                                size.width.max(1) as i32,
                                size.height.max(1) as i32,
                            ) {
                                Ok(surf) => self.surface = Some(surf),
                                Err(e) => self.status = format!("GDI init failed: {e}"),
                            }
                        }
                    }
                    Err(e) => self.status = format!("window handle error: {e}"),
                }
                self.window = Some(window);
            }
            Err(e) => {
                eprintln!("window creation failed: {e}");
                event_loop.exit();
            }
        }
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                let (w, h) = (size.width.max(1) as usize, size.height.max(1) as usize);
                if (self.frame.width, self.frame.height) != (w, h) {
                    self.frame.resize(w, h);
                    if let Some(win) = &self.window {
                        if let Ok(handle) = win.window_handle() {
                            if let RawWindowHandle::Win32(hw) = handle.as_raw()
                            {
                                if let Ok(surf) =
                                    gdi::DibSurface::new(hw.hwnd.get() as isize, w as i32, h as i32)
                                {
                                    self.surface = Some(surf);
                                }
                            }
                        }
                    }
                    self.layout_cache = None;
                    self.request_layout();
                    self.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.render();
                self.present();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = (position.x as i64, position.y as i64);
                let bar = self.bar_h();
                if let Some(grab_off) = self.scroll_drag {
                    // Thumb drag: keep the grab point under the cursor.
                    let (_, ty, _, th) = self.scroll_rect();
                    let view = (self.frame.height as i64 - bar - self.status_h()).max(1);
                    let content = self
                        .layout_cache
                        .as_ref()
                        .map(|(l, _)| l.content_height)
                        .unwrap_or(1)
                        .max(1);
                    let thumb_h = if content > view {
                        ((view * view / content).max(8 * self.scale)).min(th)
                    } else {
                        th
                    };
                    let travel = (th - thumb_h).max(1);
                    let frac =
                        ((self.mouse.1 - grab_off - ty) as f64 / travel as f64).clamp(0.0, 1.0);
                    self.scroll_y = (frac * self.max_scroll() as f64) as i64;
                    self.request_redraw();
                } else if self.mouse.1 > bar {
                    if let Some((lay, _)) = &self.layout_cache {
                        if let Some(href) =
                            hit_test(lay, self.mouse.0, self.mouse.1 - bar, self.scroll_y)
                        {
                            let base = self.page.as_ref().map(|p| p.url.as_str()).unwrap_or("");
                            let target = resolve_url(&href, base);
                            if self.status != target {
                                self.status = target;
                                self.request_redraw();
                            }
                        }
                    }
                }
                // Cursor shape update runs only when the icon changes.
                let icon = self.mouse_at_cursor();
                if icon != self.cur_icon {
                    self.cur_icon = icon;
                    if let Some(w) = &self.window {
                        w.set_cursor(icon);
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => {
                        (y * 3.0 * 12.0 * self.scale as f32) as i64
                    }
                    MouseScrollDelta::PixelDelta(p) => p.y as i64,
                };
                self.scroll_y = (self.scroll_y - amount).clamp(0, self.max_scroll());
                self.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (mx, my) = self.mouse;
                let s = self.scale;
                let hit_btn = |r: (i64, i64, i64, i64)| {
                    mx >= r.0 && mx <= r.0 + r.2 && my >= r.1 && my <= r.1 + r.3
                };
                // Scrollbar first: thumb starts a drag, track page-jumps.
                if my > self.bar_h() && self.max_scroll() > 0 {
                    let (tx, ty, tw, th) = self.scroll_rect();
                    if mx >= tx && mx <= tx + tw {
                        let view =
                            (self.frame.height as i64 - self.bar_h() - self.status_h()).max(1);
                        let content = self
                            .layout_cache
                            .as_ref()
                            .map(|(l, _)| l.content_height)
                            .unwrap_or(view)
                            .max(1);
                        let thumb_h = ((view * view / content).max(8 * s)).min(th);
                        let max_scroll = self.max_scroll();
                        let thumb_y = ty
                            + ((th - thumb_h) as f64
                                * (self.scroll_y as f64 / max_scroll as f64))
                                as i64;
                        if my >= thumb_y && my <= thumb_y + thumb_h {
                            self.scroll_drag = Some(my - thumb_y);
                        } else {
                            let travel = (th - thumb_h).max(1);
                            let frac = ((my - ty - thumb_h / 2) as f64 / travel as f64)
                                .clamp(0.0, 1.0);
                            self.scroll_y = (frac * max_scroll as f64) as i64;
                        }
                        self.request_redraw();
                        return;
                    }
                }
                if hit_btn((6 * s, 4 * s, 26 * s, 28 * s)) {
                    self.go_back();
                } else if hit_btn((36 * s, 4 * s, 26 * s, 28 * s)) {
                    self.go_forward();
                } else if hit_btn(self.go_rect()) {
                    let url = match (&self.page, self.mode) {
                        (Some(p), Mode::Page) => p.url.clone(),
                        _ => self.input.clone(),
                    };
                    self.navigate(&url);
                } else if my <= self.bar_h() {
                    let (ax, ay, aw, ah) = self.address_rect();
                    if mx >= ax && mx <= ax + aw && my >= ay && my <= ay + ah {
                        self.mode = Mode::UrlEdit;
                        self.input = self.page.as_ref().map(|p| p.url.clone()).unwrap_or_default();
                    }
                } else {
                    // Content-area link click: resolve borrow-free first.
                    let bar = self.bar_h();
                    let target = self
                        .layout_cache
                        .as_ref()
                        .and_then(|(lay, _)| hit_test(lay, mx, my - bar, self.scroll_y))
                        .map(|href| {
                            let base = self.page.as_ref().map(|p| p.url.as_str()).unwrap_or("");
                            resolve_url(&href, base)
                        });
                    if let Some(abs) = target {
                        self.navigate(&abs);
                    }
                }
                self.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                if self.scroll_drag.take().is_some() {
                    self.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                // winit 0.30: ModifiersChanged carries a `Modifiers` value
                // whose `.state()` returns ModifiersState; `control_key()`
                // / `alt_key()` are bool accessors on that state.
                let st = modifiers.state();
                self.ctrl_down = st.control_key();
                self.alt_down = st.alt_key();
            }
            WindowEvent::KeyboardInput { event: ke, .. } => {
                if ke.state == ElementState::Pressed {
                    self.handle_key(&ke);
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::LayoutReady { generation, result } => {
                // Only the newest generation applies.
                if generation == self.generation {
                    self.layout_cache = Some((result, self.frame.width as i64));
                    self.scroll_y = self.scroll_y.min(self.max_scroll());
                    self.request_redraw();
                }
            }
            UserEvent::PageReady { url, title, dom, sheet, error, blocked } => {
                self.loading = false;
                if blocked {
                    self.blocked_on_page += 1;
                    self.status = error.unwrap_or_default();
                    self.request_redraw();
                    return;
                }
                if let Some(e) = error {
                    self.status = e;
                    self.request_redraw();
                    return;
                }
                if let Some(w) = &self.window {
                    if !title.is_empty() {
                        w.set_title(&format!("{title} — FreeWeb"));
                    }
                }
                self.page = Some(LoadedPage { url, dom, sheet });
                self.layout_cache = None;
                self.request_layout();
                self.status = "Loaded.".into();
                self.request_redraw();
            }
            UserEvent::LoadFailed(msg) => {
                self.loading = false;
                self.status = format!("Error: {msg}");
                self.request_redraw();
            }
            UserEvent::FrameBudgetChanged(b) => {
                self.budget = b;
                self.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Adaptive pacing: while work is in flight (or the thermal governor
        // is active) wake once per frame-time; otherwise park in Wait —
        // 0% CPU with zero busy-waiting.
        match self.next_frame_time() {
            Some(deadline) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                self.request_redraw();
            }
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

impl FreeWeb {
    fn handle_key(&mut self, ke: &KeyEvent) {
        let ctrl = self.ctrl_down;
        match self.mode {
            Mode::Page => match ke.physical_key {
                PhysicalKey::Code(KeyCode::PageDown) => {
                    self.scroll_y = (self.scroll_y + 12 * 12 * self.scale).min(self.max_scroll());
                }
                PhysicalKey::Code(KeyCode::PageUp) => {
                    self.scroll_y = (self.scroll_y - 12 * 12 * self.scale).max(0);
                }
                PhysicalKey::Code(KeyCode::Home) => self.scroll_y = 0,
                PhysicalKey::Code(KeyCode::End) => self.scroll_y = self.max_scroll(),
                PhysicalKey::Code(KeyCode::ArrowDown) => {
                    self.scroll_y = (self.scroll_y + 3 * 12 * self.scale).min(self.max_scroll());
                }
                PhysicalKey::Code(KeyCode::ArrowUp) => {
                    self.scroll_y = (self.scroll_y - 3 * 12 * self.scale).max(0);
                }
                PhysicalKey::Code(KeyCode::ArrowLeft) if self.alt_down => self.go_back(),
                PhysicalKey::Code(KeyCode::ArrowRight) if self.alt_down => self.go_forward(),
                PhysicalKey::Code(KeyCode::Space) if !ctrl => {
                    self.scroll_y = (self.scroll_y + 12 * 12 * self.scale).min(self.max_scroll());
                }
                PhysicalKey::Code(KeyCode::F5) if !ctrl => {
                    if let Some(p) = &self.page {
                        let u = p.url.clone();
                        self.start_fetch(u);
                    }
                }
                PhysicalKey::Code(KeyCode::Backspace) if !ctrl => self.go_back(),
                _ => {
                    if ctrl {
                        // Physical-key shortcuts are layout- and IME-proof.
                        match ke.physical_key {
                            PhysicalKey::Code(KeyCode::KeyL) => {
                                self.mode = Mode::UrlEdit;
                                self.input = self
                                    .page
                                    .as_ref()
                                    .map(|p| p.url.clone())
                                    .unwrap_or_default();
                            }
                            PhysicalKey::Code(KeyCode::KeyR) => {
                                if let Some(p) = &self.page {
                                    let u = p.url.clone();
                                    self.start_fetch(u);
                                }
                            }
                            PhysicalKey::Code(KeyCode::Equal)
                            | PhysicalKey::Code(KeyCode::NumpadAdd) => {
                                self.scale = (self.scale + 1).min(4);
                                self.layout_cache = None;
                                self.request_layout();
                            }
                            PhysicalKey::Code(KeyCode::Minus)
                            | PhysicalKey::Code(KeyCode::NumpadSubtract) => {
                                self.scale = (self.scale - 1).max(1);
                                self.layout_cache = None;
                                self.request_layout();
                            }
                            PhysicalKey::Code(KeyCode::Digit0) | PhysicalKey::Code(KeyCode::Numpad0) => {
                                self.scale = 2;
                                self.layout_cache = None;
                                self.request_layout();
                            }
                            PhysicalKey::Code(KeyCode::KeyC) => {
                                let url = self
                                    .page
                                    .as_ref()
                                    .map(|p| p.url.clone())
                                    .unwrap_or_else(|| self.input.clone());
                                if copy_clipboard(&url) {
                                    self.status = "URL copied to clipboard.".into();
                                }
                            }
                            PhysicalKey::Code(KeyCode::KeyV) => {
                                if let Some(text) = paste_clipboard() {
                                    self.mode = Mode::UrlEdit;
                                    self.input = text;
                                    self.status = "Pasted. Press Enter to go.".into();
                                } else {
                                    self.status = "Clipboard has no text.".into();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            },
            Mode::UrlEdit => {
                // Clipboard shortcuts work while editing the address bar.
                if ctrl {
                    match ke.physical_key {
                        PhysicalKey::Code(KeyCode::KeyV) => {
                            if let Some(text) = paste_clipboard() {
                                self.input = text;
                            } else {
                                self.status = "Clipboard has no text.".into();
                            }
                            self.request_redraw();
                            return;
                        }
                        PhysicalKey::Code(KeyCode::KeyC) | PhysicalKey::Code(KeyCode::KeyX) => {
                            if copy_clipboard(&self.input) {
                                self.status = "Address copied to clipboard.".into();
                            }
                            self.request_redraw();
                            return;
                        }
                        _ => {}
                    }
                }
                match &ke.logical_key {
                    Key::Named(NamedKey::Enter) => {
                        let t = self.input.clone();
                        self.navigate(&t);
                    }
                    Key::Named(NamedKey::Escape) => self.mode = Mode::Page,
                    Key::Named(NamedKey::Backspace) => {
                        self.input.pop();
                    }
                    Key::Named(NamedKey::Space) => self.input.push(' '),
                    Key::Character(_) => {
                        if let Some(text) = &ke.text {
                            for ch in text.chars() {
                                if !ch.is_control() {
                                    self.input.push(ch);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            },
        }
        self.request_redraw();
    }
}

// ---------------------------------------------------------------------------
// Entry point + Zero-Crash session supervisor
// ---------------------------------------------------------------------------
fn main() {
    // Zero-Crash Engine: the whole session runs inside catch_unwind. A
    // panic anywhere on the UI thread is contained; the loop restarts and
    // the process never tears down abruptly.
    let result = std::panic::catch_unwind(run_session);
    if let Err(payload) = result {
        eprintln!(
            "FreeWeb: session fault contained ({})",
            payload_message(&payload)
        );
        // One guarded retry with a fresh event loop.
        let retry = std::panic::catch_unwind(run_session);
        if retry.is_err() {
            eprintln!("FreeWeb: second fault contained; exiting cleanly.");
        }
    }
}

fn payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn run_session() {
    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event()
        .build()
        .expect("FreeWeb: failed to create event loop");
    let proxy = event_loop.create_proxy();
    // PDH governor on core 2 pushes budget transitions straight into the
    // UI loop — no polling anywhere.
    let governor = {
        let notify_proxy = proxy.clone();
        perfmon::spawn_monitor(move |b| {
            let _ = notify_proxy.send_event(UserEvent::FrameBudgetChanged(b));
        })
    };
    let mut app = FreeWeb::new(proxy, governor);
    event_loop.set_control_flow(ControlFlow::Wait);
    let _ = event_loop.run_app(&mut app);
}
