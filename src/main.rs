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
//!
//! UI controls: back / forward / reload / GO buttons in the chrome bar with
//! hover highlight, dimmed disabled state and hover tooltips; a focusable
//! address box with a blinking caret; a draggable scrollbar with grab
//! cursors; an indeterminate loading sweep along the bar edge; and -/+ zoom
//! controls plus a live CPU/FPS/blocked readout in the status strip.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adblock;
mod affinity;
mod css;
mod dom;
mod font8x8;
mod net;
mod perfmon;
mod renderer;
mod simd_hash;
mod storage;
mod utils;

use adblock::{AdBlocker, Verdict};
use css::{parse_stylesheet, Color, Stylesheet};
use dom::{parse_html, Dom, NodeType};
use perfmon::FrameBudget;
use renderer::{
    draw_text, find_matches, hit_test, layout, paint, resolve_url, text_width, Frame, LayoutResult,
};
use std::sync::atomic::{AtomicBool, Ordering};
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

        /// Copy a BGRA8 top-down buffer of identical dimensions to screen.
        /// GDI 32bpp BI_RGB reads pixel bytes as (B,G,R,reserved), which is
        /// exactly the Frame's storage order, so this is a raw blit.
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
        /// Fetch token of the load that produced this page; a result from a
        /// load the user stopped is discarded on arrival.
        fetch_gen: u64,
        url: String,
        /// Resolution base for relative references on this page, honouring
        /// `<base href>` when the document declares one.
        base: String,
        title: String,
        dom: Dom,
        sheet: Stylesheet,
        error: Option<String>,
        blocked: bool,
    },
    /// Automatic navigation requested by the document itself
    /// (`<meta http-equiv="refresh">`).
    Navigate(String),
    LoadFailed(u64, String),
    /// Thermal governor transition (60 ↔ 30 FPS).
    FrameBudgetChanged(FrameBudget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Page,
    UrlEdit,
    /// Find-in-page bar has the keyboard focus.
    Find,
}

/// Search engine integrated in the address bar. Google is the default.
///
/// Only an ASCII glyph is shown on the toolbar (the bitmap font has no
/// non-ASCII coverage); the full name lives in the tooltip and the status
/// line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchEngine {
    Google,
    DuckDuckGo,
    Bing,
}

impl SearchEngine {
    /// Toolbar glyph: one ASCII letter per engine.
    fn glyph(self) -> &'static str {
        match self {
            SearchEngine::Google => "G",
            SearchEngine::DuckDuckGo => "D",
            SearchEngine::Bing => "B",
        }
    }

    fn name(self) -> &'static str {
        match self {
            SearchEngine::Google => "Google",
            SearchEngine::DuckDuckGo => "DuckDuckGo",
            SearchEngine::Bing => "Bing",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            SearchEngine::Google => "Search: Google (click to cycle)",
            SearchEngine::DuckDuckGo => "Search: DuckDuckGo (click to cycle)",
            SearchEngine::Bing => "Search: Bing (click to cycle)",
        }
    }

    /// Next engine in the click cycle.
    fn next(self) -> Self {
        match self {
            SearchEngine::Google => SearchEngine::DuckDuckGo,
            SearchEngine::DuckDuckGo => SearchEngine::Bing,
            SearchEngine::Bing => SearchEngine::Google,
        }
    }

    /// Stable persistence index (storage layer stores this u8).
    fn index(self) -> u8 {
        self as u8
    }

    /// Map a persisted index back to an engine; unknown values fall back
    /// to Google, matching `Preferences::sanitized`.
    fn from_index(idx: u8) -> Self {
        match idx {
            1 => SearchEngine::DuckDuckGo,
            2 => SearchEngine::Bing,
            _ => SearchEngine::Google,
        }
    }
}

struct LoadedPage {
    url: String,
    /// Base for resolving relative links and hover targets.
    base: String,
    dom: Dom,
    sheet: Stylesheet,
}

struct FreeWeb {
    /// Shared persisted preferences (engine, zoom, history) guarded by a
    /// mutex; the storage worker snapshots and flushes them when dirty.
    prefs: Arc<std::sync::Mutex<storage::Preferences>>,
    /// Set on every preference change; the saver thread debounces writes.
    prefs_dirty: Arc<AtomicBool>,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Window>,
    surface: Option<gdi::DibSurface>,
    frame: Frame,
    scale: i64,
    mode: Mode,
    input: String,
    /// Engine used when the address box receives a search query.
    engine: SearchEngine,
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
    /// `<meta http-equiv="refresh">` hops followed in a row; reset on every
    /// user-initiated navigation so a redirect loop cannot spin forever.
    auto_hops: u32,
    /// In-flight load token: bumped by every navigation and by Stop.
    fetch_gen: u64,
    /// Find-in-page query and 1-based index of the current match.
    find: String,
    find_pos: usize,
    mouse: (i64, i64),
    /// Current OS cursor shape (applied only when it changes).
    cur_icon: CursorIcon,
    /// Active scrollbar thumb drag: grab offset from the thumb top.
    scroll_drag: Option<i64>,
    alt_down: bool,
    /// Render tick driving the loading spinner and the caret blink.
    tick: u64,
    /// Smooth scrolling: the offset the page eases toward every tick.
    scroll_target: i64,
    governor: perfmon::Governor,
    /// Mirrored from the governor for status display + pacing deadlines.
    budget: FrameBudget,
}

impl FreeWeb {
    fn new(proxy: EventLoopProxy<UserEvent>, governor: perfmon::Governor) -> Self {
        // Storage layer: load persisted preferences once at startup.
        let loaded = storage::load();
        let scale = loaded.zoom;
        let engine = SearchEngine::from_index(loaded.engine);
        let history: Vec<String> = loaded.history.clone();
        let hist_idx = history.len().saturating_sub(1);
        let prefs = Arc::new(std::sync::Mutex::new(loaded));
        let prefs_dirty = Arc::new(AtomicBool::new(false));
        storage::prefs::spawn_saver(prefs.clone(), prefs_dirty.clone());
        Self {
            proxy,
            window: None,
            surface: None,
            frame: Frame::new(8, 8),
            scale,
            mode: Mode::Page,
            input: String::new(),
            engine,
            prefs,
            prefs_dirty,
            ctrl_down: false,
            adblock: Arc::new(Mutex::new(AdBlocker::new())),
            page: None,
            layout_cache: None,
            generation: 0,
            scroll_y: 0,
            history,
            hist_idx,
            status: "Ready. Type words to search, a host to visit, or press Alt+Home for the start page.".into(),
            loading: false,
            blocked_on_page: 0,
            auto_hops: 0,
            fetch_gen: 0,
            find: String::new(),
            find_pos: 0,
            mouse: (-1, -1),
            cur_icon: CursorIcon::Default,
            scroll_drag: None,
            alt_down: false,
            tick: 0,
            scroll_target: 0,
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

    /// User-initiated navigation (address bar, GO, link, history): resets the
    /// automatic-redirect budget first.
    fn navigate(&mut self, raw: &str) {
        self.auto_hops = 0;
        self.navigate_inner(raw);
    }

    /// Navigation that leaves the automatic-redirect budget alone; used by
    /// the `<meta http-equiv="refresh">` follow-up path.
    fn navigate_inner(&mut self, raw: &str) {
        let url = utils::sanitize::normalize_input(raw, self.engine.index());
        if url.is_empty() {
            return;
        }
        // Built-in pages are served from memory, never over the network.
        if url.starts_with("about:") {
            self.show_internal(&url);
            return;
        }
        if self.history.get(self.hist_idx) == Some(&url) {
            self.start_fetch(url);
            return;
        }
        self.history.truncate(self.hist_idx + 1);
        self.history.push(url.clone());
        self.hist_idx = self.history.len() - 1;
        self.record_visit(&url);
        self.start_fetch(url);
    }

    fn start_fetch(&mut self, url: String) {
        // Every load carries a token. Stop bumps it, so the worker's late
        // result is dropped instead of overwriting the page after the fact.
        self.fetch_gen = self.fetch_gen.wrapping_add(1);
        let token = self.fetch_gen;
        self.loading = true;
        self.status = url.clone();
        self.mode = Mode::Page;
        self.scroll_y = 0;
        self.blocked_on_page = 0;
        self.request_redraw();

        let proxy = self.proxy.clone();
        let adblock = self.adblock.clone();
        let _ = affinity::spawn_pinned(2, "freeweb-fetch", move || {
            fetch_worker(url, adblock, token, proxy)
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

    /// Reload the current page, or re-run whatever is typed in the address
    /// box when no page has loaded yet.
    fn reload(&mut self) {
        let url = match &self.page {
            Some(p) => p.url.clone(),
            None => self.input.clone(),
        };
        if !url.is_empty() {
            self.start_fetch(url);
        }
    }

    /// Stop the in-flight load. A blocking worker cannot be aborted, so its
    /// result is discarded instead: the token it was given no longer matches.
    fn stop(&mut self) {
        if !self.loading {
            return;
        }
        self.fetch_gen = self.fetch_gen.wrapping_add(1);
        self.loading = false;
        self.status = "Stopped.".into();
        self.request_redraw();
    }

    /// Home button / Alt+Home: the built-in start page.
    fn go_home(&mut self) {
        self.navigate(ABOUT_HOME);
    }

    /// Serve a built-in page (`about:home`) from memory, with no network and
    /// no worker: parsing a few hundred bytes on the UI thread is free.
    fn show_internal(&mut self, url: &str) {
        let (title, html) = if url == ABOUT_HOME {
            (START_PAGE_TITLE, START_PAGE_HTML)
        } else {
            ("Unknown page", ABOUT_UNKNOWN_HTML)
        };
        if self.history.get(self.hist_idx).map(|h| h.as_str()) != Some(url) {
            self.history.truncate(self.hist_idx + 1);
            self.history.push(url.to_string());
            self.hist_idx = self.history.len() - 1;
        }
        self.fetch_gen = self.fetch_gen.wrapping_add(1); // cancel any load in flight
        self.loading = false;
        self.mode = Mode::Page;
        self.scroll_y = 0;
        self.blocked_on_page = 0;
        self.layout_cache = None;
        let dom = parse_html(html);
        let sheet = parse_stylesheet(START_PAGE_CSS);
        self.page = Some(LoadedPage {
            url: url.to_string(),
            base: url.to_string(),
            dom,
            sheet,
        });
        if let Some(w) = &self.window {
            w.set_title(&format!("{title} - FreeWeb"));
        }
        self.status = format!("{url} - built-in page (no network request).");
        self.request_layout();
        self.request_redraw();
    }

    /// Advance find-in-page to the next match and scroll it into view.
    fn find_next(&mut self) {
        let query = self.find.trim().to_string();
        if query.is_empty() {
            return;
        }
        let hits = match &self.layout_cache {
            Some((lay, _)) => find_matches(lay, &query),
            None => Vec::new(),
        };
        if hits.is_empty() {
            self.find_pos = 0;
            self.status = format!("No matches for \"{query}\".");
            self.request_redraw();
            return;
        }
        let idx = self.find_pos % hits.len();
        self.find_pos = idx + 1;
        let visible = (self.frame.height as i64 - self.bar_h() - self.status_h()).max(1);
        self.scroll_y = (hits[idx] - visible / 3).clamp(0, self.max_scroll());
        self.status = format!("Match {} of {} for \"{query}\".", idx + 1, hits.len());
        self.request_redraw();
    }

    /// Open the find bar and take the keyboard.
    fn open_find(&mut self) {
        self.mode = Mode::Find;
        self.find.clear();
        self.find_pos = 0;
        self.request_redraw();
    }

    /// Close the find bar and drop the highlights.
    fn close_find(&mut self) {
        self.mode = Mode::Page;
        self.find.clear();
        self.find_pos = 0;
        self.request_redraw();
    }

    /// Transport scheme of the active page, for the security indicator.
    fn scheme(&self) -> &'static str {
        match &self.page {
            Some(p) if p.url.starts_with("https://") => "https",
            Some(p) if p.url.starts_with("http://") => "http",
            Some(_) => "local",
            None => "blank",
        }
    }

    fn scheme_label(&self) -> &'static str {
        match self.scheme() {
            "https" => "https",
            "http" => "http (not secure)",
            "local" => "about",
            _ => "no page",
        }
    }

    /// Colour of the address-box security strip: green when the connection is
    /// encrypted, amber when it is not. The bitmap font has no padlock.
    fn scheme_color(&self) -> Color {
        match self.scheme() {
            "https" => Color { r: 26, g: 159, b: 84, a: 1.0 },
            "http" => Color { r: 205, g: 140, b: 20, a: 1.0 },
            "local" => Color { r: 110, g: 110, b: 122, a: 1.0 },
            _ => Color { r: 205, g: 205, b: 210, a: 1.0 },
        }
    }

    fn request_redraw(&mut self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Put the caret in the address box, pre-filled with the current URL.
    /// Shared by Ctrl+L / Ctrl+K / Ctrl+E / F6 and by clicking the box.
    fn focus_address(&mut self) {
        self.mode = Mode::UrlEdit;
        self.input = self
            .page
            .as_ref()
            .map(|p| p.url.clone())
            .unwrap_or_default();
    }

    /// Back button rect.
    fn back_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        (6 * s, 4 * s, 26 * s, 28 * s)
    }

    /// Forward button rect.
    fn fwd_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        (36 * s, 4 * s, 26 * s, 28 * s)
    }

    /// Home button rect: the built-in start page (about:home).
    fn home_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        (66 * s, 4 * s, 26 * s, 28 * s)
    }

    /// Reload / Stop slot: one square holds both, like every desktop browser.
    fn reload_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        (96 * s, 4 * s, 26 * s, 28 * s)
    }

    /// Search-engine selector, between the reload slot and the address box.
    fn engine_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        (126 * s, 4 * s, 30 * s, 28 * s)
    }

    /// The address box doubles as the search bar, so it starts after the
    /// engine selector and runs up to the GO button.
    fn address_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        let x = 160 * s;
        let right = self.frame.width as i64 - 92 * s;
        (x, 4 * s, (right - x).max(60 * s), 28 * s)
    }

    fn go_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        (self.frame.width as i64 - 84 * s, 4 * s, 78 * s, 28 * s)
    }

    /// Zoom-out control pinned to the right end of the status strip.
    fn zoom_out_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        let b = 18 * s;
        (
            self.frame.width as i64 - (2 * b + 6 * s),
            self.frame.height as i64 - self.status_h() + 2 * s,
            b,
            b,
        )
    }

    /// Zoom-in control at the very corner of the status strip.
    fn zoom_in_rect(&self) -> (i64, i64, i64, i64) {
        let s = self.scale;
        let b = 18 * s;
        (
            self.frame.width as i64 - (b + 2 * s),
            self.frame.height as i64 - self.status_h() + 2 * s,
            b,
            b,
        )
    }

    /// Change zoom by `delta` steps (clamped 100%–400%) and relayout.
    /// Shared by the status-strip buttons and the Ctrl +/- shortcuts.
    fn set_zoom(&mut self, delta: i64) {
        let next = (self.scale + delta).clamp(1, 4);
        if next != self.scale {
            self.scale = next;
            self.persist_prefs();
            self.layout_cache = None;
            self.request_layout();
            self.request_redraw();
        }
    }

    /// Push a URL into the shared persisted history (bounded, deduped by
    /// the storage layer) and mark preferences dirty.
    fn record_visit(&mut self, url: &str) {
        if let Ok(mut p) = self.prefs.lock() {
            p.push_history(url);
        }
        self.prefs_dirty.store(true, Ordering::Relaxed);
    }

    /// Copy runtime engine/zoom state into shared preferences and mark
    /// them dirty; the saver thread flushes within a second.
    fn persist_prefs(&mut self) {
        if let Ok(mut p) = self.prefs.lock() {
            p.engine = self.engine.index();
            p.zoom = self.scale;
            p.touched = true;
        }
        self.prefs_dirty.store(true, Ordering::Relaxed);
    }

    /// Chrome-bar buttons as (label, tooltip, rect): the single source of
    /// truth for painting, hit-testing, cursor shape and tooltips.
    fn chrome_buttons(&self) -> [(&'static str, &'static str, (i64, i64, i64, i64)); 6] {
        // The reload slot doubles as Stop while a load is in flight.
        let (reload_glyph, reload_tip) = if self.loading {
            ("X", "Stop loading (Esc)")
        } else {
            ("R", "Reload (Ctrl+R / F5)")
        };
        [
            ("<", "Back (Alt+Left)", self.back_rect()),
            (">", "Forward (Alt+Right)", self.fwd_rect()),
            ("H", "Start page (Alt+Home)", self.home_rect()),
            (reload_glyph, reload_tip, self.reload_rect()),
            (self.engine.glyph(), self.engine.tooltip(), self.engine_rect()),
            ("GO", "Go to the address (Enter)", self.go_rect()),
        ]
    }

    /// Tooltip text plus anchor rect for the button under the pointer.
    fn hovered_tooltip(&self) -> Option<(&'static str, (i64, i64, i64, i64))> {
        let (mx, my) = self.mouse;
        self.chrome_buttons()
            .into_iter()
            .find(|(_, _, r)| mx >= r.0 && mx <= r.0 + r.2 && my >= r.1 && my <= r.1 + r.3)
            .map(|(_, tip, r)| (tip, r))
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

    /// Cursor icon for the current pointer position: pointer over links and
    /// buttons, I-beam over the address box, grab handles over the
    /// scrollbar, default elsewhere.
    ///
    /// winit 0.30 renamed the "hand" cursor to `CursorIcon::Pointer`, so the
    /// old `CursorIcon::Hand` no longer exists.
    fn mouse_at_cursor(&self) -> CursorIcon {
        let (mx, my) = self.mouse;
        let hit = |r: (i64, i64, i64, i64)| {
            mx >= r.0 && mx <= r.0 + r.2 && my >= r.1 && my <= r.1 + r.3
        };
        if self.scroll_drag.is_some() {
            return CursorIcon::Grabbing;
        }
        if my <= self.bar_h() {
            if self.chrome_buttons().iter().any(|(_, _, r)| hit(*r)) {
                return CursorIcon::Pointer;
            }
            let (ax, ay, aw, ah) = self.address_rect();
            if mx >= ax && mx <= ax + aw && my >= ay && my <= ay + ah {
                return CursorIcon::Text;
            }
            return CursorIcon::Default;
        }
        if self.max_scroll() > 0 && hit(self.scroll_rect()) {
            return CursorIcon::Grab;
        }
        if hit(self.zoom_out_rect()) || hit(self.zoom_in_rect()) {
            return CursorIcon::Pointer;
        }
        if let Some((lay, _)) = &self.layout_cache {
            if hit_test(lay, mx, my - self.bar_h(), self.scroll_y).is_some() {
                return CursorIcon::Pointer;
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
                Some((lay, _)) => {
                    // Matches are highlighted only while the find bar is open.
                    let needle = if self.mode == Mode::Find {
                        Some(self.find.as_str())
                    } else {
                        None
                    };
                    paint(&mut self.frame, lay, self.scroll_y, needle);
                }
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
                "Ctrl+L / F6 address  Ctrl+F find in page  Ctrl+R / F5 reload  Esc stop",
                16 * s,
                y,
                s.max(1),
                Color { r: 90, g: 90, b: 100, a: 1.0 },
            );
            y += 24 * s;
            draw_text(
                &mut self.frame,
                "Alt+Home start page  Alt+Left/Right history  Ctrl+plus / Ctrl+minus / Ctrl+0 zoom",
                16 * s,
                y,
                s.max(1),
                Color { r: 90, g: 90, b: 100, a: 1.0 },
            );
            y += 24 * s;
            draw_text(
                &mut self.frame,
                "Toolbar: < back  > forward  H home  R reload / X stop  G engine  GO    Status: -/+ zoom",
                16 * s,
                y,
                s.max(1),
                Color { r: 90, g: 90, b: 100, a: 1.0 },
            );
            y += 24 * s;
            draw_text(
                &mut self.frame,
                "Press the H button (or Alt+Home) for the built-in start page.",
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

        // Back / forward / reload: one loop drives hover highlight, the
        // dimmed state for actions that cannot run yet, and the glyph.
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
        for (label, _tip, rect) in self.chrome_buttons() {
            if label == "GO" {
                continue; // primary action, painted below with its own style
            }
            let ok = match label {
                "<" => back_ok,
                ">" => fwd_ok,
                // Reload needs something to fetch.
                "R" => self.page.is_some() || !self.input.is_empty(),
                // The search-engine selector is always usable.
                _ => true,
            };
            self.frame
                .fill_rect(rect.0, rect.1, rect.2, rect.3, btn_fill(hover(rect), ok));
            self.frame.stroke_rect(
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                s,
                Color { r: 180, g: 180, b: 190, a: 1.0 },
            );
            let lw = text_width(label, 2 * s);
            let lx = rect.0 + ((rect.2 - lw) / 2).max(0);
            let ly = rect.1 + ((rect.3 - 16 * s) / 2).max(0);
            draw_text(&mut self.frame, label, lx, ly, 2 * s, btn_text(ok));
        }

        // Address box, with the security strip inside its left edge.
        let (ax, ay, aw, ah) = self.address_rect();
        self.frame.fill_rect(ax, ay, aw, ah, Color::WHITE);
        let strip_c = self.scheme_color();
        self.frame
            .fill_rect(ax + s, ay + s, 3 * s, (ah - 2 * s).max(1), strip_c);
        let text_x = ax + 8 * s;
        let border_c = if self.mode == Mode::UrlEdit {
            Color { r: 0, g: 120, b: 215, a: 1.0 }
        } else {
            Color { r: 170, g: 170, b: 180, a: 1.0 }
        };
        self.frame.stroke_rect(ax, ay, aw, ah, s, border_c);
        // Empty and unfocused: say what the box is for instead of leaving a
        // blank white rectangle.
        let (shown, placeholder) = match (&self.page, self.mode) {
            (Some(p), Mode::Page) => (p.url.clone(), false),
            (None, Mode::Page) => (
                format!("Search {} or type a URL", self.engine.name()),
                true,
            ),
            _ => (self.input.clone(), false),
        };
        let max_chars = (((aw - 14 * s) / (9 * s)).max(1)) as usize;
        let visible: String = if self.mode == Mode::UrlEdit {
            shown
                .chars()
                .rev()
                .take(max_chars)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        } else {
            shown.chars().take(max_chars).collect()
        };
        let text_c = if placeholder {
            Color { r: 150, g: 150, b: 158, a: 1.0 }
        } else {
            Color::BLACK
        };
        draw_text(&mut self.frame, &visible, text_x, ay + 9 * s, s, text_c);
        if self.mode == Mode::UrlEdit {
            let cx = text_x + text_width(&visible, s) + s;
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
        let go_w = text_width("GO", 2 * s);
        draw_text(
            &mut self.frame,
            "GO",
            gx + ((gw - go_w) / 2).max(0),
            gy + ((gh - 16 * s) / 2).max(0),
            2 * s,
            Color::WHITE,
        );

        // Loading indicator: indeterminate sweep along the chrome bar's
        // bottom edge, driven by the render tick. No timers, no threads and
        // no busy-wait — it stops the moment `loading` clears.
        if self.loading {
            let seg = (w / 4).max(24 * s);
            let span = (w + seg).max(1) as u64;
            let x = (self.tick % span) as i64 - seg;
            self.frame.fill_rect(
                x,
                bar_h - 2 * s,
                seg,
                2 * s,
                Color { r: 0, g: 120, b: 215, a: 1.0 },
            );
        }

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

        // Find bar: an overlay row under the chrome bar, with a live match
        // counter. The highlighting itself happens in the painter.
        if self.mode == Mode::Find {
            let fh = 26 * s;
            let fy = bar_h;
            self.frame.fill_rect(0, fy, w, fh, Color { r: 250, g: 250, b: 252, a: 1.0 });
            self.frame
                .fill_rect(0, fy + fh - s, w, s, Color { r: 200, g: 201, b: 206, a: 1.0 });
            let hits = self
                .layout_cache
                .as_ref()
                .map(|(lay, _)| find_matches(lay, &self.find).len())
                .unwrap_or(0);
            let prompt = "Find:";
            draw_text(&mut self.frame, prompt, 8 * s, fy + 9 * s, s, Color::BLACK);
            let qx = 8 * s + text_width(prompt, s) + 4 * s;
            let (query_shown, query_color) = if self.find.is_empty() {
                (
                    "type and press Enter",
                    Color { r: 150, g: 150, b: 158, a: 1.0 },
                )
            } else {
                (self.find.as_str(), Color::BLACK)
            };
            draw_text(&mut self.frame, query_shown, qx, fy + 9 * s, s, query_color);
            if !self.find.is_empty() {
                let cx = qx + text_width(&self.find, s) + s;
                self.frame.fill_rect(cx, fy + 6 * s, s, 14 * s, Color::BLACK);
            }
            let count = if self.find.is_empty() {
                String::new()
            } else if hits == 0 {
                "no matches".to_string()
            } else {
                format!("{} of {}", self.find_pos.clamp(1, hits), hits)
            };
            if !count.is_empty() {
                let cw = text_width(&count, s);
                draw_text(
                    &mut self.frame,
                    &count,
                    (w - cw - 10 * s).max(0),
                    fy + 9 * s,
                    s,
                    Color { r: 70, g: 70, b: 80, a: 1.0 },
                );
            }
        }

        // Status bar: dedicated bottom strip with state / link target on
        // the left and scheme + CPU + FPS + blocked counter on the right.
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
        let right = format!(
            "{} | CPU {cpu}% | {fps} FPS | {blocked_total} blocked",
            self.scheme_label()
        );
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
        // Right-to-left layout of the strip: stats text, zoom percentage and
        // the -/+ controls in the corner, so nothing ever overlaps.
        let (zox, zoy, zow, zoh) = self.zoom_out_rect();
        let (zix, ziy, ziw, zih) = self.zoom_in_rect();
        let zoom_label = format!("{}%", (self.scale * 100).max(100));
        let zlw = text_width(&zoom_label, s.max(1));
        let stats_right = (zox - zlw - 14 * s).max(0);
        let rw = text_width(&right, s.max(1));
        let max_left_chars = (((stats_right - rw - 12 * s) / (9 * s)).max(1)) as usize;
        let left_trunc: String = left.chars().take(max_left_chars).collect();
        let stats_color = Color { r: 70, g: 70, b: 80, a: 1.0 };
        draw_text(&mut self.frame, &left_trunc, 6 * s, sy + 5 * s, s.max(1), stats_color);
        draw_text(
            &mut self.frame,
            &right,
            (stats_right - rw).max(0),
            sy + 5 * s,
            s.max(1),
            stats_color,
        );
        draw_text(
            &mut self.frame,
            &zoom_label,
            (zox - zlw - 8 * s).max(0),
            sy + 5 * s,
            s.max(1),
            stats_color,
        );

        // Zoom controls: hover highlight, border and centred glyph.
        for (rect, glyph) in [((zox, zoy, zow, zoh), "-"), ((zix, ziy, ziw, zih), "+")] {
            let fill = if hover(rect) {
                Color { r: 214, g: 227, b: 245, a: 1.0 }
            } else {
                Color { r: 246, g: 246, b: 248, a: 1.0 }
            };
            self.frame.fill_rect(rect.0, rect.1, rect.2, rect.3, fill);
            self.frame.stroke_rect(
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                s,
                Color { r: 180, g: 180, b: 190, a: 1.0 },
            );
            let gw = text_width(glyph, s.max(1));
            draw_text(
                &mut self.frame,
                glyph,
                rect.0 + ((rect.2 - gw) / 2).max(0),
                rect.1 + ((rect.3 - 8 * s) / 2).max(0),
                s.max(1),
                Color { r: 60, g: 60, b: 70, a: 1.0 },
            );
        }

        // Tooltip bubble for the hovered chrome button, painted last so it
        // sits on top of everything and clamped inside the window.
        if let Some((tip, rect)) = self.hovered_tooltip() {
            let ts = s.max(1);
            let pad = 4 * ts;
            let tw = text_width(tip, ts) + pad * 2;
            let th = 8 * ts + pad * 2;
            let hi = (w - tw - 2 * s).max(2 * s);
            let tx = (rect.0 + rect.2 / 2 - tw / 2).clamp(2 * s, hi);
            let ty = rect.1 + rect.3 + 4 * s;
            self.frame
                .fill_rect(tx, ty, tw, th, Color { r: 40, g: 42, b: 48, a: 0.96 });
            self.frame
                .stroke_rect(tx, ty, tw, th, s, Color { r: 20, g: 20, b: 24, a: 1.0 });
            draw_text(&mut self.frame, tip, tx + pad, ty + pad, ts, Color::WHITE);
        }
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
            page_bg: Color::WHITE,
        },
    }
}

// URL-vs-query heuristics, percent-encoding and address-box normalization
// live in the utils layer (utils::sanitize); the UI maps SearchEngine to
// its stable u8 index for persistence and for the pure functions there.

// ---------------------------------------------------------------------------
// Built-in pages (no network, no worker)
// ---------------------------------------------------------------------------
const ABOUT_HOME: &str = "about:home";
const START_PAGE_TITLE: &str = "FreeWeb Start";

const START_PAGE_CSS: &str = "\
h1 { color: #16324f; }
h2 { color: #0b5fa5; }
p { color: #202020; }
";

const START_PAGE_HTML: &str = r#"<!DOCTYPE html>
<html><head><title>FreeWeb Start</title></head><body>
<h1>FreeWeb 2.0</h1>
<p>Native Windows browser. Own HTML5/CSS3 renderer, Schannel TLS 1.2/1.3,
SSE4.2 adblock, core-pinned threads. No WebView2, no Chromium, no CEF,
no Electron.</p>
<hr>
<h2>Start here</h2>
<p>Type in the address bar: words become a search, a dotted host becomes a
URL. Ctrl+L or F6 puts the caret in the box, and the G button cycles the
search engine (Google, DuckDuckGo, Bing).</p>
<hr>
<h2>Quick links</h2>
<p><a href="https://en.wikipedia.org/wiki/Web_browser">Wikipedia</a> |
<a href="https://news.ycombinator.com/">Hacker News</a> |
<a href="https://www.rust-lang.org/">Rust</a> |
<a href="https://docs.rs/">docs.rs</a> |
<a href="https://www.w3.org/TR/html52/">HTML spec</a></p>
<hr>
<h2>Keys</h2>
<p>Ctrl+L / F6 address | Ctrl+F find in page | Ctrl+R / F5 reload |
Esc stop | Alt+Home this page | Alt+Left and Alt+Right history</p>
<p>Ctrl+plus / Ctrl+minus / Ctrl+0 zoom | Ctrl+C copy URL | Ctrl+V paste
and go | Space / PageUp / PageDown / Home / End scroll</p>
<hr>
<h2>Limits worth knowing</h2>
<p>There is no JavaScript engine, so pages that draw themselves with script
stay empty. No cookies, so logins do not persist. Images show as alt-text
boxes. Forms do not submit.</p>
</body></html>"#;

const ABOUT_UNKNOWN_HTML: &str = r#"<!DOCTYPE html>
<html><head><title>Unknown page</title></head><body>
<h1>Unknown built-in page</h1>
<p>This engine only serves about:home. Use the Home button or Alt+Home.</p>
</body></html>"#;

// ---------------------------------------------------------------------------
// Document-level compatibility helpers
// ---------------------------------------------------------------------------
/// Automatic navigations allowed in a row before the chain is cut.
const MAX_AUTO_HOPS: u32 = 3;
/// `<meta http-equiv="refresh">` delays above this are not followed: the
/// page is shown and the user decides where to go.
const MAX_REFRESH_SECS: u32 = 5;
/// External stylesheets fetched per page, and the size cap for each one, so a
/// hostile page cannot turn a single navigation into a download storm.
const MAX_SHEETS: usize = 8;
const MAX_SHEET_BYTES: usize = 512 * 1024;

/// True when a `<link>` element declares itself a stylesheet (`rel` is a
/// token list, so `rel="stylesheet alternate"` still counts).
fn is_stylesheet(el: &dom::ElementData) -> bool {
    el.get_attr("rel")
        .map(|rel| {
            rel.split_whitespace()
                .any(|token| token.eq_ignore_ascii_case("stylesheet"))
        })
        .unwrap_or(false)
}

/// The document's base URL: the first `<base href>` resolved against the
/// document URL, or the document URL itself.
fn find_base(d: &Dom, doc_url: &str) -> String {
    for id in d.iter() {
        let Some(n) = d.get(id) else { continue };
        let NodeType::Element(el) = &n.kind else { continue };
        if el.tag == "base" {
            if let Some(href) = el.get_attr("href") {
                let resolved = resolve_url(href, doc_url);
                if !resolved.is_empty() {
                    return resolved;
                }
            }
        }
    }
    doc_url.to_string()
}

/// The page's CSS in document order: inline `<style>` blocks plus external
/// `<link rel=stylesheet>` sheets.
///
/// External sheets are why real sites rendered unstyled before: only inline
/// style blocks were collected. Each linked sheet is resolved against the
/// page base, checked against the adblocker and fetched on the network core.
fn collect_css(d: &Dom, base: &str, adblock: &Arc<Mutex<AdBlocker>>) -> String {
    let mut css = String::new();
    let mut fetched = 0usize;
    for id in d.iter() {
        let Some(n) = d.get(id) else { continue };
        let NodeType::Element(el) = &n.kind else { continue };
        if el.tag == "style" {
            for &c in &n.children {
                let Some(child) = d.get(c) else { continue };
                if let NodeType::Text(t) = &child.kind {
                    css.push_str(t);
                    css.push('\n');
                }
            }
        } else if el.tag == "link" && is_stylesheet(el) {
            if fetched >= MAX_SHEETS {
                continue;
            }
            let Some(href) = el.get_attr("href") else { continue };
            let abs = resolve_url(href, base);
            if !abs.starts_with("http://") && !abs.starts_with("https://") {
                continue; // data: and other schemes are not fetched yet
            }
            let blocked = adblock
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .check(&abs, "")
                == Verdict::Block;
            if blocked {
                continue;
            }
            fetched += 1;
            if let Some(text) = net::fetch_stylesheet(&abs, MAX_SHEET_BYTES) {
                css.push_str(&text);
                css.push('\n');
            }
        }
    }
    css
}

/// First `<meta http-equiv="refresh" content="N; url=...">` worth following.
/// Only short delays count, and the result is resolved against the base.
fn meta_refresh(d: &Dom, base: &str) -> Option<String> {
    for id in d.iter() {
        let Some(n) = d.get(id) else { continue };
        let NodeType::Element(el) = &n.kind else { continue };
        if el.tag != "meta" {
            continue;
        }
        let Some(eq) = el.get_attr("http-equiv") else { continue };
        if !eq.eq_ignore_ascii_case("refresh") {
            continue;
        }
        let Some(content) = el.get_attr("content") else { continue };
        let (delay_txt, rest) = match content.split_once(';') {
            Some((delay, rest)) => (delay, rest),
            None => (content, ""),
        };
        let delay = delay_txt.trim().parse::<u32>().unwrap_or(0);
        if delay > MAX_REFRESH_SECS {
            continue; // let the user read the page instead of hijacking it
        }
        // Offsets in the lowercased copy map 1:1 onto `rest` because
        // to_ascii_lowercase only rewrites ASCII bytes.
        let lower = rest.to_ascii_lowercase();
        let Some(pos) = lower.find("url=") else { continue };
        let target = rest[pos + 4..]
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim();
        if target.is_empty() {
            continue;
        }
        return Some(resolve_url(target, base));
    }
    None
}

/// Links this engine can actually navigate to. `javascript:`, `mailto:`,
/// `tel:`, `data:` and bare `#fragment` references are skipped instead of
/// being handed to the URL parser, which used to turn them into a search.
fn navigable(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

/// Network worker (core 2): adblock verdict → native HTTP GET → parse →
/// linked stylesheets.
///
/// Every event carries `token`, the generation of the load it belongs to, so
/// a result that arrives after the user pressed Stop is dropped on arrival.
/// The whole body runs inside catch_unwind so no fault can abort the app.
fn fetch_worker(
    url: String,
    adblock: Arc<Mutex<AdBlocker>>,
    token: u64,
    proxy: EventLoopProxy<UserEvent>,
) {
    let verdict = adblock
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .check(&url, "");
    if verdict == Verdict::Block {
        let base = url.clone();
        let _ = proxy.send_event(UserEvent::PageReady {
            fetch_gen: token,
            url,
            base,
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
    let fetch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| net::fetch_document(&url)));
    let mut response = match fetch {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            let _ = proxy.send_event(UserEvent::LoadFailed(token, format!("{url}: {e}")));
            return;
        }
        Err(_) => {
            let _ = proxy.send_event(UserEvent::LoadFailed(
                token,
                format!("{url}: internal fetch fault contained"),
            ));
            return;
        }
    };
    if response.status != 200 && response.body.is_empty() {
        let _ = proxy.send_event(UserEvent::LoadFailed(
            token,
            format!("HTTP {} from {url}", response.status),
        ));
        return;
    }
    // A page shell that only fills itself via JavaScript (body with script
    // tags and little or no text) parses to an empty DOM here — that is not
    // a blank-page bug to hide but an engine limit to state honestly.
    let empty_dom_note = if response.body.len() < 256 * 1024 {
        let text = response.text();
        let d_probe = parse_html(&text);
        let mut text_chars = 0usize;
        for id in d_probe.iter() {
            if let Some(n) = d_probe.get(id) {
                if let NodeType::Text(t) = &n.kind {
                    text_chars += t.trim().len();
                }
            }
            if text_chars > 80 {
                break;
            }
        }
        if text_chars <= 80 {
            let lowered = text.to_ascii_lowercase();
            if lowered.contains("<script") {
                Some(
                    "Page loaded, but its content is generated by JavaScript, \
                     which this engine does not run"
                        .to_string(),
                )
            } else {
                Some("Page loaded but contains no readable text".to_string())
            }
        } else {
            None
        }
    } else {
        None
    };
    // HTTP 200 with a zero-byte body is not a valid web page: keep the load
    // alive (blank-body responses do occur legitimately) but tell the user
    // why the page came out empty instead of showing a silent blank view.
    if response.body.is_empty() {
        response.warning = Some(format!(
            "Loaded {url} but the server returned an empty body"
        ));
    }
    // Anti-bot walls (CAPTCHA / browser-integrity challenges) often arrive
    // as a normal HTTP 200 whose page is really a puzzle for a full browser
    // engine. Detect the well-known marks and say so honestly in the status
    // bar instead of silently showing a riddle (or an apparently blank page).
    let challenge_note: Option<String> = {
        let cf_mitigated = response
            .header("cf-mitigated")
            .map(|v| v.to_ascii_lowercase())
            .unwrap_or_default();
        // NOTE: a bare `Server: cloudflare` header is NOT a signal — millions
        // of normally-loading sites sit behind Cloudflare. Only the explicit
        // `cf-mitigated: challenge` header means a challenge was served.
        let head_hit = cf_mitigated.contains("challenge");
        let early = response.body.len().min(48 * 1024);
        let lowered = String::from_utf8_lossy(&response.body[..early])
            .to_ascii_lowercase();
        const STRONG: [&str; 9] = [
            "challenge-platform",
            "cf-challenge",
            "cdn-cgi/challenge",
            "g-recaptcha",
            "h-captcha",
            "turnstile",
            "geetest",
            "arkose",
            "px-captcha",
        ];
        const TITLES: [&str; 6] = [
            "just a moment",
            "attention required",
            "checking your browser",
            "verify you are a human",
            "ddos protection by",
            "enable javascript and cookies to",
        ];
        let body_hit = STRONG.iter().any(|m| lowered.contains(m))
            || TITLES.iter().any(|m| lowered.contains(m));
        if body_hit || (head_hit && !lowered.is_empty()) {
            Some(
                "Anti-bot challenge detected (CAPTCHA / JavaScript check) — \
                 this site requires a full browser engine to pass it"
                    .to_string(),
            )
        } else {
            None
        }
    };
    // Parse on the network core too (cheap); layout goes to cores 1+3.
    // Status is read before `response` is moved into the parse closure.
    let status = response.status;
    let resp_warning = response.warning.clone();
    let doc_url = url.clone();
    let css_blocker = adblock.clone();
    let parse = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let text = response.text();
        let d = parse_html(&text);
        // <base href> re-bases every relative reference on the page, and the
        // document CSS now includes its linked stylesheets.
        let base = find_base(&d, &doc_url);
        let css_src = collect_css(&d, &base, &css_blocker);
        let sheet = parse_stylesheet(&css_src);
        let title = d.title();
        let refresh = meta_refresh(&d, &base);
        (d, sheet, title, base, refresh)
    }));
    match parse {
        Ok((d, sheet, title, base, refresh)) => {
            // TLS and HTTP problems must stay visible: a page that loads
            // under a fallback (for example an untrusted root CA) shows a
            // security warning line in the status bar instead of failing
            // silently or looking perfectly trusted.
            let mut status_error = if status != 200 {
                // Non-200 with a body: the error page still renders (some
                // sites put useful text on 403/410/503), so show it but
                // keep the real status visible in the status bar.
                Some(format!("HTTP {} from {url}", status))
            } else {
                resp_warning.clone()
            };
            if let Some(note) = &empty_dom_note {
                status_error = Some(match status_error {
                    Some(prev) => format!("{prev} | {note}"),
                    None => note.clone(),
                });
            }
            if let Some(note) = &challenge_note {
                status_error = Some(match status_error {
                    Some(prev) => format!("{prev} | {note}"),
                    None => note.clone(),
                });
            }
            let _ = proxy.send_event(UserEvent::PageReady {
                fetch_gen: token,
                url,
                base,
                title,
                dom: d,
                sheet,
                error: status_error,
                blocked: false,
            });
            // Legacy sites redirect with <meta http-equiv="refresh">; the
            // page is shown first, then the hop is requested.
            if let Some(target) = refresh {
                let _ = proxy.send_event(UserEvent::Navigate(target));
            }
        }
        Err(_) => {
            let _ = proxy.send_event(UserEvent::LoadFailed(
                token,
                format!("{url}: document fault contained"),
            ));
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
                // DPI rounding informs the *first-run default* zoom only;
                // a zoom restored from the persisted preferences (already
                // applied in the constructor) is never overwritten here.
                if self.prefs.lock().map(|p| p.zoom_default()).unwrap_or(true) {
                    self.scale = window.scale_factor().round().clamp(1.0, 3.0) as i64;
                }
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
            WindowEvent::CloseRequested => {
                // Persist preferences synchronously on the way out: the
                // debounced saver may still hold up to a second of changes.
                let _ = storage::prefs::store_shared(&self.prefs);
                event_loop.exit();
            }
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
                            let base =
                                self.page.as_ref().map(|p| p.base.as_str()).unwrap_or("");
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
                // The find bar overlays the top of the content area, so a
                // click there must not reach the page underneath.
                if self.mode == Mode::Find && my >= self.bar_h() && my <= self.bar_h() + 26 * s {
                    self.request_redraw();
                    return;
                }
                if hit_btn(self.back_rect()) {
                    self.go_back();
                } else if hit_btn(self.fwd_rect()) {
                    self.go_forward();
                } else if hit_btn(self.home_rect()) {
                    self.go_home();
                } else if hit_btn(self.reload_rect()) {
                    // One slot, two actions: Stop wins while loading.
                    if self.loading {
                        self.stop();
                    } else {
                        self.reload();
                    }
                } else if hit_btn(self.engine_rect()) {
                    // Cycle Google -> DuckDuckGo -> Bing and say so.
                    self.engine = self.engine.next();
                    self.persist_prefs();
                    self.status = format!("Search engine: {}", self.engine.name());
                } else if hit_btn(self.go_rect()) {
                    let url = match (&self.page, self.mode) {
                        (Some(p), Mode::Page) => p.url.clone(),
                        _ => self.input.clone(),
                    };
                    self.navigate(&url);
                } else if my <= self.bar_h() {
                    let (ax, ay, aw, ah) = self.address_rect();
                    if mx >= ax && mx <= ax + aw && my >= ay && my <= ay + ah {
                        self.focus_address();
                    }
                } else if my >= (self.frame.height as i64 - self.status_h()) {
                    // Zoom controls living in the status strip.
                    if hit_btn(self.zoom_out_rect()) {
                        self.set_zoom(-1);
                    } else if hit_btn(self.zoom_in_rect()) {
                        self.set_zoom(1);
                    }
                } else {
                    // Content-area link click: resolve borrow-free first.
                    let bar = self.bar_h();
                    let target = self
                        .layout_cache
                        .as_ref()
                        .and_then(|(lay, _)| hit_test(lay, mx, my - bar, self.scroll_y))
                        .map(|href| {
                            let base =
                                self.page.as_ref().map(|p| p.base.as_str()).unwrap_or("");
                            resolve_url(&href, base)
                        })
                        // javascript:, mailto:, tel: and bare #fragments are
                        // not navigable: skip them instead of turning them
                        // into a search query.
                        .filter(|abs| navigable(abs));
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
            // A result from a load the user stopped (or superseded) is dropped
            // here, before it can touch the current page.
            UserEvent::PageReady { fetch_gen, .. } if fetch_gen != self.fetch_gen => {}
            UserEvent::LoadFailed(token, _) if token != self.fetch_gen => {}
            UserEvent::LayoutReady { generation, result } => {
                // Only the newest generation applies.
                if generation == self.generation {
                    self.layout_cache = Some((result, self.frame.width as i64));
                    self.scroll_y = self.scroll_y.min(self.max_scroll());
                    self.request_redraw();
                }
            }
            UserEvent::PageReady { url, base, title, dom, sheet, error, blocked, .. } => {
                self.loading = false;
                if blocked {
                    self.blocked_on_page += 1;
                    self.status = error.unwrap_or_default();
                    self.request_redraw();
                    return;
                }
                if let Some(w) = &self.window {
                    if !title.is_empty() {
                        w.set_title(&format!("{title} — FreeWeb"));
                    }
                }
                self.page = Some(LoadedPage { url, base, dom, sheet });
                self.layout_cache = None;
                self.request_layout();
                self.status = error.unwrap_or_else(|| "Loaded.".into());
                self.request_redraw();
            }
            UserEvent::Navigate(target) => {
                // A document can ask to move itself with
                // <meta http-equiv="refresh">. Follow a short chain, then
                // stop, so a redirect loop cannot spin the browser.
                if self.auto_hops >= MAX_AUTO_HOPS {
                    self.loading = false;
                    self.status = "Stopped following automatic redirects.".into();
                    self.request_redraw();
                    return;
                }
                self.auto_hops += 1;
                self.navigate_inner(&target);
            }
            UserEvent::LoadFailed(_, msg) => {
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
                // Alt+Home is the start page; plain Home scrolls to the top.
                PhysicalKey::Code(KeyCode::Home) if self.alt_down => self.go_home(),
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
                // F6 focuses the address bar, like every desktop browser.
                PhysicalKey::Code(KeyCode::F6) if !ctrl => self.focus_address(),
                PhysicalKey::Code(KeyCode::Backspace) if !ctrl => self.go_back(),
                // Escape stops a load, exactly like the toolbar Stop button.
                PhysicalKey::Code(KeyCode::Escape) => self.stop(),
                _ => {
                    if ctrl {
                        // Physical-key shortcuts are layout- and IME-proof.
                        match ke.physical_key {
                            // Ctrl+L / Ctrl+K / Ctrl+E: the browser convention
                            // for "focus the address and search bar".
                            PhysicalKey::Code(KeyCode::KeyL)
                            | PhysicalKey::Code(KeyCode::KeyK)
                            | PhysicalKey::Code(KeyCode::KeyE) => self.focus_address(),
                            PhysicalKey::Code(KeyCode::KeyR) => {
                                if let Some(p) = &self.page {
                                    let u = p.url.clone();
                                    self.start_fetch(u);
                                }
                            }
                            PhysicalKey::Code(KeyCode::KeyF) => self.open_find(),
                            PhysicalKey::Code(KeyCode::Equal)
                            | PhysicalKey::Code(KeyCode::NumpadAdd) => self.set_zoom(1),
                            PhysicalKey::Code(KeyCode::Minus)
                            | PhysicalKey::Code(KeyCode::NumpadSubtract) => self.set_zoom(-1),
                            PhysicalKey::Code(KeyCode::Digit0) | PhysicalKey::Code(KeyCode::Numpad0) => {
                                // Ctrl+0 restores the 200% baseline.
                                let delta = 2 - self.scale;
                                self.set_zoom(delta);
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
            // Find bar: Enter walks the matches, Esc closes, the rest is a
            // plain text field.
            Mode::Find => match &ke.logical_key {
                Key::Named(NamedKey::Enter) => self.find_next(),
                Key::Named(NamedKey::Escape) => self.close_find(),
                Key::Named(NamedKey::Backspace) => {
                    self.find.pop();
                    self.find_pos = 0;
                }
                Key::Named(NamedKey::Space) => {
                    self.find.push(' ');
                    self.find_pos = 0;
                }
                Key::Character(_) => {
                    if let Some(text) = &ke.text {
                        for ch in text.chars() {
                            if !ch.is_control() {
                                self.find.push(ch);
                            }
                        }
                        self.find_pos = 0;
                    }
                }
                _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_input_localhost_and_ports() {
        assert_eq!(utils::sanitize::normalize_input("localhost", 0), "http://localhost");
        assert_eq!(
            utils::sanitize::normalize_input("localhost:8080", 0),
            "http://localhost:8080"
        );
        assert_eq!(
            utils::sanitize::normalize_input("127.0.0.1:3000", 0),
            "http://127.0.0.1:3000"
        );
        assert_eq!(
            utils::sanitize::normalize_input("example.com", 0),
            "https://example.com"
        );
        assert_eq!(
            utils::sanitize::normalize_input("http://custom.local:8080", 0),
            "http://custom.local:8080"
        );
    }

    #[test]
    fn normalize_input_searches() {
        let q = utils::sanitize::normalize_input("spanish accents test", 1);
        assert!(q.contains("duckduckgo.com"));
        assert!(q.contains("spanish+accents+test"));
    }

    #[test]
    fn search_engine_persistence_roundtrip() {
        assert_eq!(SearchEngine::from_index(SearchEngine::DuckDuckGo.index()), SearchEngine::DuckDuckGo);
        assert_eq!(SearchEngine::from_index(9), SearchEngine::Google);
    }

    #[test]
    fn prefs_json_roundtrip_via_storage_layer() {
        let mut p = storage::Preferences::default();
        p.engine = 2;
        p.zoom = 3;
        p.push_history("https://example.org/");
        let back = storage::prefs::from_json(&storage::prefs::to_json(&p)).unwrap();
        assert_eq!(back, p);
    }
}
