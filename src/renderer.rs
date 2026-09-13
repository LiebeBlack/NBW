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
use crate::font8x8::get_glyph;

// ---------------------------------------------------------------------------
// Framebuffer
// ---------------------------------------------------------------------------
/// BGRA8 framebuffer, row-major, top-down. GDI's 32bpp BI_RGB DIBs read
/// the first byte of each pixel as BLUE, so colors are stored in BGRA order
/// and `DibSurface::present` blits the buffer with zero conversion.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>, // BGRA8, row-major, top-down
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
        // Frame stores BGRA (GDI 32bpp byte order): blue first.
        let px = [b, g, r, a];
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
            self.pixels[i] = b;
            self.pixels[i + 1] = g;
            self.pixels[i + 2] = r;
            self.pixels[i + 3] = 255;
            return;
        }
        // Source-over blend: dst = src*a + dst*(1-a). `a` must be the
        // Color's f32 alpha — the byte from to_rgba8() is quantized and
        // cannot participate in float math. Channels are written BGRA.
        let a = c.a;
        let ia = 1.0 - a;
        let [r, g, b, _] = c.to_rgba8();
        let dr = self.pixels[i] as f32;
        let dg = self.pixels[i + 1] as f32;
        let db = self.pixels[i + 2] as f32;
        self.pixels[i] = (b as f32 * a + dr * ia) as u8;
        self.pixels[i + 1] = (g as f32 * a + dg * ia) as u8;
        self.pixels[i + 2] = (r as f32 * a + db * ia) as u8;
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
    get_glyph(cp)
}

#[inline]
fn advance(cp: char, scale: i64) -> i64 {
    // CJK and fullwidth forms occupy two glyph cells (the font renders
    // them in one 8px cell today, but the advance must reserve the space
    // or CJK text renders squashed with overlapping columns).
    let u = cp as u32;
    let wide = matches!(u,
        0x1100..=0x115F   // Hangul Jamo
        | 0x2E80..=0xA4CF // CJK radicals .. Yi
        | 0xAC00..=0xD7A3 // Hangul syllables
        | 0xF900..=0xFAFF // CJK compatibility ideographs
        | 0xFE30..=0xFE4F // CJK compatibility forms
        | 0xFF00..=0xFF60 // fullwidth forms
        | 0xFFE0..=0xFFE6 // fullwidth signs
        | 0x20000..=0x3FFFD // CJK extensions
    );
    if wide {
        return 16 * scale + scale.max(1);
    }
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
            // font8x8 convention: bit 0 is the LEFTMOST pixel column.
            if byte & (1 << col) != 0 {
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

/// Single text entry with optional fake-italic slant, so callers never
/// branch on the style themselves.
fn draw_text_italic(f: &mut Frame, s: &str, x: i64, y: i64, scale: i64, color: Color, italic: bool) {
    if italic {
        draw_text_slant(f, s, x, y, scale, color);
    } else {
        draw_text(f, s, x, y, scale, color);
    }
}

/// Blend `c` toward `bg` by factor `k` (k=1 keeps c, k=0 yields bg).
fn blend_to_bg(c: Color, bg: Color, k: f32) -> Color {
    Color {
        r: (c.r as f32 * k + bg.r as f32 * (1.0 - k)) as u8,
        g: (c.g as f32 * k + bg.g as f32 * (1.0 - k)) as u8,
        b: (c.b as f32 * k + bg.b as f32 * (1.0 - k)) as u8,
        a: 1.0,
    }
}

/// Italic text: each glyph row is shifted right proportionally to its
/// depth, producing a true slant from the same 8x8 bitmap (fake italic,
/// as every bitmap-font browser has done). Same advance as upright text.
pub fn draw_text_slant(f: &mut Frame, s: &str, x: i64, y: i64, scale: i64, color: Color) {
    let mut cx = x;
    for c in s.chars() {
        let bits = glyph_bits(c);
        for (row, &byte) in bits.iter().enumerate() {
            let shift = (row as i64 * scale) / 8;
            for col in 0..8 {
                // font8x8 convention: bit 0 is the LEFTMOST pixel column.
                if byte & (1 << col) != 0 {
                    f.fill_rect(
                        cx + (col as i64 + shift) * scale,
                        y + row as i64 * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
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
    has_leading_space: bool,
    /// Resolved CSS text color for this word.
    color: Color,
    /// `font-style: italic` — painted with the slanted glyph path.
    italic: bool,
    /// `text-decoration: line-through` — mid-height bar through the word.
    strike: bool,
    /// Effective opacity (product of inline ancestor opacities); the
    /// painter blends the word toward the page background.
    fade: f32,
    /// Vertical offset in device px (`<sub>` positive, `<sup>` negative).
    dy: i64,
}

/// A laid-out line of inline content.
#[derive(Debug, Clone)]
pub struct Line {
    words: Vec<Word>,
    bg: Option<(i64, i64, i64, i64, Color)>,
    underline_word_idx: Vec<usize>,
    trailing_space: bool,
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
    /// Canvas background: the `<body>` background-color from CSS, or white
    /// when unset/transparent. Painted by `paint` before anything else so
    /// dark-themed pages render correctly instead of forcing white.
    pub page_bg: Color,
    /// `id` -> content-space Y for every element that declares one, so
    /// `#fragment` links can scroll in place (like a real browser).
    pub anchors: std::collections::HashMap<String, i64>,
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
    /// CSS left offset for the line/box currently being laid out (block
    /// `margin-left` + `padding-left` in device pixels). Text starts at
    /// this edge instead of the hardcoded viewport gutter.
    line_left: i64,
    /// Available width for text inside that same edge.
    line_right: i64,
    /// `id`/`name` -> content-space Y, collected during the walk for
    /// fragment-link scrolling.
    anchors: std::collections::HashMap<String, i64>,
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
        line_left: 8 * scale,
        line_right: viewport_w - 8 * scale,
        anchors: std::collections::HashMap::new(),
    };
    // Canvas background from the body element's computed CSS, if any.
    let page_bg = body_background(dom, sheet, hover).unwrap_or(Color::WHITE);
    // Start one open line; every block flushes it.
    let mut line = Line {
        words: Vec::new(),
        bg: None,
        underline_word_idx: Vec::new(),
        trailing_space: false,
    };
    let root_color = ComputedStyle::default().color;
    walk(
        &mut ctx,
        NodeId(0),
        &mut line,
        16.0,
        0,
        None,
        root_color,
        1.0,
        0,
        0,
    );
    flush_line(&mut ctx, &mut line);
    LayoutResult {
        lines: ctx.lines,
        boxes: ctx.boxes,
        content_height: ctx.content_height,
        scale,
        page_bg,
        anchors: ctx.anchors,
    }
}

/// The computed `background-color` of the `<body>` element, when it is
/// opaque. Transparent/absent yields None (caller falls back to white).
fn body_background(
    dom: &Dom,
    sheet: &Stylesheet,
    hover: &std::collections::HashMap<NodeId, u32>,
) -> Option<Color> {
    for id in dom.iter() {
        let node = dom.get(id)?;
        if let NodeType::Element(el) = &node.kind {
            if el.tag == "body" {
                let st = compute_style(dom, sheet, id, hover, 16.0);
                return if st.background_color.a > 0.0 {
                    Some(st.background_color)
                } else {
                    None
                };
            }
        }
    }
    None
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
    parent_color: Color,
    parent_fade: f32,
    vofs: i64,
    ol_index: u32,
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
                walk(
                    ctx,
                    c,
                    line,
                    parent_font,
                    depth + 1,
                    link_href,
                    parent_color,
                    parent_fade,
                    vofs,
                    ol_index,
                );
            }
        }
        NodeParts::Text(text) => {
            let st = style_for(ctx, id, parent_font);
            let scale = font_scale(st.font_size, ctx.scale);
            let words: Vec<&str> = text.split_whitespace().collect();
            if words.is_empty() {
                if !text.is_empty() {
                    line.trailing_space = true;
                }
                return;
            }
            let text_starts_ws = text.starts_with(char::is_whitespace);
            let text_ends_ws = text.ends_with(char::is_whitespace);

            for (w_idx, word) in words.iter().enumerate() {
                let w = text_width(word, scale);
                let lead_space = if w_idx == 0 {
                    line.trailing_space || text_starts_ws
                } else {
                    true
                };
                // Wrap when the word would cross the right margin.
                let pen_x = match line.words.last() {
                    Some(prev) => {
                        let sp = if lead_space {
                            advance(' ', prev.scale.max(scale))
                        } else {
                            0
                        };
                        prev.x + prev.w + sp
                    }
                    None => ctx.line_left,
                };
                if !line.words.is_empty() && pen_x + w > ctx.line_right {
                    flush_line(ctx, line);
                }
                let idx = line.words.len();
                let actual_lead = if idx == 0 { false } else { lead_space };
                let x = if idx == 0 {
                    ctx.line_left
                } else {
                    let prev = &line.words[idx - 1];
                    let sp = if actual_lead {
                        advance(' ', prev.scale.max(scale))
                    } else {
                        0
                    };
                    prev.x + prev.w + sp
                };
                line.words.push(Word {
                    text: word.to_string(),
                    x,
                    y: 0, // resolved at flush
                    w,
                    link: link_href.map(str::to_string),
                    scale,
                    align: st.text_align,
                    has_leading_space: actual_lead,
                    // Text nodes are not elements: compute_style returns
                    // defaults for them, so the color comes from the
                    // inherited parent chain (CSS `color` inheritance).
                    color: parent_color,
                    italic: st.italic,
                    strike: st.line_through,
                    // CSS opacity multiplies down the whole subtree;
                    // parent_fade arrives pre-multiplied from every
                    // element walk (root starts at 1.0).
                    fade: parent_fade * st.opacity,
                    dy: vofs,
                });
                if st.underline {
                    line.underline_word_idx.push(idx);
                }
            }
            line.trailing_space = text_ends_ws;
        }
        NodeParts::Element(tag, attrs, href, children) => {
            let st = style_for(ctx, id, parent_font);
            if st.display == Display::None {
                return;
            }
            // Record `id`/`name` anchors with their content-space Y so
            // fragment links scroll to them.
            if let Some(a) = get_attr2(&attrs, "id").or_else(|| get_attr2(&attrs, "name")) {
                if !a.is_empty() {
                    ctx.anchors.insert(a.to_string(), ctx.content_height);
                }
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
                    let x = ctx.line_left;
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
                            has_leading_space: false,
                            color: Color::BLACK,
                            italic: false,
                            strike: false,
                            fade: 1.0,
                            dy: 0,
                        }],
                        bg: None,
                        underline_word_idx: Vec::new(),
                        trailing_space: false,
                    });
                    ctx.content_height = y + h.max(8 * scale) + 4 * scale;
                }
                "hr" => {
                    flush_line(ctx, line);
                    let scale = ctx.scale;
                    let y = ctx.content_height + 6 * scale;
                    ctx.boxes.push(BoxOut {
                        x: ctx.line_left,
                        y,
                        w: (ctx.line_right - ctx.line_left).max(16 * scale),
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
                "li" => {
                    flush_line(ctx, line);
                    let scale = ctx.scale;
                    // Ordered lists number their items; unordered use the
                    // bullet. `ol_index` is stamped by the parent list walk.
                    let marker = if ol_index > 0 {
                        format!("{ol_index}.")
                    } else {
                        "\u{2022}".to_string()
                    };
                    let mw = text_width(&marker, scale);
                    line.words.push(Word {
                        text: marker,
                        x: ctx.line_left,
                        y: 0,
                        w: mw,
                        link: None,
                        scale,
                        align: TextAlign::Left,
                        has_leading_space: false,
                        color: st.color,
                        italic: st.italic,
                        strike: st.line_through,
                        fade: 1.0,
                        dy: 0,
                    });
                    line.trailing_space = true;
                    for &c in &children {
                        walk(
                            ctx,
                            c,
                            line,
                            st.font_size,
                            depth + 1,
                            href_ref,
                            st.color,
                            parent_fade,
                            vofs,
                            0,
                        );
                    }
                    flush_line(ctx, line);
                }
                "table" => {
                    // Light real table: rows split the available width into
                    // equal columns; every cell lays out its own content on
                    // its own edges (nested tables recurse through walk).
                    flush_line(ctx, line);
                    let scale = ctx.scale;
                    let table_left = ctx.line_left;
                    let table_right = ctx.line_right;
                    let table_w = (table_right - table_left).max(32 * scale);
                    let st_inner = st.clone();
                    let mut row: Vec<Vec<NodeId>> = Vec::new();
                    for &c in &children {
                        if let Some(n) = ctx.dom.get(c) {
                            if let NodeType::Element(el) = &n.kind {
                                if el.tag == "tr" {
                                    row.push(n.children.clone());
                                } else if el.tag == "tbody"
                                    || el.tag == "thead"
                                    || el.tag == "tfoot"
                                {
                                    for &rc in &n.children {
                                        if let Some(rn) = ctx.dom.get(rc) {
                                            if let NodeType::Element(rel) = &rn.kind {
                                                if rel.tag == "tr" {
                                                    row.push(rn.children.clone());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    for cells in &row {
                        let ncols = cells.len().max(1);
                        let col_w = (table_w / ncols as i64).max(8 * scale);
                        let row_top = ctx.content_height;
                        for (ci, cell_kids) in cells.iter().enumerate() {
                            let saved_l = ctx.line_left;
                            let saved_r = ctx.line_right;
                            let cell_x = table_left + (ci as i64) * col_w;
                            ctx.line_left = cell_x + 2 * scale;
                            ctx.line_right = (cell_x + col_w - 2 * scale).max(ctx.line_left);
                            let cell_top = ctx.content_height;
                            let mut cell_line = Line {
                                words: Vec::new(),
                                bg: None,
                                underline_word_idx: Vec::new(),
                                trailing_space: false,
                            };
                            for &k in cell_kids {
                                walk(
                                    ctx,
                                    k,
                                    &mut cell_line,
                                    st_inner.font_size,
                                    depth + 1,
                                    href_ref,
                                    st_inner.color,
                                    parent_fade * st_inner.opacity,
                                    0,
                                    0,
                                );
                            }
                            flush_line(ctx, &mut cell_line);
                            let cell_h = (ctx.content_height - cell_top).max(10 * scale);
                            ctx.boxes.push(BoxOut {
                                x: cell_x,
                                y: cell_top,
                                w: col_w,
                                h: cell_h,
                                bg: st_inner.background_color,
                                border_color: st_inner.border_color,
                                border_width: if st_inner.border_style == LineStyle::Solid
                                    && st_inner.border_width > 0.0
                                {
                                    (st_inner.border_width as i64).clamp(1, 8)
                                } else {
                                    scale.max(1)
                                },
                            });
                            ctx.content_height = ctx
                                .content_height
                                .max(cell_top + cell_h);
                            ctx.line_left = saved_l;
                            ctx.line_right = saved_r;
                        }
                        ctx.content_height += 2 * scale;
                    }
                    if row.is_empty() {
                        // A table with no recognized rows still eats a gap
                        // so the layout does not fuse neighbors together.
                        ctx.content_height += 4 * scale;
                    }
                }
                "pre" => {
                    // Preformatted: whitespace and newlines are significant.
                    // Words carry absolute x/y and lines are pushed directly
                    // (flush_line would re-position and collapse spaces).
                    flush_line(ctx, line);
                    let scale = ctx.scale;
                    for &c in &children {
                        if let Some(n) = ctx.dom.get(c) {
                            if let NodeType::Text(t) = &n.kind {
                                for raw_line in t.split('\n') {
                                    let y = ctx.content_height;
                                    let mut words: Vec<Word> = Vec::new();
                                    let mut cx = ctx.line_left;
                                    for seg in raw_line.split(' ') {
                                        if seg.is_empty() {
                                            cx += advance(' ', scale);
                                            continue;
                                        }
                                        let sw = text_width(seg, scale);
                                        if cx + sw > ctx.line_right && !words.is_empty() {
                                            // Wrap like a long code line.
                                            ctx.lines.push(Line {
                                                words: std::mem::take(&mut words),
                                                bg: None,
                                                underline_word_idx: Vec::new(),
                                                trailing_space: false,
                                            });
                                            ctx.content_height += 12 * scale;
                                            cx = ctx.line_left;
                                        }
                                        words.push(Word {
                                            text: seg.to_string(),
                                            x: cx,
                                            y,
                                            w: sw,
                                            link: link_href.map(str::to_string),
                                            scale,
                                            align: TextAlign::Left,
                                            has_leading_space: cx > ctx.line_left,
                                            color: st.color,
                                            italic: st.italic,
                                            strike: st.line_through,
                                            fade: 1.0,
                                            dy: 0,
                                        });
                                        cx += sw + advance(' ', scale);
                                    }
                                    if !words.is_empty() {
                                        ctx.lines.push(Line {
                                            words,
                                            bg: None,
                                            underline_word_idx: Vec::new(),
                                            trailing_space: false,
                                        });
                                    }
                                    ctx.content_height = y + 12 * scale;
                                }
                            }
                        }
                    }
                }
                "details" => {
                    // Render summary + children only when open; otherwise
                    // just the summary with a folded marker.
                    let open = get_attr("open").is_some();
                    flush_line(ctx, line);
                    for &c in &children {
                        if let Some(n) = ctx.dom.get(c) {
                            if let NodeType::Element(el) = &n.kind {
                                if el.tag == "summary" {
                                    let mut sm = Line {
                                        words: Vec::new(),
                                        bg: None,
                                        underline_word_idx: Vec::new(),
                                        trailing_space: false,
                                    };
                                    // ASCII markers: guaranteed glyphs in the
                                    // 8x8 font (arrows like ▸ are not mapped).
                                    sm.words.push(Word {
                                        text: if open { "v".into() } else { ">".into() },
                                        x: ctx.line_left,
                                        y: 0,
                                        w: text_width(">", ctx.scale),
                                        link: None,
                                        scale: ctx.scale,
                                        align: TextAlign::Left,
                                        has_leading_space: false,
                                        color: st.color,
                                        italic: false,
                                        strike: false,
                                        fade: 1.0,
                                        dy: 0,
                                    });
                                    sm.trailing_space = true;
                                    for &sc in &n.children {
                                        walk(
                                            ctx,
                                            sc,
                                            &mut sm,
                                            st.font_size,
                                            depth + 1,
                                            href_ref,
                                            st.color,
                                            parent_fade * st.opacity,
                                            0,
                                            0,
                                        );
                                    }
                                    flush_line(ctx, &mut sm);
                                } else if open {
                                    walk(
                                        ctx,
                                        c,
                                        line,
                                        st.font_size,
                                        depth + 1,
                                        href_ref,
                                        st.color,
                                        parent_fade * st.opacity,
                                        0,
                                        0,
                                    );
                                }
                            }
                        }
                    }
                }
                "input" | "button" | "select" | "textarea" => {
                    // Form controls: an honest 3D-ish control box with the
                    // value/placeholder text inside. Not interactive yet —
                    // but visible and labeled instead of invisible.
                    flush_line(ctx, line);
                    let scale = ctx.scale;
                    // Buttons render their child text (like every browser);
                    // inputs use value, then placeholder.
                    let label = get_attr("value")
                        .or_else(|| get_attr("placeholder"))
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            if tag == "button" {
                                let mut s = String::new();
                                for &c in &children {
                                    if let Some(n) = ctx.dom.get(c) {
                                        if let NodeType::Text(t) = &n.kind {
                                            s.push_str(t);
                                        }
                                    }
                                }
                                s.trim().to_string()
                            } else {
                                String::new()
                            }
                        });
                    let is_button = tag == "button"
                        || get_attr("type").map(|t| t.eq_ignore_ascii_case("submit")
                            || t.eq_ignore_ascii_case("button"))
                            .unwrap_or(false);
                    let cw = (text_width(&label, scale) + 12 * scale).max(if is_button {
                        48 * scale
                    } else {
                        96 * scale
                    });
                    let ch = 16 * scale;
                    let y = ctx.content_height;
                    let x = ctx.line_left;
                    ctx.boxes.push(BoxOut {
                        x,
                        y,
                        w: cw,
                        h: ch,
                        bg: Color { r: 255, g: 255, b: 255, a: 1.0 },
                        border_color: Color { r: 110, g: 110, b: 120, a: 1.0 },
                        border_width: scale.max(1),
                    });
                    if !label.is_empty() {
                        ctx.lines.push(Line {
                            words: vec![Word {
                                w: text_width(&label, scale),
                                text: label,
                                x: x + 4 * scale,
                                y: y + 3 * scale,
                                link: None,
                                scale,
                                align: TextAlign::Left,
                                has_leading_space: false,
                                color: Color { r: 20, g: 20, b: 20, a: 1.0 },
                                italic: false,
                                strike: false,
                                fade: 1.0,
                                dy: 0,
                            }],
                            bg: None,
                            underline_word_idx: Vec::new(),
                            trailing_space: false,
                        });
                    }
                    ctx.content_height = y + ch + 4 * scale;
                }
                "iframe" | "video" | "canvas" | "object" | "embed" | "svg" => {
                    // Embeds render as labeled bordered placeholders so the
                    // surrounding layout keeps its shape.
                    flush_line(ctx, line);
                    let scale = ctx.scale;
                    let avail = (ctx.line_right - ctx.line_left).max(32 * scale);
                    let w = get_attr("width")
                        .and_then(|v| v.trim_end_matches("px").parse::<i64>().ok())
                        .unwrap_or(320 * scale)
                        .clamp(32 * scale, avail);
                    let h = get_attr("height")
                        .and_then(|v| v.trim_end_matches("px").parse::<i64>().ok())
                        .unwrap_or(120 * scale)
                        .max(24 * scale);
                    let y = ctx.content_height;
                    let x = ctx.line_left;
                    ctx.boxes.push(BoxOut {
                        x,
                        y,
                        w,
                        h,
                        bg: Color { r: 235, g: 235, b: 240, a: 1.0 },
                        border_color: Color { r: 90, g: 90, b: 100, a: 1.0 },
                        border_width: scale.max(1),
                    });
                    let label = format!("[{tag}]");
                    ctx.lines.push(Line {
                        words: vec![Word {
                            text: label.clone(),
                            x: x + 4 * scale,
                            y: y + 4 * scale,
                            w: text_width(&label, scale),
                            link: None,
                            scale,
                            align: TextAlign::Left,
                            has_leading_space: false,
                            color: Color { r: 60, g: 60, b: 70, a: 1.0 },
                            italic: false,
                            strike: false,
                            fade: 1.0,
                            dy: 0,
                        }],
                        bg: None,
                        underline_word_idx: Vec::new(),
                        trailing_space: false,
                    });
                    ctx.content_height = y + h + 4 * scale;
                }
                "ul" | "ol" => {
                    // List wrapper: stamps `ol_index` so li can number
                    // itself; children li branches handle their own flush.
                    flush_line(ctx, line);
                    let mut i: u32 = 0;
                    for &c in &children {
                        let is_li = ctx
                            .dom
                            .get(c)
                            .map(|n| {
                                matches!(&n.kind, NodeType::Element(el) if el.tag == "li")
                            })
                            .unwrap_or(false);
                        if is_li {
                            i += 1;
                        }
                        walk(
                            ctx,
                            c,
                            line,
                            st.font_size,
                            depth + 1,
                            href_ref,
                            st.color,
                            parent_fade * st.opacity,
                            0,
                            if tag == "ol" && is_li { i } else { 0 },
                        );
                    }
                    flush_line(ctx, line);
                }
                _ => {
                    if st.display == Display::Block {
                        flush_line(ctx, line);
                        let scale = ctx.scale;
                        // CSS box margins in device px. Percentages resolve
                        // against the viewport (the containing width), like
                        // CSS; the old layout never applied margins at all,
                        // so every block sat flush against the next.
                        let ml = st.margin[3].resolve(ctx.viewport_w as f32, st.font_size.max(1.0)) as i64;
                        let mr = st.margin[1].resolve(ctx.viewport_w as f32, st.font_size.max(1.0)) as i64;
                        let mt = st.margin[0].resolve(ctx.viewport_w as f32, st.font_size.max(1.0)) as i64;
                        let mb = st.margin[2].resolve(ctx.viewport_w as f32, st.font_size.max(1.0)) as i64;
                        let pl = st.padding[3].resolve(ctx.viewport_w as f32, st.font_size.max(1.0)) as i64;
                        ctx.content_height += mt;
                        // Left edge for this block's own content.
                        let saved_left = ctx.line_left;
                        let saved_right = ctx.line_right;
                        let x0 = saved_left + ml;
                        ctx.line_left = x0 + pl;
                        ctx.line_right = (ctx.viewport_w - mr - ctx.scale).max(ctx.line_left);
                        let start_h = ctx.content_height;
                        let w = block_width(&st, ctx.line_right - ctx.line_left, scale);
                        // Recurse children.
                        let mut inner_line = Line {
                            words: Vec::new(),
                            bg: None,
                            underline_word_idx: Vec::new(),
                            trailing_space: false,
                        };
                        for &c in &children {
                            walk(
                                ctx,
                                c,
                                &mut inner_line,
                                st.font_size,
                                depth + 1,
                                href_ref,
                                st.color,
                                // Blocks multiply their own opacity into
                                // the subtree chain (CSS behavior).
                                parent_fade * st.opacity,
                                0,
                                0,
                            );
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
                        ctx.content_height += mb;
                        // Restore for following siblings.
                        ctx.line_left = saved_left;
                        ctx.line_right = saved_right;
                    } else {
                        // Inline element: recurse with own font style and
                        // resolved text color (CSS `color` now reaches paint).
                        for &c in &children {
                            walk(
                                ctx,
                                c,
                                line,
                                st.font_size,
                                depth + 1,
                                href_ref,
                                st.color,
                                // Inline chains accumulate fade and offsets.
                                parent_fade * st.opacity,
                                vofs
                                    + if tag == "sub" {
                                        3 * ctx.scale
                                    } else if tag == "sup" {
                                        -3 * ctx.scale
                                    } else {
                                        0
                                    },
                                ol_index,
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Case-insensitive attribute lookup on a raw attr list (used before the
/// per-element `get_attr` closure exists in `walk`).
fn get_attr2<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn style_for(ctx: &LayoutCtx, id: NodeId, parent_font: f32) -> ComputedStyle {
    compute_style(ctx.dom, ctx.sheet, id, ctx.hover, parent_font)
}

/// Glyph cell scale for a CSS font size at the current UI zoom.
///
/// The bitmap font is 8px per cell at 1x, so the logical scale is
/// font_px / 8, and zoom multiplies it (like a browser at 150%):
/// the old `max(font_scale, zoom)` formula never enlarged headings,
/// collapsed every size to one value at zoom 4, and forced 200% text
/// at zoom 1. Result is clamped to keep lines readable on small
/// viewports.
fn font_scale(font_px: f32, base: i64) -> i64 {
    let s = ((font_px / 8.0) * base.max(1) as f32).round() as i64;
    s.clamp(1, 6)
}

/// Block content width: `avail_w` is the width available to this block's
/// content (margins/padding/gutter already excluded by the caller).
fn block_width(st: &ComputedStyle, avail_w: i64, _scale: i64) -> i64 {
    match st.width {
        Length::Auto => avail_w,
        Length::Px(px) => (px as i64).clamp(16, avail_w),
        Length::Percent(p) => (avail_w as f32 * p / 100.0) as i64,
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
        line.trailing_space = false;
        return;
    }
    let scale = ctx.scale;
    let max_scale = line.words.iter().map(|w| w.scale).max().unwrap_or(1);
    let line_h = 10 * max_scale;
    let y = ctx.content_height;

    let mut total: i64 = 0;
    for (i, w) in line.words.iter().enumerate() {
        if i > 0 && w.has_leading_space {
            total += advance(' ', line.words[i - 1].scale.max(w.scale));
        }
        total += w.w;
    }
    let avail = (ctx.line_right - ctx.line_left).max(0);
    let align = line.words[0].align;
    let start_x = match align {
        TextAlign::Left => ctx.line_left,
        TextAlign::Center => ctx.line_left + ((avail - total) / 2).max(0),
        TextAlign::Right => ctx.line_left + (avail - total).max(0),
    };
    let mut cx = start_x;
    for i in 0..line.words.len() {
        if i > 0 && line.words[i].has_leading_space {
            cx += advance(' ', line.words[i - 1].scale.max(line.words[i].scale));
        }
        line.words[i].x = cx;
        line.words[i].y = y + (max_scale - line.words[i].scale) * 8;
        cx += line.words[i].w;
    }

    ctx.lines.push(std::mem::replace(
        line,
        Line {
            words: Vec::new(),
            bg: None,
            underline_word_idx: Vec::new(),
            trailing_space: false,
        },
    ));
    ctx.content_height = y + line_h + 2 * scale;
}

/// Paint a laid-out document. Called every frame or on dirty.
///
/// `highlight` is the find-in-page query: every matching word is painted on
/// top of a marker so matches are visible on the page, not just counted.
pub fn paint(frame: &mut Frame, layout: &LayoutResult, scroll_y: i64, highlight: Option<&str>) {
    // Page canvas: the CSS body background (white when the page doesn't
    // set one), so dark themes render as designed.
    frame.clear(layout.page_bg);
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
            let y = w.y + w.dy - scroll_y;
            if y + 10 * w.scale < -32 || y > frame.height as i64 + 32 {
                continue;
            }
            // Effective color: links stay link-blue; everything else can
            // fade toward the page background by the word's opacity
            // product (CSS `opacity` chain).
            let base = if w.link.is_some() {
                Color { r: 0, g: 0, b: 238, a: 1.0 }
            } else {
                w.color
            };
            let col = if w.fade >= 0.999 {
                base
            } else {
                blend_to_bg(base, layout.page_bg, w.fade)
            };
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
                draw_text_italic(frame, &w.text, w.x, y, w.scale, col, w.italic);
                frame.fill_rect(w.x, y + 9 * w.scale, w.w, w.scale, col);
            } else {
                // Text color resolved from CSS (Word.color), no longer
                // hardcoded black.
                draw_text_italic(frame, &w.text, w.x, y, w.scale, col, w.italic);
                if line.underline_word_idx.contains(&idx) {
                    frame.fill_rect(w.x, y + 9 * w.scale, w.w, w.scale, col);
                }
            }
            if w.strike {
                // Line-through: a bar through the glyph mid-height.
                let mid = y + 4 * w.scale;
                frame.fill_rect(w.x, mid, w.w, w.scale.max(1), col);
            }
        }
    }
}

/// Hit-test words for mouse clicks; returns link href.
pub fn hit_test(layout: &LayoutResult, x: i64, y: i64, scroll_y: i64) -> Option<String> {
    for line in &layout.lines {
        for w in &line.words {
            let wy = w.y + w.dy - scroll_y;
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
    fn css_margins_create_vertical_spacing() {
        // Two paragraphs with UA margins (1em) must be separated by real
        // vertical space — the old layout ignored margins entirely and
        // rendered pages as a text wall.
        let dom = parse_html("<p>alpha</p><p>beta</p>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 2);
        assert!(res.lines.len() >= 2, "two paragraphs must lay out");
        // font_scale(16px, zoom 2) = 4 cells, so a text line is 10*4 tall
        // and flush_line adds 2*scale of separation. Without UA margins the
        // paragraph gap is exactly line_h + sep; with margins it must be
        // strictly larger by at least one 1em margin.
        let font_scale = (16 / 8) * 2; // = 4
        let line_h = 10 * font_scale;
        let sep = 2 * 2;
        let gap = res.lines[1].words[0].y - res.lines[0].words[0].y;
        assert!(
            gap >= line_h + sep + 16,
            "paragraph gap {gap} must include the 1em UA margin (line height {line_h} + {sep} separation + 16 margin)"
        );
    }

    #[test]
    fn css_block_margin_left_indents_text() {
        let dom = parse_html("<p>normal</p><p style=\"margin-left:40px\">indented</p>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        assert!(res.lines.len() >= 2);
        let x0 = res.lines[0].words[0].x;
        let x1 = res.lines[1].words[0].x;
        assert!(
            x1 >= x0 + 39,
            "margin-left:40px must indent (x0={x0}, x1={x1})"
        );
    }

    #[test]
    fn italics_slant_the_glyphs() {
        // `<i>` must reach the painter as a slanted draw: some pixels the
        // upright draw leaves dark must lighten, and vice versa.
        let dom = parse_html("<p><i>Wmmm</i></p>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 4);
        let mut f1 = Frame::new(400, 80);
        paint(&mut f1, &res, 0, None);
        // Same word, upright: the slant shifts pixels horizontally per
        // row, so the per-COLUMN dark profile must differ (a pure row
        // count is translation-invariant).
        let dom2 = parse_html("<p>Wmmm</p>");
        let res2 = layout(&dom2, &sheet, &hover, 400, 4);
        let mut f2 = Frame::new(400, 80);
        paint(&mut f2, &res2, 0, None);
        let col_profile = |f: &Frame| -> Vec<usize> {
            (0..400)
                .map(|x| {
                    (0..80)
                        .filter(|y| f.pixels[(y * 400 + x) * 4] < 64)
                        .count()
                })
                .collect()
        };
        assert_ne!(
            col_profile(&f1),
            col_profile(&f2),
            "italic draw must shift pixels column-wise"
        );
    }

    #[test]
    fn opacity_fades_words_toward_the_background() {
        let dom = parse_html("<p style=\"opacity:0.2\">faded</p>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 2);
        let mut f = Frame::new(300, 60);
        paint(&mut f, &res, 0, None);
        // Faded #202020 on white cannot stay dark.
        assert!(
            !f.pixels.chunks_exact(4).any(|p| p[0] < 64 && p[1] < 64),
            "opacity 0.2 must not leave dark glyph pixels"
        );
    }

    #[test]
    fn line_through_paints_a_bar() {
        let dom = parse_html("<s>gone</s>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 2);
        let mut f = Frame::new(300, 60);
        paint(&mut f, &res, 0, None);
        // A strike bar creates a full-width dark run at one row band.
        let dark: Vec<usize> = (0..60)
            .map(|y| {
                (0..300)
                    .filter(|x| f.pixels[(y * 300 + x) * 4] < 64)
                    .count()
            })
            .collect();
        assert!(
            dark.iter().any(|&c| c > 30),
            "expected a wide strike bar row"
        );
    }

    #[test]
    fn text_decoration_none_clears_underline() {
        // The UA sheet gives <u> an underline; an inline `none` on the
        // SAME element must clear it (token-level, not substring).
        let dom = parse_html("<u style=\"text-decoration: none\">xx</u>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 2);
        let mut f = Frame::new(300, 60);
        paint(&mut f, &res, 0, None);
        // With only glyphs left, no row has a full word-width dark bar.
        let dark: Vec<usize> = (0..60)
            .map(|y| {
                (0..300)
                    .filter(|x| f.pixels[(y * 300 + x) * 4] < 64)
                    .count()
            })
            .collect();
        assert!(
            dark.iter().all(|&c| c <= 20),
            "underline bar must be cleared by text-decoration: none"
        );
    }

    #[test]
    fn wide_chars_advance_two_cells() {
        let w = text_width("\u{4e2d}", 1); // U+4E2D CJK ideograph
        assert_eq!(w, 17, "wide char must advance 16+1");
        let w2 = text_width("ab", 1);
        assert_eq!(w2, 18, "two narrow chars 2*(8+1)");
    }

    #[test]
    fn tables_produce_aligned_cell_columns() {
        let dom = parse_html(
            "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>",
        );
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        // Cells carry borders (BoxOut) and words align in two columns.
        assert!(!res.boxes.is_empty(), "table cells must draw boxes");
        let words: Vec<&str> = res
            .lines
            .iter()
            .flat_map(|l| l.words.iter().map(|w| w.text.as_str()))
            .collect();
        for t in ["a", "b", "c", "d"] {
            assert!(words.contains(&t), "cell text {t} must render");
        }
    }

    #[test]
    fn ol_numbers_items() {
        let dom = parse_html("<ol><li>one</li><li>two</li></ol>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        let words: Vec<&str> = res
            .lines
            .iter()
            .flat_map(|l| l.words.iter().map(|w| w.text.as_str()))
            .collect();
        assert!(words.iter().any(|t| t == "1."), "ol must number items");
        assert!(words.iter().any(|t| t == "2."));
    }

    #[test]
    fn form_controls_draw_boxes() {
        let dom = parse_html(
            "<p><input value=\"type here\"><button>Send</button></p>",
        );
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        assert!(res.boxes.len() >= 2, "input and button must draw boxes");
        let words: Vec<&str> = res
            .lines
            .iter()
            .flat_map(|l| l.words.iter().map(|w| w.text.as_str()))
            .collect();
        assert!(words.contains(&"type here"));
        assert!(words.contains(&"Send"));
    }

    #[test]
    fn pre_preserves_line_breaks() {
        let dom = parse_html("<pre>line1\nline2</pre>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        let ys: Vec<i64> = res
            .lines
            .iter()
            .filter(|l| !l.words.is_empty())
            .map(|l| l.words[0].y)
            .collect();
        assert!(
            ys.len() >= 2 && ys[1] > ys[0],
            "pre must break lines, got ys={ys:?}"
        );
    }

    #[test]
    fn details_children_hidden_until_open() {
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let closed = parse_html("<details><summary>S</summary><p>secret</p></details>");
        let res = layout(&closed, &sheet, &hover, 400, 1);
        let txt: String = res
            .lines
            .iter()
            .flat_map(|l| l.words.iter().map(|w| w.text.clone()))
            .collect();
        assert!(!txt.contains("secret"), "closed details hide children");
        let open = parse_html("<details open><summary>S</summary><p>secret</p></details>");
        let res2 = layout(&open, &sheet, &hover, 400, 1);
        let txt2: String = res2
            .lines
            .iter()
            .flat_map(|l| l.words.iter().map(|w| w.text.clone()))
            .collect();
        assert!(txt2.contains("secret"), "open details show children");
    }

    #[test]
    fn fragment_anchors_resolve_to_scroll_positions() {
        let dom = parse_html(
            "<p>intro</p><h2 id=\"target\">Chapter</h2><p>body text here</p>",
        );
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        assert!(
            res.anchors.contains_key("target"),
            "id=target must be recorded"
        );
        assert!(res.anchors["target"] > 0);
    }

    #[test]
    fn noscript_content_renders() {
        // A JS-less engine must SHOW noscript fallback content, not hide it.
        let dom = parse_html(
            "<body><noscript>Please enable scripts or read this text.</noscript></body>",
        );
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        let shown: String = res
            .lines
            .iter()
            .flat_map(|l| l.words.iter().map(|w| w.text.clone()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            shown.contains("read this text"),
            "noscript fallback text must render, got: {shown}"
        );
    }

    #[test]
    fn paint_does_not_panic_and_draws() {
        let dom = parse_html("<p style=\"background:#eee\">boxed text <a href='x'>link</a></p>");
        let sheet = parse_stylesheet("p { background: #eeeeee; }");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        let mut frame = Frame::new(400, 300);
        paint(&mut frame, &res, 0, None);
        // Text paints #202020 (BGRA 32,32,32): any dark glyph pixel.
        assert!(frame.pixels.chunks_exact(4).any(|p| p[0] < 64 && p[1] < 64));

        // Find-in-page highlighting paints the marker behind matching words.
        let hits = find_matches(&res, "link");
        assert_eq!(hits.len(), 1);
        paint(&mut frame, &res, 0, Some("link"));
        // Frame is BGRA8: yellow (255,235,59) is stored as [59,235,255].
        assert!(frame.pixels.chunks_exact(4).any(|p| p[0] == 59 && p[1] == 235));
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
    fn glyphs_are_not_mirrored() {
        // font8x8 is LSB-first: bit 0 is the leftmost column, so '(' bulges
        // LEFT. Guards against an MSB-first regression, which rendered every
        // glyph mirrored (left=2/right=12 for this glyph before the fix).
        let mut frame = Frame::new(16, 8);
        draw_text(&mut frame, "(", 0, 0, 1, Color::BLACK);
        let mut left = 0usize;
        let mut right = 0usize;
        for y in 0..8usize {
            for x in 0..8usize {
                let on = frame.pixels[(y * 16 + x) * 4] == 0; // BGRA black
                if x < 4 && on {
                    left += 1;
                }
                if x >= 4 && on {
                    right += 1;
                }
            }
        }
        assert!(left > right, "glyph '(' renders mirrored: left={left} right={right}");
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

    #[test]
    fn renders_spanish_accents_and_li() {
        let dom = parse_html("<ul><li>¿Cómo estás? ¡Muy bien, niño!</li></ul>");
        let sheet = parse_stylesheet("");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 800, 2);
        assert!(!res.lines.is_empty());
        let mut frame = Frame::new(800, 200);
        paint(&mut frame, &res, 0, None);
        assert!(frame.pixels.chunks_exact(4).any(|p| p[0] < 64 && p[1] < 64));
    }

    #[test]
    fn css_colors_reach_the_painter() {
        // CSS `color` on elements must inherit to their text, and the body
        // background must become the page canvas — both were previously
        // computed and then ignored by the painter.
        let dom = parse_html("<body><p class=\"warn\">alert</p></body>");
        let sheet = parse_stylesheet("body { background-color: #101820; } p { color: #FF8800; }");
        let hover = Default::default();
        let res = layout(&dom, &sheet, &hover, 400, 1);
        assert_eq!(
            res.page_bg,
            Color {
                r: 16,
                g: 24,
                b: 32,
                a: 1.0
            }
        );
        let mut frame = Frame::new(400, 200);
        paint(&mut frame, &res, 0, None);
        // Orange text stored BGRA: [0x00, 0x88, 0xFF].
        assert!(
            frame
                .pixels
                .chunks_exact(4)
                .any(|p| p[0] == 0 && p[1] == 0x88 && p[2] == 0xFF),
            "CSS text color did not reach the painter"
        );
        // Canvas painted with the body background, BGRA [32, 24, 16].
        assert!(
            frame
                .pixels
                .chunks_exact(4)
                .any(|p| p[0] == 32 && p[1] == 24 && p[2] == 16),
            "body background did not become the page canvas"
        );
    }
}
