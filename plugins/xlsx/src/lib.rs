use abi_stable::{
    export_root_module,
    prefix_type::PrefixTypeTrait,
    std_types::{RResult, RStr, RVec},
};
use calamine::{Data, Reader, open_workbook_auto};
use kkc_plugin_api::{
    KKC_VIEWER_PLUGIN_API_VERSION, ViewerDocumentImage, ViewerHandleKeyResult, ViewerLine,
    ViewerPluginMetadata, ViewerPluginMod, ViewerPluginModRef, ViewerPluginResult, ViewerSpan,
};
use serde_json::{Map, Value};
use std::cmp;
use std::path::Path;
use unicode_width::UnicodeWidthChar;

const MAX_COLS: usize = 64;
const MAX_ROWS: usize = 5000;
const MAX_COL_WIDTH: usize = 40;
const MIN_COL_WIDTH: usize = 3;
/// Column separator: 5 display chars
const COL_SEP: &str = "  │  ";
const COL_SEP_W: usize = 5;

// ── helpers ──────────────────────────────────────────────────────────────────

fn sp(text: impl Into<String>, fg: &str, bold: bool) -> ViewerSpan {
    ViewerSpan {
        text: text.into().into(),
        fg: fg.into(),
        bg: "".into(),
        bold,
    }
}

/// Display width of a string.
///
/// `unicode-width` already gives 2 for full emoji codepoints (📧, 👥, 🏢, …).
/// For "text + VS16" sequences (☁️, ⚙️, …) the base char has width 1 and VS16
/// has width 0, so unicode-width returns 1 — but terminals render them as 2.
/// Fix: add +1 for each U+FE0F whose *preceding* char has width < 2 (i.e. the
/// VS16 is actually promoting a 1-wide glyph to emoji presentation).
/// If the preceding char is already 2-wide, VS16 is decorative and we skip it.
fn disp_w(s: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    let chars: Vec<char> = s.chars().collect();
    let mut width = 0usize;
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '\u{FE0F}' {
            // Only promote if the base char was 1-wide
            if i > 0 && UnicodeWidthChar::width(chars[i - 1]).unwrap_or(0) < 2 {
                width += 1;
            }
            // else: base already double-wide, VS16 has no extra visual cost
        } else {
            width += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    width
}

/// Truncate to at most `width` display columns, appending `…` if cut.
fn truncate(s: &str, width: usize) -> String {
    if disp_w(s) <= width {
        return s.to_string();
    }
    let target = width.saturating_sub(1); // room for '…'
    let mut out = String::new();
    let mut used = 0usize;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        // simulate disp_w for one char (including VS16 lookahead)
        let cw = if ch == '\u{FE0F}' {
            if i > 0 && UnicodeWidthChar::width(chars[i - 1]).unwrap_or(0) < 2 {
                1
            } else {
                0
            }
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if used + cw > target {
            break;
        }
        out.push(ch);
        used += cw;
        i += 1;
    }
    out.push('…');
    out
}
fn pad_right(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let w = disp_w(&t);
    if w >= width {
        t
    } else {
        format!("{}{}", t, " ".repeat(width - w))
    }
}

fn pad_left(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let w = disp_w(&t);
    if w >= width {
        t
    } else {
        format!("{}{}", " ".repeat(width - w), t)
    }
}

fn is_numeric_cell(cell: &Data) -> bool {
    matches!(cell, Data::Float(_) | Data::Int(_))
}

/// Shrink column widths so total rendered width ≤ max_w.
/// Total = sum(widths) + (n-1)*COL_SEP_W
fn fit_widths(widths: &[usize], max_w: usize) -> Vec<usize> {
    let n = widths.len();
    if n == 0 {
        return vec![];
    }
    let total =
        |r: &[usize]| -> usize { r.iter().sum::<usize>() + n.saturating_sub(1) * COL_SEP_W };
    let mut r = widths.to_vec();
    while total(&r) > max_w {
        let (idx, &max_val) = r.iter().enumerate().max_by_key(|&(_, &v)| v).unwrap();
        if max_val <= MIN_COL_WIDTH {
            break;
        }
        let second = r
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != idx)
            .map(|(_, &v)| v)
            .max()
            .unwrap_or(0);
        let step = (max_val - second).max(1);
        let excess = total(&r).saturating_sub(max_w);
        r[idx] = (max_val - step.min(excess)).max(MIN_COL_WIDTH);
    }
    r
}

fn line_no_col_width(total_rows: usize) -> usize {
    // "NNN│ " → digits + 2
    disp_w(&total_rows.to_string()) + 2
}

fn make_lineno_span(row_no: Option<usize>, col_w: usize) -> ViewerSpan {
    let text = if let Some(n) = row_no {
        format!("{n:>width$}│ ", width = col_w - 2)
    } else {
        format!("{:>width$}│ ", "", width = col_w - 2)
    };
    sp(text, "darkgray", false)
}

// ── plugin boilerplate ────────────────────────────────────────────────────────

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
        id: "xlsx".into(),
        name: "XLSX Viewer".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "XLSX / XLS / ODS spreadsheet viewer".into(),
        modes: vec!["text".into()].into(),
        mime_types: vec![
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".into(),
            "application/vnd.ms-excel".into(),
            "application/vnd.oasis.opendocument.spreadsheet".into(),
        ]
        .into(),
        extensions: vec!["xlsx".into(), "xls".into(), "xlsm".into(), "ods".into()].into(),
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
        let sheet_index = state_u(&state, "sheet", 0);
        let col_offset = state_u(&state, "col_offset", 0);

        let panel_w = if width >= 20 { width as usize } else { 120 };

        let file_path = Path::new(path.as_str());
        let mut workbook = open_workbook_auto(file_path).map_err(|e| e.to_string())?;
        let sheet_names = workbook.sheet_names().to_vec();

        if sheet_names.is_empty() {
            return Ok(simple_lines(vec![
                sp("XLSX Viewer", "yellow", true),
                sp("  Workbook has no sheets.", "red", false),
            ]));
        }

        let n_sheets = sheet_names.len();
        let selected = cmp::min(sheet_index, n_sheets.saturating_sub(1));
        let sheet_name = sheet_names[selected].clone();

        let range = workbook
            .worksheet_range(&sheet_name)
            .map_err(|e| e.to_string())?;

        let total_rows = range.height();
        let total_cols = range.width();

        // Collect cell strings and detect numeric columns
        let vis_cols = cmp::min(total_cols.saturating_sub(col_offset), MAX_COLS);
        let mut col_is_numeric = vec![true; vis_cols];
        let mut widths = vec![MIN_COL_WIDTH; vis_cols];

        let mut cell_rows: Vec<Vec<String>> = Vec::with_capacity(cmp::min(total_rows, MAX_ROWS));

        for (row_i, row) in range.rows().enumerate().take(MAX_ROWS + 1) {
            if row_i == 0 {
                // compute widths + numeric from all rows including header
            }
            let mut row_cells = Vec::with_capacity(vis_cols);
            for ci in 0..vis_cols {
                let abs_col = col_offset + ci;
                let cell = row.get(abs_col);
                let text = cell.map(cell_to_string).unwrap_or_default();
                let text = text.replace('\n', " ");
                let clen = disp_w(&text);
                widths[ci] = cmp::min(cmp::max(widths[ci], clen), MAX_COL_WIDTH);
                // numeric: any non-empty non-numeric value disqualifies (skip header row 0)
                if row_i > 0 {
                    if let Some(c) = cell {
                        if *c != Data::Empty && !is_numeric_cell(c) {
                            col_is_numeric[ci] = false;
                        }
                    }
                }
                row_cells.push(text);
            }
            cell_rows.push(row_cells);
        }

        let lineno_w = line_no_col_width(total_rows);
        let table_w = panel_w.saturating_sub(lineno_w);
        let fitted = fit_widths(&widths, table_w);

        // ── status bar ───────────────────────────────────────────────────────
        let mut status: Vec<ViewerSpan> = Vec::new();
        status.push(sp("XLSX", "yellow", true));
        status.push(sp("  sheet: ", "darkgray", false));
        // Sheet tabs
        for (i, name) in sheet_names.iter().enumerate() {
            if i == selected {
                status.push(sp(format!("[{}]", name), "lightcyan", true));
            } else {
                let short = truncate(name, 12);
                status.push(sp(format!(" {} ", short), "darkgray", false));
            }
        }
        status.push(sp("  rows: ", "darkgray", false));
        status.push(sp(total_rows.to_string(), "cyan", false));
        status.push(sp("  cols: ", "darkgray", false));
        status.push(sp(total_cols.to_string(), "cyan", false));
        if col_offset > 0 || total_cols > vis_cols + col_offset {
            status.push(sp(
                format!(
                    "  col {}-{}/{}",
                    col_offset + 1,
                    col_offset + vis_cols,
                    total_cols
                ),
                "lightyellow",
                false,
            ));
        }
        if n_sheets > 1 {
            status.push(sp("  [tab/[/]] sheet", "darkgray", false));
        }
        if total_cols > vis_cols {
            status.push(sp("  [</> ←/→] cols", "darkgray", false));
        }

        let mut out: Vec<ViewerLine> = Vec::new();
        out.push(ViewerLine {
            spans: status.into(),
        });
        out.push(ViewerLine {
            spans: vec![sp("", "white", false)].into(),
        });

        // ── rows ─────────────────────────────────────────────────────────────
        for (row_i, row_cells) in cell_rows.iter().enumerate() {
            let is_header = row_i == 0;

            let mut spans: Vec<ViewerSpan> = Vec::new();
            // line number
            spans.push(make_lineno_span(
                if is_header { None } else { Some(row_i) },
                lineno_w,
            ));

            for ci in 0..vis_cols {
                if ci > 0 {
                    spans.push(sp(COL_SEP, "darkgray", false));
                }
                let w = fitted[ci];
                let text = row_cells.get(ci).map(String::as_str).unwrap_or("");
                let padded = if col_is_numeric[ci] && !is_header {
                    pad_left(text, w)
                } else {
                    pad_right(text, w)
                };
                let fg = if is_header {
                    "lightcyan"
                } else if col_is_numeric[ci] {
                    "lightgreen"
                } else {
                    "white"
                };
                spans.push(sp(padded, fg, is_header));
            }

            out.push(ViewerLine {
                spans: spans.into(),
            });

            // separator line after header
            if is_header {
                let mut sep_spans: Vec<ViewerSpan> = Vec::new();
                sep_spans.push(sp(" ".repeat(lineno_w - 2) + "┼─", "darkgray", false));
                for ci in 0..vis_cols {
                    if ci > 0 {
                        sep_spans.push(sp("──┼──", "darkgray", false));
                    }
                    sep_spans.push(sp("─".repeat(fitted[ci]), "darkgray", false));
                }
                out.push(ViewerLine {
                    spans: sep_spans.into(),
                });
            }
        }

        if total_rows > MAX_ROWS {
            out.push(ViewerLine {
                spans: vec![sp(
                    format!("  … showing first {} of {} rows", MAX_ROWS, total_rows),
                    "darkgray",
                    false,
                )]
                .into(),
            });
        }

        Ok(out.into())
    })
}

// ── image rendering ───────────────────────────────────────────────────────────

extern "C" fn render_document_image(
    _path: RStr<'_>,
    _mode: RStr<'_>,
    _state_json: RStr<'_>,
    _width: u64,
    _height: u64,
) -> ViewerPluginResult<ViewerDocumentImage> {
    wrap(|| Err("XLSX viewer does not support image rendering".into()))
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
        let mut sheet = state_u(&state, "sheet", 0) as i64;
        let mut col_offset = state_u(&state, "col_offset", 0) as i64;
        let key = key.as_str();

        // Read sheet count and col count from workbook
        let (n_sheets, n_cols) = {
            let file_path = Path::new(path.as_str());
            if let Ok(mut wb) = open_workbook_auto(file_path) {
                let names = wb.sheet_names().to_vec();
                let nc = if !names.is_empty() {
                    let sel = cmp::min(sheet as usize, names.len().saturating_sub(1));
                    wb.worksheet_range(&names[sel])
                        .ok()
                        .map(|r| r.width())
                        .unwrap_or(0)
                } else {
                    0
                };
                (names.len() as i64, nc as i64)
            } else {
                (1, 0)
            }
        };

        let consumed = match key {
            "tab" | "char:]]" | "char:]" => {
                sheet = (sheet + 1).min(n_sheets - 1);
                true
            }
            "backtab" | "char:[[" | "char:[" => {
                sheet = (sheet - 1).max(0);
                true
            }
            "right" | "char:>" => {
                let max_off = (n_cols - 1).max(0);
                col_offset = (col_offset + 1).min(max_off);
                true
            }
            "left" | "char:<" => {
                col_offset = (col_offset - 1).max(0);
                true
            }
            "home" => {
                col_offset = 0;
                true
            }
            _ => false,
        };

        state.insert("sheet".into(), Value::String(sheet.max(0).to_string()));
        state.insert(
            "col_offset".into(),
            Value::String(col_offset.max(0).to_string()),
        );

        Ok(ViewerHandleKeyResult {
            consumed,
            state_json: serde_json::to_string(&state)
                .unwrap_or_else(|_| "{}".to_string())
                .into(),
        })
    })
}

// ── utilities ─────────────────────────────────────────────────────────────────

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
        Some(Value::String(s)) => s.parse::<usize>().unwrap_or(default),
        _ => default,
    }
}

fn simple_lines(spans: Vec<ViewerSpan>) -> RVec<ViewerLine> {
    vec![ViewerLine {
        spans: spans.into(),
    }]
    .into()
}

fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.to_string(),
        Data::Float(v) => {
            if v.fract() == 0.0 {
                format!("{}", *v as i64)
            } else {
                format!("{v}")
            }
        }
        Data::Int(v) => v.to_string(),
        Data::Bool(v) => if *v { "true" } else { "false" }.to_string(),
        Data::DateTime(v) => v.to_string(),
        Data::DateTimeIso(v) => v.to_string(),
        Data::DurationIso(v) => v.to_string(),
        Data::Error(v) => format!("#ERR:{v:?}"),
    }
}
