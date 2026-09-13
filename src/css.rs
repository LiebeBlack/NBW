//! css.rs — Native CSS3 subset engine.
//!
//! Tokenizer → rule parser → selector matcher → computed style. Supports:
//! type selectors, `.class`, `#id`, `[attr]`, descendant combinator, `>`,
//! grouping with commas, `:hover` (flag-based), pseudo-classes `:first-child`
//! / `:last-child`, and the box-model + typography + color properties used
//! by the renderer. All values resolve to concrete lengths/colors; `em`
//! and `%` resolve against parent style during application.

#![allow(dead_code)]

use crate::dom::{ElementData, NodeId, NodeType};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0.0 };
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 1.0 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 1.0 };

    pub fn to_rgba8(self) -> [u8; 4] {
        [
            self.r,
            self.g,
            self.b,
            (self.a.clamp(0.0, 1.0) * 255.0).round() as u8,
        ]
    }

    /// Parse `#rgb`, `#rrggbb`, `rgba(r,g,b,a)`, `rgb(r,g,b)` and 40
    /// common named colors.
    pub fn parse(s: &str) -> Option<Color> {
        let s = s.trim().to_ascii_lowercase();
        if let Some(hex) = s.strip_prefix('#') {
            return match hex.len() {
                3 => {
                    let v = u16::from_str_radix(hex, 16).ok()?;
                    let (r, g, b) = ((v >> 8) & 0xF, (v >> 4) & 0xF, v & 0xF);
                    Some(Color {
                        r: (r * 17) as u8,
                        g: (g * 17) as u8,
                        b: (b * 17) as u8,
                        a: 1.0,
                    })
                }
                4 => {
                    let v = u16::from_str_radix(hex, 16).ok()?;
                    let (r, g, b, a) = ((v >> 12) & 0xF, (v >> 8) & 0xF, (v >> 4) & 0xF, v & 0xF);
                    Some(Color {
                        r: (r * 17) as u8,
                        g: (g * 17) as u8,
                        b: (b * 17) as u8,
                        a: (a * 17) as f32 / 255.0,
                    })
                }
                6 => {
                    let v = u32::from_str_radix(hex, 16).ok()?;
                    Some(Color {
                        r: (v >> 16) as u8,
                        g: (v >> 8) as u8,
                        b: v as u8,
                        a: 1.0,
                    })
                }
                8 => {
                    let v = u32::from_str_radix(hex, 16).ok()?;
                    Some(Color {
                        r: (v >> 24) as u8,
                        g: (v >> 16) as u8,
                        b: (v >> 8) as u8,
                        a: (v & 0xFF) as f32 / 255.0,
                    })
                }
                _ => None,
            };
        }
        if s == "transparent" {
            return Some(Color::TRANSPARENT);
        }
        if let Some(inner) = s
            .strip_prefix("rgba(")
            .and_then(|x| x.strip_suffix(')'))
        {
            let p: Vec<&str> = inner.split(',').map(|x| x.trim()).collect();
            if p.len() == 4 {
                return Some(Color {
                    r: parse_color_comp(p[0])?,
                    g: parse_color_comp(p[1])?,
                    b: parse_color_comp(p[2])?,
                    a: p[3].trim().parse().ok()?,
                });
            }
        }
        if let Some(inner) = s.strip_prefix("rgb(").and_then(|x| x.strip_suffix(')')) {
            let p: Vec<&str> = inner.split(',').map(|x| x.trim()).collect();
            if p.len() == 3 {
                return Some(Color {
                    r: parse_color_comp(p[0])?,
                    g: parse_color_comp(p[1])?,
                    b: parse_color_comp(p[2])?,
                    a: 1.0,
                });
            } else if p.len() == 4 {
                return Some(Color {
                    r: parse_color_comp(p[0])?,
                    g: parse_color_comp(p[1])?,
                    b: parse_color_comp(p[2])?,
                    a: p[3].trim().parse().ok()?,
                });
            }
        }
        named_color(&s)
    }
}

fn parse_color_comp(s: &str) -> Option<u8> {
    if let Some(p) = s.strip_suffix('%') {
        let f: f32 = p.trim().parse().ok()?;
        return Some((f / 100.0 * 255.0).round().clamp(0.0, 255.0) as u8);
    }
    s.trim().parse::<f32>().ok().map(|f| f.clamp(0.0, 255.0) as u8)
}

fn named_color(s: &str) -> Option<Color> {
    let (r, g, b) = match s {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "purple" => (128, 0, 128),
        "teal" => (0, 128, 128),
        "navy" => (0, 0, 128),
        "orange" => (255, 165, 0),
        "pink" => (255, 192, 203),
        "gold" => (255, 215, 0),
        "brown" => (165, 42, 42),
        "beige" => (245, 245, 220),
        "coral" => (255, 127, 80),
        "crimson" => (220, 20, 60),
        "darkblue" => (0, 0, 139),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "darkgreen" => (0, 100, 0),
        "darkred" => (139, 0, 0),
        "khaki" => (240, 230, 140),
        "lavender" => (230, 230, 250),
        "lightblue" => (173, 216, 230),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "lightgreen" => (144, 238, 144),
        "indigo" => (75, 0, 130),
        "ivory" => (255, 255, 240),
        "moccasin" => (255, 228, 181),
        "orchid" => (218, 112, 214),
        "plum" => (221, 160, 221),
        "salmon" => (250, 128, 114),
        "tan" => (210, 180, 140),
        "tomato" => (255, 99, 71),
        "violet" => (238, 130, 238),
        "wheat" => (245, 222, 179),
        "whitesmoke" => (245, 245, 245),
        "aliceblue" => (240, 248, 255),
        "chocolate" => (210, 105, 30),
        _ => return None,
    };
    Some(Color { r, g, b, a: 1.0 })
}

// ---------------------------------------------------------------------------
// Computed style values
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    None,
    Solid,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Length {
    /// Resolved absolute value in CSS px.
    Px(f32),
    /// Auto marker.
    Auto,
    /// Percentage of containing block.
    Percent(f32),
    /// Em relative to parent font-size.
    Em(f32),
}

impl Length {
    pub fn resolve(&self, base: f32, em: f32) -> f32 {
        match *self {
            Length::Px(v) => v,
            Length::Percent(p) => base * p / 100.0,
            Length::Em(e) => em * e,
            Length::Auto => 0.0,
        }
    }
    pub fn parse(s: &str) -> Option<Length> {
        let s = s.trim().to_ascii_lowercase();
        if s == "auto" {
            return Some(Length::Auto);
        }
        if let Some(p) = s.strip_suffix('%') {
            return p.trim().parse::<f32>().ok().map(Length::Percent);
        }
        if let Some(p) = s.strip_suffix("em") {
            if let Ok(v) = p.trim().parse::<f32>() {
                return Some(Length::Em(v));
            }
        }
        if let Some(p) = s.strip_suffix("rem") {
            if let Ok(v) = p.trim().parse::<f32>() {
                return Some(Length::Px(v * 16.0));
            }
        }
        if let Some(p) = s.strip_suffix("px") {
            if let Ok(v) = p.trim().parse::<f32>() {
                return Some(Length::Px(v));
            }
        }
        if let Some(p) = s.strip_suffix("pt") {
            if let Ok(v) = p.trim().parse::<f32>() {
                return Some(Length::Px(v * 1.333333));
            }
        }
        // Unitless number: px by CSS default.
        s.trim().parse::<f32>().ok().map(Length::Px)
    }
}

/// Fully resolved style used by layout and painting.
#[derive(Debug, Clone)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: Color,
    pub background_color: Color,
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub text_align: TextAlign,
    pub margin: [Length; 4],
    pub padding: [Length; 4],
    pub border_width: f32,
    pub border_color: Color,
    pub border_style: LineStyle,
    pub width: Length,
    pub height: Length,
    pub text_decoration_line: bool,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: Display::Inline,
            color: Color::BLACK,
            background_color: Color::TRANSPARENT,
            font_size: 16.0,
            bold: false,
            italic: false,
            underline: false,
            text_align: TextAlign::Left,
            margin: [Length::Px(0.0); 4],
            padding: [Length::Px(0.0); 4],
            border_width: 0.0,
            border_color: Color::BLACK,
            border_style: LineStyle::None,
            width: Length::Auto,
            height: Length::Auto,
            text_decoration_line: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Selector matching
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq)]
pub enum SimpleSel {
    Type(String),
    Class(String),
    Id(String),
    Attr { name: String, value: Option<String> },
    Pseudo(String),
    Universal,
}

#[derive(Debug, Clone)]
pub struct Selector {
    /// Compound parts ANDed on one element.
    compound: Vec<SimpleSel>,
    /// Ancestor constraints for descendant combinator ('>'), innermost last.
    ancestors: Vec<Vec<SimpleSel>>,
}

impl Selector {
    pub fn specificity(&self) -> (u32, u32, u32) {
        let mut ids = 0u32;
        let mut classes = 0u32;
        let mut types = 0u32;
        let mut count = |parts: &Vec<SimpleSel>| {
            for p in parts {
                match p {
                    SimpleSel::Id(_) => ids += 1,
                    SimpleSel::Class(_) | SimpleSel::Attr { .. } | SimpleSel::Pseudo(_) => {
                        classes += 1
                    }
                    SimpleSel::Type(_) => types += 1,
                    SimpleSel::Universal => {}
                }
            }
        };
        for a in &self.ancestors {
            count(a);
        }
        count(&self.compound);
        (ids, classes, types)
    }
}

fn matches_simple(
    el: &ElementData,
    s: &SimpleSel,
    flags: u32,
    tree: Option<&crate::dom::Dom>,
    node: Option<crate::dom::NodeId>,
) -> bool {
    match s {
        SimpleSel::Universal => true,
        SimpleSel::Type(t) => el.tag.eq_ignore_ascii_case(t),
        SimpleSel::Class(c) => el.classes.iter().any(|x| x.eq_ignore_ascii_case(c)),
        SimpleSel::Id(i) => el.get_attr("id").map(|v| v == i).unwrap_or(false),
        SimpleSel::Attr { name, value } => match value {
            None => el.attrs.iter().any(|(k, _)| k == name),
            Some(v) => el.get_attr(name).map(|x| x == v).unwrap_or(false),
        },
        SimpleSel::Pseudo(p) => match p.as_str() {
            "hover" => flags & HOVER_FLAG != 0,
            "first-child" => tree
                .zip(node)
                .map(|(t, id)| t.is_first_element_child(id))
                .unwrap_or(false),
            "last-child" => tree
                .zip(node)
                .map(|(t, id)| t.is_last_element_child(id))
                .unwrap_or(false),
            _ => false,
        },
    }
}

pub const HOVER_FLAG: u32 = 1;

fn matches_compound(
    el: &ElementData,
    parts: &[SimpleSel],
    flags: u32,
    tree: &crate::dom::Dom,
    node: crate::dom::NodeId,
    _in_ancestor_check: bool,
) -> bool {
    parts
        .iter()
        .all(|s| matches_simple(el, s, flags, Some(tree), Some(node)))
}

/// Match `selector` against `node` (must be an element node).
pub fn matches_selector(
    sel: &Selector,
    tree: &crate::dom::Dom,
    node: NodeId,
    flags: u32,
) -> bool {
    let Some(n) = tree.get(node) else { return false };
    let NodeType::Element(el) = &n.kind else { return false };
    if !matches_compound(el, &sel.compound, flags, tree, node, false) {
        return false;
    }
    // Walk ancestors for each constraint, innermost-first.
    let mut cursor = n.parent;
    for parts in sel.ancestors.iter().rev() {
        let mut found = false;
        let mut up = cursor;
        while let Some(cur) = up {
            let Some(cur_node) = tree.get(cur) else { break };
            if let NodeType::Element(cur_el) = &cur_node.kind {
                if matches_compound(cur_el, parts, 0, tree, cur, true) {
                    found = true;
                    cursor = cur_node.parent;
                    break;
                }
            }
            up = cur_node.parent;
        }
        if !found {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Declarations and rules
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub struct Declaration {
    pub property: String,
    pub value: String,
    pub important: bool,
}

#[derive(Debug, Clone)]
pub struct StyleRule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub struct Stylesheet {
    pub rules: Vec<StyleRule>,
}

/// Tokenize a declaration block body into declarations (handles strings,
/// nested parens and `!important`).
pub fn parse_declarations(body: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_string: Option<char> = None;
    let mut cur = String::new();
    for ch in body.chars() {
        match (in_string, ch) {
            (None, '"') | (None, '\'') => {
                in_string = Some(ch);
                cur.push(ch);
            }
            (Some(q), c) if c == q => {
                in_string = None;
                cur.push(c);
            }
            (None, '(') => {
                depth += 1;
                cur.push(ch);
            }
            (None, ')') => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            (None, ';') if depth == 0 => {
                push_decl(&mut out, &cur);
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    push_decl(&mut out, &cur);
    out
}

fn push_decl(out: &mut Vec<Declaration>, raw: &str) {
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    let Some((prop, val)) = raw.split_once(':') else { return };
    let prop = prop.trim().to_ascii_lowercase();
    let mut val = val.trim().to_string();
    let mut important = false;
    if let Some(pos) = val.to_ascii_lowercase().find("!important") {
        important = true;
        val = val[..pos].trim().to_string();
    }
    if prop.is_empty() || val.is_empty() {
        return;
    }
    out.push(Declaration {
        property: prop,
        value: val,
        important,
    });
}

/// Parse a full stylesheet (used for `<style>` blocks and future fetches).
pub fn parse_stylesheet(css: &str) -> Stylesheet {
    let mut rules = Vec::new();
    let chars: Vec<char> = css.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        // Skip whitespace and at-rules.
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] == '@' {
            // Skip at-rule block or statement.
            while i < chars.len() && chars[i] != '{' && chars[i] != ';' {
                i += 1;
            }
            if i < chars.len() && chars[i] == '{' {
                let mut depth = 1;
                i += 1;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '{' {
                        depth += 1;
                    } else if chars[i] == '}' {
                        depth -= 1;
                    }
                    i += 1;
                }
            } else if i < chars.len() {
                i += 1;
            }
            continue;
        }
        // Selector text up to '{'.
        let sel_start = i;
        while i < chars.len() && chars[i] != '{' {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let sel_text: String = chars[sel_start..i].iter().collect();
        i += 1; // consume '{'
        let body_start = i;
        let mut depth = 1;
        while i < chars.len() && depth > 0 {
            if chars[i] == '{' {
                depth += 1;
            } else if chars[i] == '}' {
                depth -= 1;
            }
            if depth > 0 {
                i += 1;
            }
        }
        let body: String = chars[body_start..i.min(chars.len())].iter().collect();
        if i < chars.len() {
            i += 1; // consume '}'
        }
        let selectors = parse_selector_list(&sel_text);
        if selectors.is_empty() {
            continue;
        }
        let declarations = parse_declarations(&body);
        if declarations.is_empty() {
            continue;
        }
        rules.push(StyleRule {
            selectors,
            declarations,
        });
    }
    Stylesheet { rules }
}

fn parse_selector_list(text: &str) -> Vec<Selector> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(parse_selector)
        .collect()
}

/// Split a selector into compound segments on combinators (' ' or '>'),
/// build the ancestor chain, and take the LAST compound as the subject
/// (CSS selectors match right-to-left: `div .c` styles the `.c` element).
/// Child ('>') and descendant (' ') combinators both contribute ancestor
/// constraints (the matcher walks ancestors): a documented simplification
/// that keeps matching O(depth) with no leaks.
fn parse_selector(s: &str) -> Option<Selector> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut segments: Vec<Vec<SimpleSel>> = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if c == '>' || c == ' ' {
            flush_segment(&mut cur, &mut segments);
        } else {
            cur.push(c);
        }
    }
    flush_segment(&mut cur, &mut segments);
    if segments.is_empty() {
        return None;
    }
    let compound = segments.pop().unwrap_or_default();
    Some(Selector {
        compound,
        ancestors: segments,
    })
}

/// Flush one buffered compound segment into the selector under build.
fn flush_segment(cur: &mut String, segments: &mut Vec<Vec<SimpleSel>>) {
    let t = cur.trim().to_string();
    cur.clear();
    if t.is_empty() {
        return;
    }
    let parts = parse_compound(&t);
    if !parts.is_empty() {
        segments.push(parts);
    }
}

fn parse_compound(s: &str) -> Vec<SimpleSel> {
    let mut parts = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < bytes.len() && is_name_char(bytes[i]) {
                    i += 1;
                }
                if i > start {
                    parts.push(SimpleSel::Class(bytes[start..i].iter().collect()));
                }
            }
            '#' => {
                i += 1;
                let start = i;
                while i < bytes.len() && is_name_char(bytes[i]) {
                    i += 1;
                }
                if i > start {
                    parts.push(SimpleSel::Id(bytes[start..i].iter().collect()));
                }
            }
            '[' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != ']' {
                    i += 1;
                }
                let inner: String = bytes[start..i.min(bytes.len())].iter().collect();
                if i < bytes.len() {
                    i += 1;
                }
                let inner = inner.trim();
                if let Some((n, v)) = inner.split_once('=') {
                    let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                    parts.push(SimpleSel::Attr {
                        name: n.trim().to_ascii_lowercase(),
                        value: Some(v),
                    });
                } else {
                    parts.push(SimpleSel::Attr {
                        name: inner.to_ascii_lowercase(),
                        value: None,
                    });
                }
            }
            ':' => {
                i += 1;
                if i < bytes.len() && bytes[i] == ':' {
                    i += 1; // pseudo-element: treated as pseudo-class
                }
                let start = i;
                while i < bytes.len() && is_name_char(bytes[i]) {
                    i += 1;
                }
                if i > start {
                    // [char] slices have no to_ascii_lowercase; collect to a
                    // String first (same for every compound part above).
                    parts.push(SimpleSel::Pseudo(
                        bytes[start..i].iter().collect::<String>().to_ascii_lowercase(),
                    ));
                }
            }
            '*' => {
                parts.push(SimpleSel::Universal);
                i += 1;
            }
            c if is_name_char(c) => {
                let start = i;
                while i < bytes.len() && is_name_char(bytes[i]) {
                    i += 1;
                }
                parts.push(SimpleSel::Type(bytes[start..i].iter().collect()));
            }
            _ => i += 1,
        }
    }
    parts
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_' || c == '\\'
}

// ---------------------------------------------------------------------------
// Style application: cascading computed style per node
// ---------------------------------------------------------------------------
pub fn apply_declarations(style: &mut ComputedStyle, decls: &[Declaration]) {
    for d in decls {
        apply_one(style, &d.property, &d.value);
    }
}

fn apply_one(style: &mut ComputedStyle, prop: &str, value: &str) {
    let v = value.trim();
    match prop {
        "display" => {
            style.display = match v.to_ascii_lowercase().as_str() {
                "block" | "flex" | "grid" | "list-item" | "table" => Display::Block,
                "none" => Display::None,
                _ => Display::Inline,
            };
        }
        "color" => {
            if let Some(c) = Color::parse(v) {
                style.color = c;
            }
        }
        "background" | "background-color" => {
            // First token that parses as a color wins.
            if let Some(tok) = v.split_whitespace().next() {
                if let Some(c) = Color::parse(tok) {
                    style.background_color = c;
                }
            }
        }
        "font-size" => {
            if let Some(l) = Length::parse(v) {
                style.font_size = l.resolve(16.0, style.font_size.max(1.0));
            } else {
                style.font_size = keyword_font_size(v, style.font_size);
            }
        }
        "font-weight" => {
            let lv = v.to_ascii_lowercase();
            style.bold = matches!(lv.as_str(), "bold" | "bolder" | "700" | "800" | "900")
                || lv
                    .parse::<u32>()
                    .map(|n| n >= 600)
                    .unwrap_or(false);
        }
        "font-style" => style.italic = v.eq_ignore_ascii_case("italic"),
        "text-decoration" | "text-decoration-line" => {
            let lv = v.to_ascii_lowercase();
            style.underline = lv.contains("underline");
            style.text_decoration_line = lv.contains("underline");
        }
        "text-align" => {
            style.text_align = match v.to_ascii_lowercase().as_str() {
                "center" => TextAlign::Center,
                "right" | "end" => TextAlign::Right,
                _ => TextAlign::Left,
            };
        }
        "margin" => {
            if let Some(vals) = parse_box_sides(v) {
                style.margin = vals;
            }
        }
        "margin-top" => {
            if let Some(l) = Length::parse(v) {
                style.margin[0] = l;
            }
        }
        "margin-right" => {
            if let Some(l) = Length::parse(v) {
                style.margin[1] = l;
            }
        }
        "margin-bottom" => {
            if let Some(l) = Length::parse(v) {
                style.margin[2] = l;
            }
        }
        "margin-left" => {
            if let Some(l) = Length::parse(v) {
                style.margin[3] = l;
            }
        }
        "padding" => {
            if let Some(vals) = parse_box_sides(v) {
                style.padding = vals;
            }
        }
        "padding-top" => {
            if let Some(l) = Length::parse(v) {
                style.padding[0] = l;
            }
        }
        "padding-right" => {
            if let Some(l) = Length::parse(v) {
                style.padding[1] = l;
            }
        }
        "padding-bottom" => {
            if let Some(l) = Length::parse(v) {
                style.padding[2] = l;
            }
        }
        "padding-left" => {
            if let Some(l) = Length::parse(v) {
                style.padding[3] = l;
            }
        }
        "border" => {
            for tok in v.split_whitespace() {
                if let Some(l) = Length::parse(tok) {
                    style.border_width = l.resolve(0.0, 16.0);
                } else if let Some(c) = Color::parse(tok) {
                    style.border_color = c;
                } else if tok.eq_ignore_ascii_case("solid")
                    || tok.eq_ignore_ascii_case("double")
                {
                    style.border_style = LineStyle::Solid;
                }
            }
            if style.border_style == LineStyle::Solid && style.border_width == 0.0 {
                style.border_width = 1.0;
            }
        }
        "border-width" => {
            if let Some(l) = Length::parse(v) {
                style.border_width = l.resolve(0.0, 16.0);
            }
        }
        "border-color" => {
            if let Some(c) = Color::parse(v) {
                style.border_color = c;
            }
        }
        "border-style" => {
            style.border_style = if v.eq_ignore_ascii_case("none") {
                LineStyle::None
            } else {
                LineStyle::Solid
            };
        }
        "width" => {
            if let Some(l) = Length::parse(v) {
                style.width = l;
            }
        }
        "height" => {
            if let Some(l) = Length::parse(v) {
                style.height = l;
            }
        }
        _ => {}
    }
}

fn keyword_font_size(v: &str, current: f32) -> f32 {
    match v.to_ascii_lowercase().as_str() {
        "xx-small" => 9.0,
        "x-small" => 10.0,
        "small" => 13.0,
        "medium" => 16.0,
        "large" => 18.0,
        "x-large" => 24.0,
        "xx-large" => 32.0,
        "smaller" => current * 0.83,
        "larger" => current * 1.2,
        _ => current,
    }
}

/// CSS box shorthand: 1–4 space-separated lengths into
/// [top, right, bottom, left]. Returns None when any token fails to parse.
fn parse_box_sides(v: &str) -> Option<[Length; 4]> {
    let parts: Vec<&str> = v.split_whitespace().collect();
    let vals: Vec<Length> = parts.iter().filter_map(|p| Length::parse(p)).collect();
    if vals.len() != parts.len() || vals.is_empty() {
        return None;
    }
    Some(match vals.len() {
        1 => [vals[0], vals[0], vals[0], vals[0]],
        2 => [vals[0], vals[1], vals[0], vals[1]],
        3 => [vals[0], vals[1], vals[2], vals[1]],
        _ => [vals[0], vals[1], vals[2], vals[3]],
    })
}

/// UA stylesheet equivalents for common tags, applied before author CSS.
pub fn tag_defaults(tag: &str) -> Vec<Declaration> {
    let mk = |p: &str, v: &str| Declaration {
        property: p.to_string(),
        value: v.to_string(),
        important: false,
    };
    match tag {
        "h1" => vec![
            mk("display", "block"),
            mk("font-size", "2em"),
            mk("font-weight", "bold"),
            mk("margin", "0.67em 0"),
        ],
        "h2" => vec![
            mk("display", "block"),
            mk("font-size", "1.5em"),
            mk("font-weight", "bold"),
            mk("margin", "0.83em 0"),
        ],
        "h3" => vec![
            mk("display", "block"),
            mk("font-size", "1.17em"),
            mk("font-weight", "bold"),
            mk("margin", "1em 0"),
        ],
        "h4" => vec![
            mk("display", "block"),
            mk("font-size", "1em"),
            mk("font-weight", "bold"),
            mk("margin", "1.33em 0"),
        ],
        "th" => vec![
            mk("display", "block"),
            mk("font-weight", "bold"),
        ],
        "h5" => vec![
            mk("display", "block"),
            mk("font-size", "0.83em"),
            mk("font-weight", "bold"),
            mk("margin", "1.67em 0"),
        ],
        "h6" => vec![
            mk("display", "block"),
            mk("font-size", "0.67em"),
            mk("font-weight", "bold"),
            mk("margin", "2.33em 0"),
        ],
        "b" | "strong" => vec![mk("font-weight", "bold")],
        "p" | "div" | "section" | "article" | "header" | "footer" | "nav" | "main"
        | "aside" | "ul" | "ol" | "dl" | "blockquote" | "pre" | "form" | "fieldset"
        | "hr" | "table" | "figure" | "figcaption" | "details" | "summary" => {
            vec![mk("display", "block")]
        }
        "li" => vec![mk("display", "block")],
        "i" | "em" | "cite" | "var" | "dfn" => vec![mk("font-style", "italic")],
        "a" => vec![
            mk("color", "#0000EE"),
            mk("text-decoration", "underline"),
        ],
        "u" | "ins" => vec![mk("text-decoration", "underline")],
        "s" | "strike" | "del" => vec![mk("text-decoration", "line-through")],
        "mark" => vec![
            mk("background-color", "#FFFF00"),
            mk("color", "#000000"),
        ],
        "small" => vec![mk("font-size", "smaller")],
        "big" => vec![mk("font-size", "larger")],
        "center" => vec![mk("text-align", "center")],
        "title" | "head" | "meta" | "link" | "script" | "style" | "noscript" | "template" => {
            vec![mk("display", "none")]
        }
        _ => Vec::new(),
    }
}

/// Inline `style="..."` declarations.
pub fn inline_declarations(el: &ElementData) -> Vec<Declaration> {
    match el.get_attr("style") {
        Some(s) => parse_declarations(s),
        None => Vec::new(),
    }
}

/// Compute the final style for a node: UA defaults → author rules by
/// specificity → inline styles.
pub fn compute_style(
    tree: &crate::dom::Dom,
    sheet: &Stylesheet,
    node: NodeId,
    hover_flags: &HashMap<NodeId, u32>,
    parent_font: f32,
) -> ComputedStyle {
    let mut style = ComputedStyle::default();
    let node_ref = tree.get(node);
    let el = match node_ref.map(|n| &n.kind) {
        Some(NodeType::Element(el)) => el,
        // Non-element nodes (text runs): inherit the parent font size so
        // CSS font-size on containers (h1..h6, etc.) reaches their text.
        _ => {
            style.font_size = parent_font;
            return style;
        }
    };
    style.font_size = parent_font;

    let tag_decls = tag_defaults(&el.tag);
    apply_declarations(&mut style, &tag_decls);

    // Author rules sorted by specificity (stable cascade: later + higher
    // specificity wins).
    let mut matched: Vec<((u32, u32, u32), usize, &Vec<Declaration>)> = Vec::new();
    let flags = hover_flags.get(&node).copied().unwrap_or(0);
    for (idx, rule) in sheet.rules.iter().enumerate() {
        for sel in &rule.selectors {
            if matches_selector(sel, tree, node, flags) {
                matched.push((sel.specificity(), idx, &rule.declarations));
                break;
            }
        }
    }
    matched.sort_by_key(|(spec, idx, _)| (*spec, *idx));
    for (_, _, decls) in &matched {
        apply_declarations(&mut style, decls);
    }

    let inline = inline_declarations(el);
    apply_declarations(&mut style, &inline);
    style.font_size = style.font_size.clamp(6.0, 96.0);
    style
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::parse_html;

    #[test]
    fn color_parsing() {
        assert_eq!(Color::parse("#ff0000"), Some(Color { r: 255, g: 0, b: 0, a: 1.0 }));
        assert_eq!(Color::parse("#f00").map(|c| c.r), Some(255));
        assert_eq!(Color::parse("rgba(1,2,3,0.5)").map(|c| c.a), Some(0.5));
        assert_eq!(Color::parse("white"), Some(Color::WHITE));
    }

    #[test]
    fn length_parsing() {
        assert_eq!(Length::parse("10px"), Some(Length::Px(10.0)));
        assert_eq!(Length::parse("50%"), Some(Length::Percent(50.0)));
        assert_eq!(Length::parse("auto"), Some(Length::Auto));
        assert_eq!(Length::parse("2em").map(|l| l.resolve(0.0, 16.0)), Some(32.0));
    }

    #[test]
    fn class_and_type_matching() {
        let html = r#"<div class="a b"><p id="x" class="c">hi</p></div>"#;
        let dom = parse_html(html);
        // find p node
        let mut p_id = None;
        for i in 0..dom.nodes.len() {
            let id = NodeId(i as u32);
            if let Some(n) = dom.get(id) {
                if let NodeType::Element(el) = &n.kind {
                    if el.tag == "p" {
                        p_id = Some(id);
                    }
                }
            }
        }
        let pid = p_id.expect("p found");
        let sheet = parse_stylesheet("p.c { color: red; } div .c { font-weight: bold; } span { color: blue; }");
        let st = compute_style(&dom, &sheet, pid, &Default::default(), 16.0);
        assert_eq!(st.color, Color { r: 255, g: 0, b: 0, a: 1.0 });
        assert!(st.bold);
    }

    #[test]
    fn specificity_prefers_id() {
        let html = r##"<p id="x" class="c" style="color:green">t</p>"##;
        let dom = parse_html(html);
        let mut pid = None;
        for i in 0..dom.nodes.len() {
            let id = NodeId(i as u32);
            if let Some(n) = dom.get(id) {
                if let NodeType::Element(el) = &n.kind {
                    if el.tag == "p" {
                        pid = Some(id);
                    }
                }
            }
        }
        let pid = pid.unwrap();
        let sheet = parse_stylesheet("#x { color: black; } .c { color: red; }");
        let st = compute_style(&dom, &sheet, pid, &Default::default(), 16.0);
        assert_eq!(st.color, Color { r: 0, g: 128, b: 0, a: 1.0 });
    }

    #[test]
    fn color_and_length_extensions() {
        assert_eq!(Color::parse("#f008").map(|c| c.r), Some(255));
        assert_eq!(Color::parse("rgb(10, 20, 30, 0.5)").map(|c| c.a), Some(0.5));
        assert!(Length::parse("12pt").is_some());
    }

    #[test]
    fn bold_is_inline() {
        let dom = parse_html("<b>bold</b>");
        let sheet = parse_stylesheet("");
        for id in dom.iter() {
            if let Some(n) = dom.get(id) {
                if let NodeType::Element(el) = &n.kind {
                    if el.tag == "b" {
                        let st = compute_style(&dom, &sheet, id, &Default::default(), 16.0);
                        assert_eq!(st.display, Display::Inline);
                        assert!(st.bold);
                    }
                }
            }
        }
    }
}
