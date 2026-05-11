use abi_stable::{
    export_root_module,
    prefix_type::PrefixTypeTrait,
    std_types::{RResult, RStr, RVec},
};
use epub::doc::EpubDoc;
use kkc_plugin_api::{
    KKC_VIEWER_PLUGIN_API_VERSION, ViewerDocumentImage, ViewerHandleKeyResult, ViewerLine,
    ViewerPluginMetadata, ViewerPluginMod, ViewerPluginModRef, ViewerPluginResult, ViewerSpan,
};
use serde_json::{Map, Value};
use std::path::Path;
use unicode_width::UnicodeWidthChar;

// ── boilerplate ───────────────────────────────────────────────────────────────

#[export_root_module]
pub fn get_library() -> ViewerPluginModRef {
    ViewerPluginMod {
        api_version,
        metadata,
        render_document,
        render_document_image,
        handle_key,
    }
    .leak_into_prefix()
}

extern "C" fn api_version() -> u32 {
    KKC_VIEWER_PLUGIN_API_VERSION
}

extern "C" fn metadata() -> ViewerPluginMetadata {
    ViewerPluginMetadata {
        id: "epub".into(),
        name: "EPUB Viewer".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "EPUB ebook viewer".into(),
        modes: vec!["text".into()].into(),
        mime_types: vec!["application/epub+zip".into()].into(),
        extensions: vec!["epub".into()].into(),
    }
}

// ── render ────────────────────────────────────────────────────────────────────

extern "C" fn render_document(
    path: RStr<'_>,
    _mode: RStr<'_>,
    state_json: RStr<'_>,
    width: u64,
) -> ViewerPluginResult<RVec<ViewerLine>> {
    wrap(|| {
        let state = parse_state(state_json.as_str());
        let chapter = state_u(&state, "chapter", 0);
        let panel_w = if width >= 20 { width as usize } else { 80 };

        let mut doc =
            EpubDoc::new(Path::new(path.as_str())).map_err(|e| format!("Cannot open EPUB: {e}"))?;

        let num_chapters = doc.spine.len();
        if num_chapters == 0 {
            return Ok(error_lines("EPUB has no chapters"));
        }

        let chapter = chapter.min(num_chapters.saturating_sub(1));

        // Navigate to the requested chapter
        doc.set_current_chapter(chapter);

        // Extract title from metadata
        let book_title = doc.get_title().unwrap_or_else(|| "Unknown".to_string());
        let author = doc
            .mdata("creator")
            .map(|m| m.value.clone())
            .unwrap_or_default();

        // Get chapter content (HTML)
        let html = match doc.get_current_str() {
            Some((content, _mime)) => content,
            None => return Ok(error_lines("Cannot read chapter content")),
        };

        // Parse HTML with styling info
        let styled_lines = html_to_styled_lines(&html);

        // Apply word-wrap and render
        let rendered: Vec<ViewerLine> = styled_lines
            .iter()
            .flat_map(|line| render_styled_line(line, panel_w.saturating_sub(4)))
            .collect();

        // ── status bar ────────────────────────────────────────────────────────
        let mut status: Vec<ViewerSpan> = Vec::new();
        status.push(sp("EPUB", "yellow", true));
        status.push(sp(format!("  {book_title}"), "lightcyan", false));
        if !author.is_empty() {
            status.push(sp(format!("  \u{2014} {author}"), "darkgray", false));
        }
        status.push(sp(
            format!("  [{}/{}]", chapter + 1, num_chapters),
            "cyan",
            false,
        ));
        if num_chapters > 1 {
            status.push(sp("  [tab/[/]] chapter", "darkgray", false));
        }

        let mut out: Vec<ViewerLine> = Vec::new();
        out.push(ViewerLine {
            spans: status.into(),
        });
        out.push(blank_line());

        // Skip redundant first-line heading (content already has H1)
        let skip_first = rendered
            .first()
            .map(|l| {
                !l.spans.is_empty()
                    && (l.spans[0].fg == "lightcyan" || l.spans[0].fg == "cyan")
                    && l.spans[0].bold
            })
            .unwrap_or(false);

        out.extend(
            rendered
                .iter()
                .skip(if skip_first { 1 } else { 0 })
                .cloned(),
        );

        // Footer
        out.push(blank_line());
        if chapter + 1 < num_chapters {
            out.push(ViewerLine {
                spans: vec![
                    sp("  ── end of chapter ── ", "darkgray", false),
                    sp("tab", "cyan", false),
                    sp(" / ", "darkgray", false),
                    sp("]", "cyan", false),
                    sp(" → next chapter", "darkgray", false),
                ]
                .into(),
            });
        } else {
            out.push(ViewerLine {
                spans: vec![sp("  ── end of book ──", "darkgray", false)].into(),
            });
        }

        Ok(out.into())
    })
}

// ── render_document_image (stub for epub - not used) ────────────────────────

extern "C" fn render_document_image(
    _path: RStr<'_>,
    _mode: RStr<'_>,
    _state_json: RStr<'_>,
    _width: u64,
    _height: u64,
) -> ViewerPluginResult<ViewerDocumentImage> {
    // EPUB is text-based; return error
    wrap(|| Err("EPUB viewer does not support image rendering".into()))
}

// ── key handling ──────────────────────────────────────────────────────────────

extern "C" fn handle_key(
    path: RStr<'_>,
    _mode: RStr<'_>,
    key: RStr<'_>,
    state_json: RStr<'_>,
) -> ViewerPluginResult<ViewerHandleKeyResult> {
    wrap(|| {
        let mut state = parse_state(state_json.as_str());
        let chapter = state_u(&state, "chapter", 0) as i64;

        let num_chapters = EpubDoc::new(Path::new(path.as_str()))
            .map(|doc| doc.spine.len() as i64)
            .unwrap_or(1);

        let new_chapter = match key.as_str() {
            "tab" | "char:]" | "char:n" => (chapter + 1).min(num_chapters - 1),
            "backtab" | "char:[" | "char:p" => (chapter - 1).max(0),
            _ => chapter,
        };

        let consumed = new_chapter != chapter;
        state.insert("chapter".into(), Value::String(new_chapter.to_string()));

        Ok(ViewerHandleKeyResult {
            consumed,
            state_json: serde_json::to_string(&state)
                .unwrap_or_else(|_| "{}".into())
                .into(),
        })
    })
}

// ── Line type for styling ─────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
enum StyledLineType {
    Normal,
    Heading(u8),     // level 1-6
    Quote,           // blockquote
    Code,            // code block
    ListItem(usize), // list item with indent level
}

struct StyledLine {
    text: String,
    ty: StyledLineType,
}

// ── HTML → styled text ────────────────────────────────────────────────────────

/// Block-level HTML tags that should produce a newline break.
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "div"
            | "section"
            | "article"
            | "blockquote"
            | "li"
            | "dt"
            | "dd"
            | "tr"
            | "br"
            | "hr"
            | "pre"
            | "ul"
            | "ol"
            | "dl"
    )
}

/// Tags whose entire content should be skipped.
fn is_skip_tag(tag: &str) -> bool {
    matches!(tag, "head" | "style" | "script" | "svg" | "math")
}

/// Get heading level from tag name (returns 0 if not a heading).
fn heading_level(tag: &str) -> u8 {
    match tag {
        "h1" => 1,
        "h2" => 2,
        "h3" => 3,
        "h4" => 4,
        "h5" => 5,
        "h6" => 6,
        _ => 0,
    }
}

/// Strip HTML and return styled lines, preserving structure.
fn html_to_styled_lines(html: &str) -> Vec<StyledLine> {
    let mut lines: Vec<StyledLine> = Vec::new();
    let mut current_text = String::with_capacity(256);
    let mut current_type = StyledLineType::Normal;
    let mut current_list_indent = 0usize;

    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut in_entity = false;
    let mut entity_buf = String::new();
    let mut skip_stack: Vec<String> = Vec::new();

    fn flush_line(lines: &mut Vec<StyledLine>, text: &mut String, ty: &mut StyledLineType) {
        if !text.is_empty() {
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                lines.push(StyledLine {
                    text: trimmed,
                    ty: ty.clone(),
                });
            }
            text.clear();
            *ty = StyledLineType::Normal;
        }
    }

    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' if in_tag => {
                let raw = tag_buf.trim();
                let closing = raw.starts_with('/');
                let self_closing = raw.ends_with('/');
                let inner = raw.trim_start_matches('/').trim_end_matches('/').trim();
                let tag_name = inner
                    .split_ascii_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();

                if !skip_stack.is_empty() {
                    if closing {
                        if skip_stack.last().map(String::as_str) == Some(&tag_name) {
                            skip_stack.pop();
                        }
                    } else if !self_closing && is_skip_tag(&tag_name) {
                        skip_stack.push(tag_name);
                    }
                } else {
                    let level = heading_level(&tag_name);
                    if closing {
                        if level > 0 {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                        } else if tag_name == "blockquote" {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                        } else if tag_name == "pre" {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                        } else if tag_name == "li" {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                            if current_list_indent > 0 {
                                current_list_indent -= 1;
                            }
                        } else if tag_name == "ul" || tag_name == "ol" {
                            if current_list_indent > 0 {
                                current_list_indent -= 1;
                            }
                        } else if is_block_tag(&tag_name) {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                        }
                    } else {
                        if level > 0 {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                            current_type = StyledLineType::Heading(level);
                        } else if tag_name == "blockquote" {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                            current_type = StyledLineType::Quote;
                        } else if tag_name == "pre" {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                            current_type = StyledLineType::Code;
                        } else if tag_name == "li" {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                            // Indent list items
                            current_list_indent = current_list_indent.saturating_add(1).min(3);
                            current_type = StyledLineType::ListItem(current_list_indent);
                        } else if tag_name == "ul" || tag_name == "ol" {
                            current_list_indent = current_list_indent.saturating_add(1).min(3);
                        } else if is_skip_tag(&tag_name) {
                            skip_stack.push(tag_name);
                        } else if is_block_tag(&tag_name) {
                            flush_line(&mut lines, &mut current_text, &mut current_type);
                        }
                    }
                }

                in_tag = false;
                tag_buf.clear();
            }
            _ if in_tag => tag_buf.push(ch),
            '&' => {
                in_entity = true;
                entity_buf.clear();
            }
            ';' if in_entity => {
                if skip_stack.is_empty() {
                    current_text.push_str(&decode_entity(&entity_buf));
                }
                in_entity = false;
                entity_buf.clear();
            }
            _ if in_entity => {
                entity_buf.push(ch);
                if entity_buf.len() > 12 {
                    if skip_stack.is_empty() {
                        current_text.push('&');
                        current_text.push_str(&entity_buf);
                    }
                    in_entity = false;
                    entity_buf.clear();
                }
            }
            '\n' | '\r' | '\t' => {
                if skip_stack.is_empty() && !current_text.ends_with(['\n', ' ']) {
                    current_text.push(' ');
                }
            }
            _ => {
                if skip_stack.is_empty() {
                    current_text.push(ch);
                }
            }
        }
    }
    flush_line(&mut lines, &mut current_text, &mut current_type);
    lines
}

/// Render a styled line with appropriate colors, word-wrapping and indentation.
fn render_styled_line(styled: &StyledLine, max_w: usize) -> Vec<ViewerLine> {
    let wrapped = word_wrap(&styled.text, max_w);
    wrapped
        .into_iter()
        .map(|line| {
            if line.is_empty() {
                ViewerLine {
                    spans: vec![sp("", "white", false)].into(),
                }
            } else {
                match styled.ty {
                    StyledLineType::Heading(1) => {
                        // H1: big cyan bold, with upper spacing
                        vec![
                            ViewerLine {
                                spans: vec![sp("", "white", false)].into(),
                            },
                            ViewerLine {
                                spans: vec![sp(
                                    format!("  {}", line.to_uppercase()),
                                    "lightcyan",
                                    true,
                                )]
                                .into(),
                            },
                        ]
                        .into_iter()
                        .next()
                        .unwrap()
                    }
                    StyledLineType::Heading(2) => {
                        // H2: cyan bold with separator
                        ViewerLine {
                            spans: vec![sp("  ", "darkgray", false), sp(line, "cyan", true)].into(),
                        }
                    }
                    StyledLineType::Heading(n) => {
                        // H3+: yellow bold, indent by level
                        let indent = "  ".to_string() + &"  ".repeat((n - 1) as usize);
                        ViewerLine {
                            spans: vec![
                                sp(indent, "darkgray", false),
                                sp("▸ ", "lightyellow", false),
                                sp(line, "lightyellow", true),
                            ]
                            .into(),
                        }
                    }
                    StyledLineType::Quote => {
                        // Blockquote: yellow, left-indented with │
                        ViewerLine {
                            spans: vec![sp("  │ ", "yellow", false), sp(line, "yellow", false)]
                                .into(),
                        }
                    }
                    StyledLineType::Code => {
                        // Code block: gray mono-ish
                        ViewerLine {
                            spans: vec![sp("  ", "darkgray", false), sp(line, "gray", false)]
                                .into(),
                        }
                    }
                    StyledLineType::ListItem(indent) => {
                        // List item: indented with bullet
                        let spaces = "    ".repeat(indent);
                        ViewerLine {
                            spans: vec![
                                sp(spaces, "white", false),
                                sp("• ", "lightyellow", false),
                                sp(line, "white", false),
                            ]
                            .into(),
                        }
                    }
                    StyledLineType::Normal => {
                        // Normal paragraph: white, 2-space indent
                        ViewerLine {
                            spans: vec![sp(format!("  {}", line), "white", false)].into(),
                        }
                    }
                }
            }
        })
        .collect()
}

fn decode_entity(name: &str) -> &'static str {
    match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "ldquo" | "laquo" => "\u{201C}",
        "rdquo" | "raquo" => "\u{201D}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "eacute" => "é",
        "egrave" => "è",
        "ecirc" => "ê",
        "agrave" => "à",
        "acirc" => "â",
        "ugrave" => "ù",
        "ucirc" => "û",
        "ocirc" => "ô",
        "icirc" => "î",
        "ccedil" => "ç",
        _ => "",
    }
}

// ── word wrap ─────────────────────────────────────────────────────────────────

/// Wrap text paragraphs (split by `\n`) to at most `max_w` display columns.
fn word_wrap(text: &str, max_w: usize) -> Vec<String> {
    if max_w == 0 {
        return text.lines().map(str::to_owned).collect();
    }
    let mut lines: Vec<String> = Vec::new();
    for para in text.split('\n') {
        let para = para.trim();
        if para.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_w = 0usize;
        for word in para.split_whitespace() {
            let word_w = str_width(word);
            if current.is_empty() {
                current.push_str(word);
                current_w = word_w;
            } else if current_w + 1 + word_w <= max_w {
                current.push(' ');
                current.push_str(word);
                current_w += 1 + word_w;
            } else {
                lines.push(current.clone());
                current = word.to_string();
                current_w = word_w;
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

fn str_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

// ── utilities ──────────────────────────────────────────────────────────────────

fn wrap<T>(f: impl FnOnce() -> Result<T, String>) -> ViewerPluginResult<T> {
    match f() {
        Ok(v) => RResult::ROk(v),
        Err(e) => RResult::RErr(e.into()),
    }
}

fn parse_state(raw: &str) -> Map<String, Value> {
    if raw.trim().is_empty() {
        return Map::new();
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(obj)) => obj,
        _ => Map::new(),
    }
}

fn state_u(state: &Map<String, Value>, key: &str, default: usize) -> usize {
    match state.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(default as u64) as usize,
        Some(Value::String(s)) => s.parse().unwrap_or(default),
        _ => default,
    }
}

fn sp(text: impl Into<String>, fg: &str, bold: bool) -> ViewerSpan {
    ViewerSpan {
        text: text.into().into(),
        fg: fg.into(),
        bg: "".into(),
        bold,
    }
}

fn blank_line() -> ViewerLine {
    ViewerLine {
        spans: vec![sp("", "white", false)].into(),
    }
}

fn error_lines(msg: &str) -> RVec<ViewerLine> {
    vec![ViewerLine {
        spans: vec![sp("EPUB Viewer  ", "yellow", true), sp(msg, "red", false)].into(),
    }]
    .into()
}
