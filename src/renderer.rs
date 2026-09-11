//! renderer.rs — Layout engine and frame painter.
//!
//! Two phases:
//!   1. LAYOUT: walks the DOM with computed styles, produces a `Line` list
//!      (word boxes, link ranges, backgrounds, borders). Block elements
//!      break lines; inline content wraps against the viewport width.
//!   2. PAINT: draws background rectangles, borders and glyphs from the
//!      built-in public-domain 8x8 bitmap font (scaled 1x/2x), then blits
//!      the RGBA buffer to the window through GDI ( StretchDIBits path
//!      managed in main.rs). This is direct-to-framebuffer rendering with
//!      no D3D scene graph — the same model as early WebKit/Mosaic, sized
//!      for a < 20 MB working set.

#![allow(dead_code)]

use crate::css::{
    compute_style, Color, ComputedStyle, Display, Length, LineStyle, Stylesheet, TextAlign,
};
use crate::dom::{Dom, NodeId, NodeType};
use crate::font8x8::GLYPHS;

// ---------------------------------------------------------------------------
// Framebuffer
// ---------------------------------------------------------------------------
/// RGBA8 framebuffer, bottom-up for GDI (negative biHeight is avoided by
/// flipping rows at blit time; we store top-down and pass -height).
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>, // RGBA8, row-major, top-down
}

impl Frame {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![255; width * height * 4],
        }
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        if self.width != w || self.height != h {
            self.width = w;
            self.height = h;
            self.pixels = vec![255; w * h * 4];
        }
    }

    #[inline]
    pub fn clear(&mut self, c: Color) {
        let [r, g, b, a] = c.to_rgba8();
        let px = [r, g, b, a];
        for p in self.pixels.chunks_exact_mut(4) {
            p.copy_from_slice(&px);
        }
    }

    #[inline]
    pub fn set(&mut self, x: i64, y: i64, c: Color) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            return;
        }
        let i = (y as usize * self.width + x as usize) * 4;
        if c.a >= 1.0 {
            let [r, g, b, _] = c.to_rgba8();
            self.pixels[i] = r;
            self.pixels[i + 1] = g;
            self.pixels[i + 2] = b;
            self.pixels[i + 3] = 255;
            return;
        }
        // Source-over blend: dst = src*a + dst*(1-a). `a` must be the
        // Color's f32 alpha — the byte from to_rgba8() is quantized and
        // cannot participate in float math.
        let a = c.a;
        let ia = 1.0 - a;
        let [r, g, b, _] = c.to_rgba8();
        let dr = self.pixels[i] as f32;
        let dg = self.pixels[i + 1] as f32;
        let db = self.pixels[i + 2] as f32;
        self.pixels[i] = (r as f32 * a + dr * ia) as u8;
        self.pixels[i + 1] = (g as f32 * a + dg * ia) as u8;
        self.pixels[i + 2] = (b as f32 * a + db * ia) as u8;
        self.pixels[i + 3] = 255;
    }

    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: Color) {
        if c.a <= 0.0 {
            return;
        }
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.width as i64);
        let y1 = (y + h).min(self.height as i64);
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.set(xx, yy, c);
            }
        }
    }

    /// Outline rectangle (border painter).
    pub fn stroke_rect(&mut self, x: i64, y: i64, w: i64, h: i64, t: i64, c: Color) {
        if t <= 0 {
            return;
        }
        self.fill_rect(x, y, w, t, c);
        self.fill_rect(x, y + h - t, w, t, c);
        self.fill_rect(x, y + t, t, (h - 2 * t).max(0), c);
        self.fill_rect(x + w - t, y + t, t, (h - 2 * t).max(0), c);
    }
}

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------
#[inline]
fn glyph_bits(cp: char) -> [u8; 8] {
    let idx = match cp {
        ' '..='~' => cp as u32 as usize,
        '\u{A0}' => 32, // nbsp renders as space
        _ => 63,        // '?'
    };
    GLYPHS[idx]
}

#[inline]
fn advance(_cp: char, scale: i64) -> i64 {
    // 1px tracking at 1x to keep words legible.
    8 * scale + scale.max(1)
}

pub fn text_width(s: &str, scale: i64) -> i64 {
    s.chars().map(|c| advance(c, scale)).sum()
}

fn draw_char(f: &mut Frame, cp: char, x: i64, y: i64, scale: i64, color: Color) {
    let bits = glyph_bits(cp);
    for (row, &byte) in bits.iter().enumerate() {
        for col in 0..8 {
            if byte & (1 << (7 - col)) != 0 {
                f.fill_rect(x + col as i64 * scale, y + row as i64 * scale, scale, scale, color);
            }
        }
    }
}

pub fn draw_text(f: &mut Frame, s: &str, x: i64, y: i64, scale: i64, color: Color) {
    let mut cx = x;
    for c in s.chars() {
        draw_char(f, c, cx, y, scale, color);
        cx += advance(c, scale);
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------
/// A word-sized inline run with style reference.
#[derive(Debug, Clone)]
struct Word {
    text: String,
    x: i64,
    y: i64,
    w: i64,
    link: Option<String>,
    scale: i64,
    align: TextAlign,
}

/// A laid-out line of inline content.
#[derive(Debug, Clone)]
pub struct Line {
    words: Vec<Word>,
    bg: Option<(i64, i64, i64, i64, Color)>,
    underline_word_idx: Vec<usize>,
}

/// A painted block box (background + border).
#[derive(Debug, Clone)]
pub struct BoxOut {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    bg: Color,
    border_color: Color,
    border_width: i64,
}

pub struct LayoutResult {
    pub lines: Vec<Line>,
    pub boxes: Vec<BoxOut>,
    pub content_height: i64,
    pub scale: i64,
}

struct LayoutCtx<'a> {
    dom: &'a Dom,
    sheet: &'a Stylesheet,
    hover: &'a std::collections::HashMap<NodeId, u32>,
    viewport_w: i64,
    lines: Vec<Line>,
    boxes: Vec<BoxOut>,
    content_height: i64,
    scale: i64,
    link_count: u32,
}

pub fn layout(
    dom: &Dom,
    sheet: &Stylesheet,
    hover: &std::collections::HashMap<NodeId, u32>,
    viewport_w: i64,
    scale: i64,
) -> LayoutResult {
    let mut ctx = LayoutCtx {
        dom,
        sheet,
        hover,
        viewport_w,
        lines: Vec::new(),
        boxes: Vec::new(),
        content_height: 16 * scale,
        scale,
        link_count: 0,
    };
    // Start one open line; every block flushes it.
    let mut line = Line {
        words: Vec::new(),
        bg: None,
        underline_word_idx: Vec::new(),
    };
    walk(&mut ctx, NodeId(0), &mut line, 16.0, 0, None);
    flush_line(&mut ctx, &mut line);
    LayoutResult {
        lines: ctx.lines,
        boxes: ctx.boxes,
        content_height: ctx.content_height,
        scale,
    }
}

/// Owned snapshot of one node's relevant data, extracted before recursion
/// so layout can take `&mut LayoutCtx` while iterating children.
enum NodeParts {
    Empty,
    Container(Vec<NodeId>),
    Text(String),
    Element(String, Vec<(String, String)>, Option<String>, Vec<NodeId>),
}

fn walk(
    ctx: &mut LayoutCtx,
    id: NodeId,
    line: &mut Line,
    parent_font: f32,
    depth: i64,
    link_href: Option<&str>,
) {
    if depth > 64 {
        return;
    }
    let Some(node) = ctx.dom.get(id) else { return };
    // Node data is cloned up front: the recursive calls below take
    // `&mut ctx`, which cannot coexist with a live `ctx.dom` borrow.
    let node_parts = match &node.kind {
        NodeType::Document => NodeParts::Container(node.children.clone()),
        NodeType::Text(text) => NodeParts::Text(text.clone()),
        NodeType::Comment(_) => NodeParts::Empty,
        NodeType::Element(el) => NodeParts::Element(
            el.tag.clone(),
            el.attrs.clone(),
            el.get_attr("href").map(str::to_string),
            node.children.clone(),
        ),
    };
    match node_parts {
        NodeParts::Empty => {}
        NodeParts::Container(children) => {
            for &c in &children {
                walk(ctx, c, line, parent_font, depth + 1, link_href);
            }
        }
        NodeParts::Text(text) => {
            let st = style_for(ctx, id, parent_font);
            let scale = font_scale(st.font_size, ctx.scale);
            for word in text.split_whitespace() {
                let w = text_width(word, scale);
                // Wrap when the word would cross the right margin.
                let pen_x = match line.words.last() {
                    Some(prev) => prev.x + prev.w + advance(' ', prev.scale.max(scale)),
                    None => 8 * ctx.scale,
                };
                if !line.words.is_empty() && pen_x + w > ctx.viewport_w - 8 * ctx.scale {
                    flush_line(ctx, line);
                }
                let idx = line.words.len();
                let x = if idx == 0 {
                    8 * ctx.scale
                } else {
                    let prev = &line.words[idx - 1];
                    prev.x + prev.w + advance(' ', prev.scale.max(scale))
                };
                line.words.push(Word {
                    text: word.to_string(),
                    x,
                    y: 0, // resolved at flush
                    w,
                    link: link_href.map(str::to_string),
                    scale,
                    align: st.text_align,
                });
                if st.underline {
                    line.underline_word_idx.push(idx);
                }
            }
        }
        NodeParts::Element(tag, attrs, href, children) => {
            let st = style_for(ctx, id, parent_font);
            if st.display == Display::None {
                return;
            }
            let href_ref = href.as_deref().or(link_href);

            let get_attr = |name: &str| -> Option<&str> {
                attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.as_str())
            };

            match tag.as_str() {
                "br" => {
                    flush_line(ctx, line);
                }
                "img" => {
                    // Placeholder box sized from attrs (width/height honored).
                    let scale = ctx.scale;
                    let w = get_attr("width")
                        .and_then(|v| v.trim_end_matches("px").parse::<i64>().ok())
                        .unwrap_or(64 * scale);
                    let h = get_attr("height")
                        .and_then(|v| v.trim_end_matches("px").parse::<i64>().ok())
                        .unwrap_or(48 * scale);
                    let alt = get_attr("alt").unwrap_or("img").to_string();
                    flush_line(ctx, line);
                    let y = ctx.content_height;
                    let x = 8 * scale;
                    ctx.boxes.push(BoxOut {
                        x,
                        y,
                        w: w.max(8 * scale),
                        h: h.max(8 * scale),
                        bg: Color {
                            r: 200,
                            g: 200,
                            b: 210,
                            a: 1.0,
                        },
                        border_color: Color::BLACK,
                        border_width: scale,
                    });
                    ctx.lines.push(Line {
                        words: vec![Word {
                            text: format!("[{alt}]"),
                            x: x + 4 * scale,
                            y: y + 4 * scale,
                            w: text_width(&alt, scale) + 8 * scale,
                            link: None,
                            scale,
                            align: TextAlign::Left,
                        }],
                        bg: None,
                        underline_word_idx: Vec::new(),
                    });
                    ctx.content_height = y + h.max(8 * scale) + 4 * scale;
                }
                "hr" => {
                    flush_line(ctx, line);
                    let scale = ctx.scale;
                    let y = ctx.content_height + 6 * scale;
                    ctx.boxes.push(BoxOut {
                        x: 8 * scale,
                        y,
                        w: ctx.viewport_w - 16 * scale,
                        h: scale.max(1),
                        bg: Color {
                            r: 120,
                            g: 120,
                            b: 130,
                            a: 1.0},
                        border_color: Color::TRANSPARENT,
                        border_width: 0,
                    });
                    ctx.content_height = y + 8 * scale;
                }
                _ => {
                    if st.display == Display::Block {
                        flush_line(ctx, line);
                        let start_h = ctx.content_height;
                        let scale = ctx.scale;
                        let x0 = 8 * scale;
                        let w = block_width(&st, ctx.viewport_w, scale);
                        // Recurse children.
                        let mut inner_line = Line {
                            words: Vec::new(),
                            bg: None,
                            underline_word_idx: Vec::new(),
                        };
                        for &c in &children {
                            walk(ctx, c, &mut inner_line, st.font_size, depth + 1, href_ref);
                        }
                        flush_line(ctx, &mut inner_line);
                        // Background/border box.
                        let bg = st.background_color;
                        let bw = border_w(&st, scale);
                        if bg.a > 0.0 || bw > 0 {
                            let h = (ctx.content_height - start_h + 6 * scale).max(10 * scale);
                            ctx.boxes.push(BoxOut {
                                x: x0 - 2 * scale,
                                y: start_h,
                                w: w + 4 * scale,
                                h,
                                bg,
                                border_color: st.border_color,
                                border_width: bw,
                            });
                        }
                    } else {
                        // Inline element: recurse with own font style.
                        for &c in &children {
                            walk(ctx, c, line, st.font_size, depth + 1, href_ref);
                        }
                    }
                }
            }
        }
    }
}

fn style_for(ctx: &LayoutCtx, id: NodeId, parent_font: f32) -> ComputedStyle {
    compute_style(ctx.dom, ctx.sheet, id, ctx.hover, parent_font)
}

fn font_scale(font_px: f32, base: i64) -> i64 {
    let s = (font_px / 8.0).round() as i64;
    s.clamp(1, 4).max(base.min(4))
}

fn block_width(st: &ComputedStyle, viewport_w: i64, scale: i64) -> i64 {
    match st.width {
        Length::Auto => viewport_w - 16 * scale,
        Length::Px(px) => (px as i64).clamp(16, viewport_w - 16 * scale),
        Length::Percent(p) => ((viewport_w - 16 * scale) as f32 * p / 100.0) as i64,
        Length::Em(e) => (e * 16.0) as i64,
    }
}

fn border_w(st: &ComputedStyle, _scale: i64) -> i64 {
    if st.border_style == LineStyle::Solid && st.border_width > 0.0 {
        (st.border_width as i64).clamp(1, 8)
    } else {
        0
    }
}

/// Assign x/y positions to words (left/center/right per first word's
/// text-align), push the finished line into ctx.lines.
fn flush_line(ctx: &mut LayoutCtx, line: &mut Line) {
    if line.words.is_empty() {
        return;
    }
    let scale = ctx.scale;
    let max_scale = line.words.iter().map(|w| w.scale).max().unwrap_or(1);
    let line_h = 10 * max_scale;
    let y = ctx.content_height;

    let words_w: i64 = line.words.iter().map(|w| w.w).sum();
    let spaces_w: i64 = line
        .words
        .windows(2)
        .map(|p| advance(' ', p[0].scale.max(p[1].scale)))
        .sum();
    let total = words_w + spaces_w;
    let avail = (ctx.viewport_w - 16 * scale).max(0);
    let align = line.words[0].align;
    let start_x = match align {
        TextAlign::Left => 8 * scale,
        TextAlign::Center => 8 * scale + ((avail - total) / 2).max(0),
        TextAlign::Right => 8 * scale + (avail - total).max(0),
    };
    let mut cx = start_x;
    for w in line.words.iter_mut() {
        w.x = cx;
        w.y = y;
        cx += w.w + advance(' ', w.scale);
    }

    ctx.lines.push(std::mem::replace(
        line,
        Line {
            words: Vec::new(),
            bg: None,
            underline_word_idx: Vec::new(),
        },
    ));
    ctx.content_height = y + line_h + 2 * scale;
}

/// Paint a laid-out document. Called every frame or on dirty.
///
/// `highlight` is the find-in-page query: every matching word is painted on
/// top of a marker so matches are visible on the page, not just counted.
pub fn paint(frame: &mut Frame, layout: &LayoutResult, scroll_y: i64, highlight: Option<&str>) {
    frame.clear(Color::WHITE);
    let needle = highlight
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty());

    // Boxes first (backgrounds, borders).
    for b in &layout.boxes {
        let y = b.y - scroll_y;
        if y + b.h < -64 || y > frame.height as i64 + 64 {
            continue;
        }
        if b.bg.a > 0.0 {
            frame.fill_rect(b.x, y, b.w, b.h, b.bg);
        }
        if b.border_width > 0 {
            frame.stroke_rect(b.x, y, b.w, b.h, b.border_width, b.border_color);
        }
    }

    // Then text lines.
    for line in &layout.lines {
        for (idx, w) in line.words.iter().enumerate() {
            let y = w.y - scroll_y;
            if y + 10 * w.scale < -32 || y > frame.height as i64 + 32 {
                continue;
            }
            if let Some(n) = &needle {
                if w.text.to_ascii_lowercase().contains(n.as_str()) {
                    frame.fill_rect(
                        w.x,
                        y,
                        w.w,
                        10 * w.scale,
                        Color { r: 255, g: 235, b: 59, a: 1.0 },
                    );
                }
            }
            if w.link.is_some() {
                // Link words render in classic link blue with an underline.
                let link_color = Color { r: 0, g: 0, b: 238, a: 1.0 };
                draw_text(frame, &w.text, w.x, y, w.scale, link_color);
                frame.fill_rect(w.x, y + 9 * w.scale, w.w, w.scale, link_color);
            } else {
                draw_text(frame, &w.text, w.x, y, w.scale, Color::BLACK);
                if line.underline_word_idx.contains(&idx) {
                    frame.fill_rect(w.x, y + 9 * w.scale, w.w, w.scale, Color::BLACK);
                }
            }
        }
    }
}

/// Hit-test words for mouse clicks; returns link href.
pub fn hit_test(layout: &LayoutResult, x: i64, y: i64, scroll_y: i64) -> Option<String> {
    for line in &layout.lines {
        for w in &line.words {
            let wy = w.y - scroll_y;
            if x >= w.x && x <= w.x + w.w && y >= wy && y <= wy + 10 * w.scale {
                if let Some(href) = &w.link {
                    return Some(href.clone());
                }
            }
        }
    }
    None
}

/// Content-space Y positions of every word matching `query`,
/// case-insensitively, in document order.
///
/// Matching is word-level because layout stores words, not a text stream, so
/// a multi-word phrase matches per word instead of as a sequence.
pub fn find_matches(layout: &LayoutResult, query: &str) -> Vec<i64> {
    let q = query.trim().to_ascii_lowercase();
    let mut hits = Vec::new();
    if q.is_empty() {
        return hits;
    }
    for line in &layout.lines {
        for w in &line.words {
            if w.text.to_ascii_lowercase().contains(q.as_str()) {
                hits.push(w.y);
            }
        }
    }
    hits
}

/// Resolve a reference against a base URL using the RFC 3986 merge rules a
/// browser uses: absolute URIs, protocol-relative `//host`, absolute paths,
/// `./` and `../` dot segments, query-only and fragment-only references.
///
/// Relative links are the norm on real sites, and the previous version only
/// handled `/abs` and bare names: `../x` produced a bogus path and no site
/// with a nested directory structure was navigable.
///
/// References to other schemes (`javascript:`, `mailto:`, `tel:`) come back
/// untouched — the caller decides whether they are navigable — and a dotted
/// prefix like `example.com:8080` is a host with a port, not a scheme.
pub fn resolve_url(href: &str, base: &str) -> String {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return href.to_string();
    }
    if let Some(colon) = href.find(':') {
        if is_scheme_name(&href[..colon]) {
            return href.to_string();
        }
    }
    let Some(sep) = base.find("://") else {
        return href.to_string();
    };
    let scheme = &base[..sep];
    let rest = &base[sep + 3..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path_and_query = &rest[authority_end..];
    let path_end = path_and_query.find('?').unwrap_or(path_and_query.len());
    let path = &path_and_query[..path_end];

    if href.starts_with("//") {
        return format!("{scheme}:{href}");
    }
    if href.starts_with('/') {
        return format!("{scheme}://{authority}{href}");
    }
    if href.starts_with('?') {
        return format!("{scheme}://{authority}{path}{href}");
    }
    // Relative path: merge it into the base directory, then drop dot
    // segments. Any query/fragment tail is appended verbatim.
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    let tail_at = href
        .find(|c: char| c == '?' || c == '#')
        .unwrap_or(href.len());
    let (rel_path, tail) = href.split_at(tail_at);
    if rel_path.is_empty() {
        return format!("{scheme}://{authority}{path}{tail}");
    }
    let merged = remove_dot_segments(&format!("{dir}{rel_path}"));
    format!("{scheme}://{authority}{merged}{tail}")
}

/// True for a URI scheme name: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
/// A dotted prefix is a host name, so `example.com:8080` is not a scheme.
fn is_scheme_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    !s.contains('.') && chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

/// Drop `.` and `..` segments from an absolute path, returning an absolute
/// path (browser-style normalisation).
fn remove_dot_segments(path: &str) -> String {
    let trailing_slash = path.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    let mut s = String::from("/");
    s.push_str(&out.join("/"));
    if trailing_slash && !s.ends_with('/') {
        s.push('/');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::parse_stylesheet;
    use crate::dom::parse_html;

    #[test]
    fn layout_produces_lines() {
        let dom = parse_html("<h1>Title</h1><p>Some body text here that is long enough to wrap across the viewport multiple times for sure.</p>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 800, 2);
        assert!(!res.lines.is_empty());
        assert!(res.content_height > 0);
    }

    #[test]
    fn paint_does_not_panic_and_draws() {
        let dom = parse_html("<p style=\"background:#eee\">boxed text <a href='x'>link</a></p>");
        let sheet = parse_stylesheet("p { background: #eeeeee; }");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        let mut frame = Frame::new(400, 300);
        paint(&mut frame, &res, 0, None);
        assert!(frame.pixels.iter().any(|&p| p == 0)); // some dark glyph pixels

        // Find-in-page highlighting paints the marker behind matching words.
        let hits = find_matches(&res, "link");
        assert_eq!(hits.len(), 1);
        paint(&mut frame, &res, 0, Some("link"));
        assert!(frame.pixels.chunks_exact(4).any(|p| p[0] == 255 && p[1] == 235));
    }

    #[test]
    fn hit_test_finds_link() {
        let dom = parse_html("<a href=\"https://example.com\">click</a>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        // First line words sit at y = 16*scale .. 26*scale; click inside
        // the "click" link word (x starts at 8*scale).
        assert!(hit_test(&res, 12, 20, 0).is_some());
        assert!(hit_test(&res, 300, 200, 0).is_none());
    }

    #[test]
    fn url_resolution() {
        assert_eq!(
            resolve_url("/a/b", "https://x.y/page"),
            "https://x.y/a/b"
        );
        assert_eq!(
            resolve_url("https://q.r/s", "https://x.y/page"),
            "https://q.r/s"
        );
    }
}
