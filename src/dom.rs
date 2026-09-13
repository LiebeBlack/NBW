//! dom.rs — HTML5 tokenizer + tree builder producing an arena-based DOM.
//!
//! Spec-driven subset: void elements, raw-text elements (`script`,
//! `style`), attributes (quoted, unquoted, valueless), comments, DOCTYPE,
//! CDATA tolerance, entities (named ~40 + numeric), implied ends (`<li>`,
//! `<p>`, `<tr>`, `<td>`...), and mismatched-tag recovery. The tree is a
//! flat `Vec<Node>` (arena) — no `Rc<RefCell>`, cache-friendly iteration.

#![allow(dead_code)]

pub const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link",
    "meta", "param", "source", "track", "wbr",
];

pub const RAWTEXT_TAGS: &[&str] = &["script", "style", "textarea", "title"];

const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"), ("lt", "<"), ("gt", ">"), ("quot", "\""),
    ("apos", "'"), ("nbsp", "\u{00A0}"), ("iexcl", "\u{00A1}"),
    ("cent", "\u{00A2}"), ("pound", "\u{00A3}"), ("curren", "\u{00A4}"),
    ("yen", "\u{00A5}"), ("brvbar", "\u{00A6}"), ("sect", "\u{00A7}"),
    ("uml", "\u{00A8}"), ("copy", "\u{00A9}"), ("ordf", "\u{00AA}"),
    ("laquo", "\u{00AB}"), ("not", "\u{00AC}"), ("shy", "\u{00AD}"),
    ("reg", "\u{00AE}"), ("macr", "\u{00AF}"), ("deg", "\u{00B0}"),
    ("plusmn", "\u{00B1}"), ("sup2", "\u{00B2}"), ("sup3", "\u{00B3}"),
    ("acute", "\u{00B4}"), ("micro", "\u{00B5}"), ("para", "\u{00B6}"),
    ("middot", "\u{00B7}"), ("cedil", "\u{00B8}"), ("sup1", "\u{00B9}"),
    ("ordm", "\u{00BA}"), ("raquo", "\u{00BB}"), ("frac14", "\u{00BC}"),
    ("frac12", "\u{00BD}"), ("frac34", "\u{00BE}"), ("iquest", "\u{00BF}"),
    ("Agrave", "\u{00C0}"), ("Aacute", "\u{00C1}"), ("Acirc", "\u{00C2}"),
    ("Atilde", "\u{00C3}"), ("Auml", "\u{00C4}"), ("Aring", "\u{00C5}"),
    ("AElig", "\u{00C6}"), ("Ccedil", "\u{00C7}"), ("Egrave", "\u{00C8}"),
    ("Eacute", "\u{00C9}"), ("Ecirc", "\u{00CA}"), ("Euml", "\u{00CB}"),
    ("Igrave", "\u{00CC}"), ("Iacute", "\u{00CD}"), ("Icirc", "\u{00CE}"),
    ("Iuml", "\u{00CF}"), ("ETH", "\u{00D0}"), ("Ntilde", "\u{00D1}"),
    ("Ograve", "\u{00D2}"), ("Oacute", "\u{00D3}"), ("Ocirc", "\u{00D4}"),
    ("Otilde", "\u{00D5}"), ("Ouml", "\u{00D6}"), ("times", "\u{00D7}"),
    ("Oslash", "\u{00D8}"), ("Ugrave", "\u{00D9}"), ("Uacute", "\u{00DA}"),
    ("Ucirc", "\u{00DB}"), ("Uuml", "\u{00DC}"), ("Yacute", "\u{00DD}"),
    ("THORN", "\u{00DE}"), ("szlig", "\u{00DF}"),
    ("agrave", "\u{00E0}"), ("aacute", "\u{00E1}"), ("acirc", "\u{00E2}"),
    ("atilde", "\u{00E3}"), ("auml", "\u{00E4}"), ("aring", "\u{00E5}"),
    ("aelig", "\u{00E6}"), ("ccedil", "\u{00E7}"), ("egrave", "\u{00E8}"),
    ("eacute", "\u{00E9}"), ("ecirc", "\u{00EA}"), ("euml", "\u{00EB}"),
    ("igrave", "\u{00EC}"), ("iacute", "\u{00ED}"), ("icirc", "\u{00EE}"),
    ("iuml", "\u{00EF}"), ("eth", "\u{00F0}"), ("ntilde", "\u{00F1}"),
    ("ograve", "\u{00F2}"), ("oacute", "\u{00F3}"), ("ocirc", "\u{00F4}"),
    ("otilde", "\u{00F5}"), ("ouml", "\u{00F6}"), ("divide", "\u{00F7}"),
    ("oslash", "\u{00F8}"), ("ugrave", "\u{00F9}"), ("uacute", "\u{00FA}"),
    ("ucirc", "\u{00FB}"), ("uuml", "\u{00FC}"), ("yacute", "\u{00FD}"),
    ("thorn", "\u{00FE}"), ("yuml", "\u{00FF}"),
    ("trade", "\u{2122}"), ("hellip", "\u{2026}"),
    ("mdash", "\u{2014}"), ("ndash", "\u{2013}"), ("lsquo", "\u{2018}"),
    ("rsquo", "\u{2019}"), ("sbquo", "\u{201A}"), ("ldquo", "\u{201C}"),
    ("rdquo", "\u{201D}"), ("bdquo", "\u{201E}"), ("dagger", "\u{2020}"),
    ("Dagger", "\u{2021}"), ("bull", "\u{2022}"), ("euro", "\u{20AC}"),
    ("permil", "\u{2030}"), ("lsaquo", "\u{2039}"), ("rsaquo", "\u{203A}"),
    ("alpha", "\u{03B1}"), ("beta", "\u{03B2}"), ("pi", "\u{03C0}"),
    ("Omega", "\u{03A9}"), ("infin", "\u{221E}"), ("ne", "\u{2260}"),
    ("le", "\u{2264}"), ("ge", "\u{2265}"),
    ("larr", "\u{2190}"), ("uarr", "\u{2191}"), ("rarr", "\u{2192}"),
    ("darr", "\u{2193}"), ("harr", "\u{2194}"), ("check", "\u{2713}"),
];

pub fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let bytes: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != '&' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // Find terminator.
        let mut j = i + 1;
        let mut name = String::new();
        let mut numeric = false;
        let mut hex = false;
        let mut matched: Option<String> = None;
        while j < bytes.len() && j - i <= 12 {
            let c = bytes[j];
            if c == ';' {
                if numeric {
                    let radix = if hex { 16 } else { 10 };
                    if let Ok(cp) = u32::from_str_radix(&name, radix) {
                        if let Some(ch) = char::from_u32(cp) {
                            matched = Some(ch.to_string());
                        }
                    }
                } else {
                    for (k, v) in NAMED_ENTITIES {
                        if *k == name {
                            matched = Some(v.to_string());
                            break;
                        }
                    }
                }
                j += 1;
                break;
            }
            if c == '#' && name.is_empty() {
                numeric = true;
                j += 1;
                continue;
            }
            if (c == 'x' || c == 'X') && name.is_empty() && numeric {
                // Hex entity: &#x1F600;
                hex = true;
                j += 1;
                continue;
            }
            name.push(c);
            j += 1;
            // Longest-prefix match for unterminated entities.
            if !numeric {
                for (k, v) in NAMED_ENTITIES {
                    if *k == name {
                        matched = Some(v.to_string());
                        break;
                    }
                }
                if matched.is_some() && j < bytes.len() && bytes[j] == ';' {
                    j += 1;
                    break;
                }
            }
        }
        match matched {
            Some(s) => {
                out.push_str(&s);
                i = j;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct ElementData {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub classes: Vec<String>,
}

impl ElementData {
    pub fn get_attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub enum NodeType {
    Document,
    Element(ElementData),
    Text(String),
    Comment(String),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeType,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

impl Dom {
    /// True when `id` is the first element child of its parent.
    pub fn is_first_element_child(&self, id: NodeId) -> bool {
        let Some(n) = self.get(id) else { return false };
        let Some(p) = n.parent else { return false };
        let Some(pn) = self.get(p) else { return false };
        pn.children.first() == Some(&id)
    }

    /// True when `id` is the last element child of its parent.
    pub fn is_last_element_child(&self, id: NodeId) -> bool {
        let Some(n) = self.get(id) else { return false };
        let Some(p) = n.parent else { return false };
        let Some(pn) = self.get(p) else { return false };
        pn.children.last() == Some(&id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone)]
pub struct Dom {
    pub nodes: Vec<Node>,
}

impl Default for Dom {
    fn default() -> Self {
        Self::new()
    }
}

impl Dom {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                kind: NodeType::Document,
                parent: None,
                children: Vec::new(),
            }],
        }
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0 as usize)
    }

    fn alloc(&mut self, kind: NodeType, parent: NodeId) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            kind,
            parent: Some(parent),
            children: Vec::new(),
        });
        self.nodes[parent.0 as usize].children.push(id);
        id
    }

    pub fn title(&self) -> String {
        for id in self.iter() {
            if let Some(node) = self.get(id) {
                if let NodeType::Element(el) = &node.kind {
                    if el.tag == "title" {
                        for &c in &node.children {
                            if let Some(child_node) = self.get(c) {
                                if let NodeType::Text(t) = &child_node.kind {
                                    return t.trim().to_string();
                                }
                            }
                        }
                    }
                }
            }
        }
        String::new()
    }

    /// Pre-order element/text iteration.
    pub fn iter(&self) -> DomIter<'_> {
        DomIter {
            dom: self,
            stack: vec![NodeId(0)],
        }
    }
}

pub struct DomIter<'a> {
    dom: &'a Dom,
    stack: Vec<NodeId>,
}

impl<'a> Iterator for DomIter<'a> {
    type Item = NodeId;
    fn next(&mut self) -> Option<NodeId> {
        while let Some(id) = self.stack.pop() {
            let node = self.dom.get(id)?;
            for &c in node.children.iter().rev() {
                self.stack.push(c);
            }
            return Some(id);
        }
        None
    }
}

/// Attribute parsing state.
struct AttrOut {
    name: String,
    value: String,
}

fn auto_close_tags(dom: &Dom, stack: &mut Vec<NodeId>, new_tag: &str) {
    match new_tag {
        "p" => {
            // A <p> tag closes an open <p>.
            if let Some(pos) = stack.iter().rposition(|id| {
                matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "p")
            }) {
                if pos > 0 {
                    stack.truncate(pos);
                }
            }
        }
        "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "table" | "hr"
        | "blockquote" | "pre" | "section" | "article" | "aside" | "header" | "footer"
        | "nav" | "main" | "form" | "fieldset" | "address" => {
            // Block-level elements close an open <p>.
            if let Some(pos) = stack.iter().rposition(|id| {
                matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "p")
            }) {
                if pos > 0 {
                    stack.truncate(pos);
                }
            }
        }
        "li" => {
            // An <li> closes an open <li> if in the same list.
            if let Some(pos) = stack.iter().rposition(|id| {
                matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "li")
            }) {
                let ul_pos = stack.iter().rposition(|id| {
                    matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "ul" || el.tag == "ol")
                });
                if ul_pos.map_or(true, |u| pos > u) && pos > 0 {
                    stack.truncate(pos);
                }
            }
        }
        "dt" | "dd" => {
            if let Some(pos) = stack.iter().rposition(|id| {
                matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "dt" || el.tag == "dd")
            }) {
                let dl_pos = stack.iter().rposition(|id| {
                    matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "dl")
                });
                if dl_pos.map_or(true, |u| pos > u) && pos > 0 {
                    stack.truncate(pos);
                }
            }
        }
        "tr" => {
            if let Some(pos) = stack.iter().rposition(|id| {
                matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "tr")
            }) {
                if pos > 0 {
                    stack.truncate(pos);
                }
            }
        }
        "td" | "th" => {
            if let Some(pos) = stack.iter().rposition(|id| {
                matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "td" || el.tag == "th")
            }) {
                let tr_pos = stack.iter().rposition(|id| {
                    matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "tr")
                });
                if tr_pos.map_or(true, |t| pos > t) && pos > 0 {
                    stack.truncate(pos);
                }
            }
        }
        "option" => {
            if let Some(pos) = stack.iter().rposition(|id| {
                matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == "option")
            }) {
                if pos > 0 {
                    stack.truncate(pos);
                }
            }
        }
        _ => {}
    }
}

/// Tokenizer + tree builder.
pub fn parse_html(input: &str) -> Dom {
    let mut dom = Dom::new();
    // Implicit <html> root: everything attaches inside it.
    let html = dom.alloc(
        NodeType::Element(ElementData {
            tag: "html".into(),
            attrs: Vec::new(),
            classes: Vec::new(),
        }),
        NodeId(0),
    );
    let mut stack: Vec<NodeId> = vec![html];
    let bytes: Vec<char> = input.chars().collect();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == '<' {
            // Comment?
            if starts_with(&bytes, i, "<!--") {
                i += 4;
                let start = i;
                while i + 2 < bytes.len() && !(bytes[i] == '-' && bytes[i + 1] == '-' && bytes[i + 2] == '>') {
                    i += 1;
                }
                let comment: String = bytes[start..i.min(bytes.len())].iter().collect();
                let parent = stack.last().copied().unwrap_or(NodeId(0));
                dom.alloc(NodeType::Comment(comment), parent);
                i = (i + 3).min(bytes.len());
                continue;
            }
            // DOCTYPE.
            if starts_with_ci(&bytes, i, "<!doctype") {
                while i < bytes.len() && bytes[i] != '>' {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                continue;
            }
            // CDATA — treat as text.
            if starts_with(&bytes, i, "<![CDATA[") {
                i += 9;
                let start = i;
                while i + 2 < bytes.len() && !(bytes[i] == ']' && bytes[i + 1] == ']' && bytes[i + 2] == '>') {
                    i += 1;
                }
                let text: String = bytes[start..i.min(bytes.len())].iter().collect();
                let parent = stack.last().copied().unwrap_or(NodeId(0));
                dom.alloc(NodeType::Text(decode_entities(&text)), parent);
                i = (i + 3).min(bytes.len());
                continue;
            }
            // Closing tag.
            if i + 1 < bytes.len() && bytes[i + 1] == '/' {
                let start = i + 2;
                let mut j = start;
                while j < bytes.len() && bytes[j] != '>' && bytes[j] != '<' {
                    j += 1;
                }
                let name: String = bytes[start..j.min(bytes.len())].iter().collect();
                let name = name.trim().to_ascii_lowercase();
                // Pop to matching open tag if present on the stack.
                if let Some(pos) = stack.iter().rposition(|id| {
                    matches!(dom.get(*id).map(|n| &n.kind), Some(NodeType::Element(el)) if el.tag == name)
                }) {
                    if pos > 0 {
                        stack.truncate(pos);
                    }
                }
                i = (j + 1).min(bytes.len());
                continue;
            }
            // Opening tag or standalone '<'.
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j] == '!' || bytes[j] == '?') {
                while j < bytes.len() && bytes[j] != '>' {
                    j += 1;
                }
                i = (j + 1).min(bytes.len());
                continue;
            }
            if j >= bytes.len() || !(bytes[j].is_ascii_alphabetic()) {
                // Literal '<' as text.
                let parent = stack.last().copied().unwrap_or(NodeId(0));
                dom.alloc(NodeType::Text("<".into()), parent);
                i += 1;
                continue;
            }
            // Tag name.
            let name_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == '-' || bytes[j] == '_' || bytes[j] == ':') {
                j += 1;
            }
            let tag: String = bytes[name_start..j].iter().collect::<String>().to_ascii_lowercase();
            // Attributes.
            let mut attrs: Vec<AttrOut> = Vec::new();
            let mut is_self_closing = false;
            loop {
                while j < bytes.len() && (bytes[j] == ' ' || bytes[j] == '\t' || bytes[j] == '\n' || bytes[j] == '\r') {
                    j += 1;
                }
                if j >= bytes.len() {
                    break;
                }
                if bytes[j] == '>' {
                    j += 1;
                    break;
                }
                if bytes[j] == '/' && j + 1 < bytes.len() && bytes[j + 1] == '>' {
                    is_self_closing = true;
                    j += 2;
                    break;
                }
                // Attribute name.
                let an_start = j;
                while j < bytes.len() && bytes[j] != '=' && bytes[j] != '>' && bytes[j] != ' ' && bytes[j] != '/' {
                    j += 1;
                }
                if j == an_start {
                    j += 1; // skip stray char
                    continue;
                }
                let an: String = bytes[an_start..j].iter().collect::<String>().to_ascii_lowercase();
                let mut av = String::new();
                // Skip whitespace.
                while j < bytes.len() && (bytes[j] == ' ' || bytes[j] == '\t' || bytes[j] == '\n' || bytes[j] == '\r') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == '=' {
                    j += 1;
                    while j < bytes.len() && (bytes[j] == ' ' || bytes[j] == '\t' || bytes[j] == '\n' || bytes[j] == '\r') {
                        j += 1;
                    }
                    if j < bytes.len() && (bytes[j] == '"' || bytes[j] == '\'') {
                        let q = bytes[j];
                        j += 1;
                        let v_start = j;
                        while j < bytes.len() && bytes[j] != q {
                            j += 1;
                        }
                        av = bytes[v_start..j.min(bytes.len())].iter().collect();
                        if j < bytes.len() {
                            j += 1;
                        }
                    } else {
                        let v_start = j;
                        while j < bytes.len() && bytes[j] != '>' && bytes[j] != ' ' {
                            j += 1;
                        }
                        av = bytes[v_start..j.min(bytes.len())].iter().collect();
                    }
                }
                attrs.push(AttrOut { name: an, value: decode_entities(&av) });
            }

            // Auto-close implied tags before allocating the new element.
            auto_close_tags(&dom, &mut stack, &tag);

            // Build element.
            let classes: Vec<String> = attrs
                .iter()
                .find(|a| a.name == "class")
                .map(|a| {
                    a.value
                        .split_whitespace()
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let elem = ElementData {
                tag: tag.clone(),
                attrs: attrs.into_iter().map(|a| (a.name, a.value)).collect(),
                classes,
            };
            let parent = stack.last().copied().unwrap_or(NodeId(0));
            let id = dom.alloc(NodeType::Element(elem), parent);

            if VOID_TAGS.contains(&tag.as_str()) || is_self_closing {
                // Void or self-closing elements never push to the stack, but the cursor
                // must still jump past the tag or the tokenizer spins on
                // `<` forever (every real page contains <img>/<br>/<meta>).
                i = j;
                continue;
            }
            stack.push(id);

            // Raw-text elements: swallow until matching close tag.
            if RAWTEXT_TAGS.contains(&tag.as_str()) {
                let close = format!("</{}", tag);
                let mut k = j;
                let mut found_end = bytes.len();
                while k < bytes.len() {
                    if starts_with_ci(&bytes, k, &close) {
                        // Find the '>' of the closing tag.
                        let mut e = k;
                        while e < bytes.len() && bytes[e] != '>' {
                            e += 1;
                        }
                        found_end = k; // text ends here
                        // Pop the rawtext element and continue after close.
                        // <title> is escapable raw text: entities decode; script/style do not.
                        let text: String = if tag == "title" {
                            decode_entities(&bytes[j..k].iter().collect::<String>())
                        } else {
                            bytes[j..k].iter().collect()
                        };
                        dom.nodes[id.0 as usize].children.clear();
                        dom.alloc(NodeType::Text(text), id);
                        // Remove tag from stack and skip close tag.
                        stack.pop();
                        i = (e + 1).min(bytes.len());
                        break;
                    }
                    k += 1;
                }
                if found_end == bytes.len() {
                    // Unterminated rawtext: everything is content.
                    let text: String = if tag == "title" {
                        decode_entities(&bytes[j..].iter().collect::<String>())
                    } else {
                        bytes[j..].iter().collect()
                    };
                    dom.nodes[id.0 as usize].children.clear();
                    dom.alloc(NodeType::Text(text), id);
                    stack.pop();
                    i = bytes.len();
                }
                continue;
            }
            i = j;
            continue;
        }
        // Text run.
        let start = i;
        while i < bytes.len() && bytes[i] != '<' {
            i += 1;
        }
        let raw: String = bytes[start..i].iter().collect();
        let text = decode_entities(&raw);
        let collapsed = collapse_ws(&text);
        if !collapsed.is_empty() {
            let parent = stack.last().copied().unwrap_or(NodeId(0));
            dom.alloc(NodeType::Text(collapsed), parent);
        }
    }
    dom
}

fn starts_with(bytes: &[char], pos: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    pos + p.len() <= bytes.len() && bytes[pos..pos + p.len()] == p[..]
}

fn starts_with_ci(bytes: &[char], pos: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    if pos + p.len() > bytes.len() {
        return false;
    }
    for (k, pc) in p.iter().enumerate() {
        if bytes[pos + k].to_ascii_lowercase() != pc.to_ascii_lowercase() {
            return false;
        }
    }
    true
}

/// Collapse consecutive whitespace into single spaces (HTML semantics).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_document() {
        let dom = parse_html("<!DOCTYPE html><html><head><title>T</title></head><body><h1>Hello</h1><p>World<br>!</p></body></html>");
        let mut h1_text = String::new();
        for id in dom.iter() {
            let n = dom.get(id).unwrap();
            if let NodeType::Element(el) = &n.kind {
                if el.tag == "h1" {
                    for &c in &n.children {
                        if let NodeType::Text(t) = &dom.get(c).unwrap().kind {
                            h1_text = t.clone();
                        }
                    }
                }
            }
        }
        assert_eq!(h1_text, "Hello");
        assert_eq!(dom.title(), "T");
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a&amp;b &lt;c&gt; &#65;"), "a&b <c> A");
        assert_eq!(decode_entities("&nbsp;x"), "\u{00A0}x");
    }

    #[test]
    fn hex_entities_decode() {
        assert_eq!(decode_entities("&#x41; &#X42;"), "A B");
    }

    #[test]
    fn void_and_rawtext() {
        let dom = parse_html("<div><img src=x><script>if(a<b){}</script></div>");
        let mut script_text = String::new();
        for id in dom.iter() {
            let n = dom.get(id).unwrap();
            if let NodeType::Element(el) = &n.kind {
                if el.tag == "script" {
                    for &c in &n.children {
                        if let NodeType::Text(t) = &dom.get(c).unwrap().kind {
                            script_text = t.clone();
                        }
                    }
                }
            }
        }
        assert_eq!(script_text, "if(a<b){}");
    }

    #[test]
    fn attrs_parse() {
        let dom = parse_html(r#"<a href="https://x.y" class="link big" disabled>go</a>"#);
        for id in dom.iter() {
            let n = dom.get(id).unwrap();
            if let NodeType::Element(el) = &n.kind {
                if el.tag == "a" {
                    assert_eq!(el.get_attr("href"), Some("https://x.y"));
                    assert_eq!(el.get_attr("disabled"), Some(""));
                    assert!(el.classes.contains(&"big".to_string()));
                    return;
                }
            }
        }
        panic!("a not found");
    }

    #[test]
    fn implied_tags_closing() {
        let dom = parse_html("<p>First<p>Second<ul><li>Item 1<li>Item 2</ul>");
        let mut p_count = 0;
        let mut li_count = 0;
        for id in dom.iter() {
            if let Some(n) = dom.get(id) {
                if let NodeType::Element(el) = &n.kind {
                    if el.tag == "p" {
                        p_count += 1;
                        // An open <p> should NOT contain another <p>
                        for &c in &n.children {
                            if let Some(child) = dom.get(c) {
                                if let NodeType::Element(child_el) = &child.kind {
                                    assert_ne!(child_el.tag, "p");
                                }
                            }
                        }
                    }
                    if el.tag == "li" {
                        li_count += 1;
                        // An open <li> should NOT contain another <li>
                        for &c in &n.children {
                            if let Some(child) = dom.get(c) {
                                if let NodeType::Element(child_el) = &child.kind {
                                    assert_ne!(child_el.tag, "li");
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(p_count, 2);
        assert_eq!(li_count, 2);
    }

    #[test]
    fn self_closing_non_void_tags() {
        let dom = parse_html(r#"<svg><path d="M0 0" /><span>after</span></svg>"#);
        for id in dom.iter() {
            if let Some(n) = dom.get(id) {
                if let NodeType::Element(el) = &n.kind {
                    if el.tag == "path" {
                        // <span> must not be child of self-closing <path>
                        assert!(n.children.is_empty());
                    }
                }
            }
        }
    }
}
