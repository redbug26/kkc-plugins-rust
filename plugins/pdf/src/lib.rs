//! PDF viewer plugin for kkc-rust
//!
//! Uses MuPDF for text extraction, with:
//! - LRU cache (per page text) to avoid re-parsing on every keystroke
//! - Rayon-based parallel prefetch for adjacent pages
//! - Flume channels for communicating prefetch requests to a background worker
//! - SIMD (wide) for fast ASCII line-width computation during word-wrap

use abi_stable::{
    export_root_module,
    prefix_type::PrefixTypeTrait,
    std_types::{RResult, RStr, RVec},
};
use base64::Engine as _;
use kkc_plugin_api::{
    KKC_VIEWER_PLUGIN_API_VERSION, ViewerDocumentImage, ViewerHandleKeyResult, ViewerImage,
    ViewerLine, ViewerPluginMetadata, ViewerPluginMod, ViewerPluginModRef, ViewerPluginResult,
    ViewerSpan,
};
use lru::LruCache;
use mupdf::text_page::TextBlockType;
use mupdf::{Colorspace, Document, Matrix, TextPageFlags};
use once_cell::sync::Lazy;
use rayon::prelude::*;
use serde_json::{Map, Value};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Mutex;
use unicode_width::UnicodeWidthChar;
use wide::u8x16;

// ── Global page-text cache ────────────────────────────────────────────────────
//
// Key:   (file path, 0-based page index)
// Value: extracted and word-wrapped lines for that page at their natural width
//        (wrapping at runtime happens in render_document with the actual panel width)

const CACHE_CAPACITY: usize = 30;

/// Raw extracted text per page (before word-wrap), keyed by (path, page).
static PAGE_TEXT_CACHE: Lazy<Mutex<LruCache<(String, usize), String>>> =
    Lazy::new(|| Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap())));

// ── Prefetch channel ──────────────────────────────────────────────────────────
//
// render_document sends (path, page) pairs to a flume channel; a background
// Rayon thread drains the channel and populates the cache.

type PrefetchMsg = (String, usize, usize); // (path, page, total_pages)

static PREFETCH_TX: Lazy<flume::Sender<PrefetchMsg>> = Lazy::new(|| {
    let (tx, rx) = flume::unbounded::<PrefetchMsg>();
    // Spawn a rayon task that continuously drains the prefetch channel
    rayon::spawn(move || {
        while let Ok((path, page, total)) = rx.recv() {
            // Prefetch a small window around the requested page
            let lo = page.saturating_sub(2);
            let hi = (page + 3).min(total);
            let pages_to_fetch: Vec<usize> = (lo..hi)
                .filter(|&p| {
                    PAGE_TEXT_CACHE
                        .lock()
                        .map(|c| !c.contains(&(path.clone(), p)))
                        .unwrap_or(false)
                })
                .collect();

            pages_to_fetch.into_par_iter().for_each(|p| {
                if let Ok(text) = extract_page_text_raw(&path, p) {
                    if let Ok(mut cache) = PAGE_TEXT_CACHE.lock() {
                        cache.put((path.clone(), p), text);
                    }
                }
            });
        }
    });
    tx
});

// ── Text extraction ───────────────────────────────────────────────────────────

/// Extract the raw text (unsorted lines joined by `\n`) from a single PDF page.
/// Each block is separated by a blank line to preserve paragraph structure.
fn extract_page_text_raw(path: &str, page_idx: usize) -> Result<String, String> {
    let doc = Document::open(path).map_err(|e| format!("mupdf open: {e}"))?;
    let page = doc
        .load_page(page_idx as i32)
        .map_err(|e| format!("mupdf load page {page_idx}: {e}"))?;
    let text_page = page
        .to_text_page(TextPageFlags::empty())
        .map_err(|e| format!("mupdf text page: {e}"))?;

    let mut out = String::new();
    let mut prev_y = f32::NEG_INFINITY;
    const LINE_GAP_THRESHOLD: f32 = 2.0;

    for block in text_page.blocks() {
        if block.r#type() != TextBlockType::Text {
            continue;
        }
        for line in block.lines() {
            let chars: Vec<_> = line.chars().collect();
            if chars.is_empty() {
                continue;
            }

            // Detect large vertical gap → blank separator between paragraphs
            let y = chars[0].origin().y;
            if prev_y != f32::NEG_INFINITY && (y - prev_y).abs() > LINE_GAP_THRESHOLD * 12.0 {
                out.push('\n');
            }
            prev_y = y;

            let mut line_text = String::new();
            for ch in &chars {
                if let Some(c) = ch.char() {
                    if c != '\n' {
                        line_text.push(c);
                    }
                }
            }
            let trimmed = line_text.trim();
            if !trimmed.is_empty() {
                out.push_str(trimmed);
                out.push('\n');
            }
        }
        // Blank line between blocks
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Get the number of pages in the document (returns 0 on error).
fn page_count(path: &str) -> usize {
    Document::open(path)
        .ok()
        .and_then(|doc| doc.page_count().ok())
        .map(|n| n.max(0) as usize)
        .unwrap_or(0)
}

// ── Word-wrap with SIMD fast-path ─────────────────────────────────────────────

/// Display width of `s`.
/// Fast-path: if all bytes are ASCII (< 128), each char has width 1.
fn str_display_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    // SIMD: check 16 bytes at a time for pure ASCII
    let chunks = bytes.len() / 16;
    let mut all_ascii = true;
    for i in 0..chunks {
        let chunk = u8x16::new(bytes[i * 16..i * 16 + 16].try_into().unwrap());
        // Any byte ≥ 128 → not pure ASCII
        let high_bit = chunk & u8x16::splat(0x80);
        if high_bit.move_mask() != 0 {
            all_ascii = false;
            break;
        }
    }
    if all_ascii {
        for &b in &bytes[chunks * 16..] {
            if b >= 0x80 {
                all_ascii = false;
                break;
            }
        }
    }
    if all_ascii {
        return bytes.len();
    }
    // Fallback: proper Unicode width
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Word-wrap raw text (paragraphs split by `\n`) to at most `max_w` columns.
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
            let ww = str_display_width(word);
            if current.is_empty() {
                current.push_str(word);
                current_w = ww;
            } else if current_w + 1 + ww <= max_w {
                current.push(' ');
                current.push_str(word);
                current_w += 1 + ww;
            } else {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
                current_w = ww;
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

// ── Plugin boilerplate ────────────────────────────────────────────────────────

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
        id: "pdf".into(),
        name: "PDF Viewer".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "PDF viewer with MuPDF text extraction".into(),
        modes: vec!["text".into(), "image".into()].into(),
        mime_types: vec!["application/pdf".into()].into(),
        extensions: vec!["pdf".into()].into(),
    }
}

// ── render_document ───────────────────────────────────────────────────────────

extern "C" fn render_document(
    path: RStr<'_>,
    _mode: RStr<'_>,
    state_json: RStr<'_>,
    width: u64,
) -> ViewerPluginResult<RVec<ViewerLine>> {
    wrap(|| {
        let state = parse_state(state_json.as_str());
        let page = state_u(&state, "page", 0);
        let panel_w = if width >= 20 { width as usize } else { 80 };
        let path_str = path.as_str();

        let total = page_count(path_str);
        if total == 0 {
            return Ok(error_lines("Cannot open PDF or document is empty"));
        }
        let page = page.min(total.saturating_sub(1));

        // Trigger background prefetch for adjacent pages
        let _ = PREFETCH_TX.send((path_str.to_owned(), page, total));

        // Get (or extract) text for current page
        let raw_text = {
            let cached = PAGE_TEXT_CACHE
                .lock()
                .ok()
                .and_then(|mut c| c.get(&(path_str.to_owned(), page)).cloned());
            match cached {
                Some(t) => t,
                None => {
                    let t = extract_page_text_raw(path_str, page)?;
                    if let Ok(mut cache) = PAGE_TEXT_CACHE.lock() {
                        cache.put((path_str.to_owned(), page), t.clone());
                    }
                    t
                }
            }
        };

        let wrapped = word_wrap(&raw_text, panel_w.saturating_sub(4));

        // ── status bar ────────────────────────────────────────────────────────
        let filename = Path::new(path_str)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path_str);
        let mut status: Vec<ViewerSpan> = Vec::new();
        status.push(sp("PDF", "yellow", true));
        status.push(sp(format!("  {filename}"), "lightcyan", false));
        status.push(sp(format!("  page {}/{}", page + 1, total), "cyan", false));
        status.push(sp("  [tab/[/]] page", "darkgray", false));

        let mut out: Vec<ViewerLine> = Vec::new();
        out.push(ViewerLine {
            spans: status.into(),
        });
        out.push(blank_line());

        // Body lines with subtle line-number column
        let lineno_w = format!("{total}").len() + 2;
        for (i, line) in wrapped.iter().enumerate() {
            let spans: Vec<ViewerSpan> = if line.is_empty() {
                vec![sp("", "white", false)]
            } else {
                vec![
                    sp(
                        format!("{:>w$}│ ", i + 1, w = lineno_w - 2),
                        "darkgray",
                        false,
                    ),
                    sp(line.as_str(), "white", false),
                ]
            };
            out.push(ViewerLine {
                spans: spans.into(),
            });
        }

        if wrapped.is_empty() {
            out.push(ViewerLine {
                spans: vec![sp("  (no text on this page)", "darkgray", true)].into(),
            });
        }

        // Footer
        out.push(blank_line());
        let mut footer: Vec<ViewerSpan> = Vec::new();
        if page + 1 < total {
            footer.push(sp("  ── end of page ── ", "darkgray", false));
            footer.push(sp("tab", "cyan", false));
            footer.push(sp(" / ", "darkgray", false));
            footer.push(sp("]", "cyan", false));
            footer.push(sp(" → next page", "darkgray", false));
        } else {
            footer.push(sp("  ── end of document ──", "darkgray", false));
        }
        out.push(ViewerLine {
            spans: footer.into(),
        });

        Ok(out.into())
    })
}

// ── render_document_image ─────────────────────────────────────────────────────

extern "C" fn render_document_image(
    path: RStr<'_>,
    _mode: RStr<'_>,
    state_json: RStr<'_>,
    width: u64,
    height: u64,
) -> ViewerPluginResult<ViewerDocumentImage> {
    wrap(|| {
        let state = parse_state(state_json.as_str());
        let page = state_u(&state, "page", 0);
        let path_str = path.as_str();

        let total = page_count(path_str);
        if total == 0 {
            return Err("Cannot open PDF or document is empty".into());
        }
        let page = page.min(total.saturating_sub(1));

        // Render page to pixmap (PNG-like RGB)
        let doc = Document::open(path_str).map_err(|e| format!("mupdf open: {e}"))?;
        let mupdf_page = doc
            .load_page(page as i32)
            .map_err(|e| format!("mupdf load page {page}: {e}"))?;

        // Get page bounds
        let bounds = mupdf_page
            .bounds()
            .map_err(|e| format!("mupdf bounds: {e}"))?;
        let page_w = bounds.x1 - bounds.x0;
        let page_h = bounds.y1 - bounds.y0;

        // Calculate zoom to fit terminal size (rough estimate: 8px per char width)
        let char_width_px = 8.0_f32;
        let char_height_px = 16.0_f32;
        let viewport_w = width as f32 * char_width_px;
        let viewport_h = height as f32 * char_height_px;

        let scale_x = viewport_w / page_w;
        let scale_y = viewport_h / page_h;
        let scale = scale_x.min(scale_y).max(0.5).min(3.0); // Clamp 0.5x to 3x zoom

        // Render to pixmap using page.to_pixmap()
        let display_w = (page_w * scale).ceil() as u32;
        let display_h = (page_h * scale).ceil() as u32;

        let rgb = Colorspace::device_rgb();
        let matrix = Matrix::new_scale(scale, scale);
        let pixmap = mupdf_page
            .to_pixmap(&matrix, &rgb, false, false)
            .map_err(|e| format!("mupdf render pixmap: {e}"))?;

        // Extract RGB pixel data (convert from pixmap to RGB triplets)
        let n = pixmap.n() as usize;
        if n < 3 {
            return Err(format!("Unsupported pixmap format: {n} channels"));
        }
        let pix_width = pixmap.width() as usize;
        let pix_height = pixmap.height() as usize;
        let stride = pixmap.stride() as usize;
        let samples = pixmap.samples();

        let mut rgb_data = Vec::with_capacity(pix_width * pix_height * 3);
        for y in 0..pix_height {
            let row_start = y * stride;
            let row_end = row_start + pix_width * n;
            if row_end > samples.len() {
                break;
            }
            let row = &samples[row_start..row_end];
            if n == 3 {
                rgb_data.extend_from_slice(row);
            } else {
                for px in row.chunks_exact(n) {
                    rgb_data.extend_from_slice(&px[..3]);
                }
            }
        }

        // Encode as base64
        let encoded = base64::engine::general_purpose::STANDARD.encode(&rgb_data);

        // Create status lines
        let filename = Path::new(path_str)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path_str);
        let mut status: Vec<ViewerSpan> = Vec::new();
        status.push(sp("PDF", "yellow", true));
        status.push(sp(format!("  {filename}"), "lightcyan", false));
        status.push(sp(format!("  page {}/{}", page + 1, total), "cyan", false));
        status.push(sp("  [tab/[/]] page", "darkgray", false));

        let overlay_lines = vec![ViewerLine {
            spans: status.into(),
        }];

        Ok(ViewerDocumentImage {
            image: ViewerImage {
                data: encoded.into(),
                format: "rgb".into(),
                width: display_w,
                height: display_h,
            },
            overlay_lines: overlay_lines.into(),
        })
    })
}

// ── handle_key ────────────────────────────────────────────────────────────────

extern "C" fn handle_key(
    path: RStr<'_>,
    _mode: RStr<'_>,
    key: RStr<'_>,
    state_json: RStr<'_>,
) -> ViewerPluginResult<ViewerHandleKeyResult> {
    wrap(|| {
        let mut state = parse_state(state_json.as_str());
        let page = state_u(&state, "page", 0) as i64;
        let total = page_count(path.as_str()) as i64;
        let max_page = (total - 1).max(0);

        let new_page = match key.as_str() {
            "tab" | "char:]" | "char:n" | "pagedown" => (page + 1).min(max_page),
            "backtab" | "char:[" | "char:p" | "pageup" => (page - 1).max(0),
            "home" | "char:g" => 0,
            "end" | "char:G" => max_page,
            _ => page,
        };

        let consumed = new_page != page;
        state.insert("page".into(), Value::String(new_page.to_string()));

        Ok(ViewerHandleKeyResult {
            consumed,
            state_json: serde_json::to_string(&state)
                .unwrap_or_else(|_| "{}".into())
                .into(),
        })
    })
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
        spans: vec![sp("PDF Viewer  ", "yellow", true), sp(msg, "red", false)].into(),
    }]
    .into()
}
