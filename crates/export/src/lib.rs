//! pdfcraft-export — Export a PDF ▸ Word, HTML, RTF (L4).
//!
//! The engine reduces each page to [`Page`]: paragraphs (text, box, size, bold/italic) and
//! images (encoded bytes and box), in reading order. The writers turn that into a flowing
//! document: headings are the paragraphs set larger than the body text, and images sit where
//! they fall between paragraphs.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod family;
mod grid;
mod zip;

pub use family::font_family;
pub use zip::Zip;

/// A paragraph of a page (or, in [`Page::fragments`], one piece of a line).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block {
    pub text: String,
    /// [x0, y0, x1, y1] in user space (y up).
    pub rect: [f64; 4],
    /// Font size in points.
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    /// The paragraph reads right to left (Arabic, Hebrew); `text` is in logical order.
    pub rtl: bool,
    /// The font family the PDF names for the text ("Sakkal Majalla"), when it can be told.
    pub font: Option<String>,
}

/// An image of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    /// "png" or "jpg".
    pub ext: &'static str,
    pub bytes: Vec<u8>,
    pub rect: [f64; 4],
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub width: f64,
    pub height: f64,
    pub blocks: Vec<Block>,
    pub images: Vec<Image>,
    /// The page's text in pieces that never span two table cells (one line each), for finding
    /// tables. Empty: tables are looked for among `blocks`.
    pub fragments: Vec<Block>,
    /// The horizontal and vertical rules the page draws ([x0, y0, x1, y1], user space): table
    /// borders. A grid of them is a table whatever its text.
    pub rules: Vec<[f64; 4]>,
}

/// How a cell takes part in a cell merged down several rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VMerge {
    #[default]
    None,
    /// The first row of a merged cell: it holds the text.
    Restart,
    /// A later row of the merged cell above (no text of its own).
    Continue,
}

/// One cell of a detected table. `span` is how many grid columns the cell covers (a header
/// row that stretches over sub-columns); `vmerge` joins it to the cells below. Lines of text are
/// separated by `\n`.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub text: String,
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    pub span: usize,
    /// The cell's text reads right to left.
    pub rtl: bool,
    pub vmerge: VMerge,
    pub font: Option<String>,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { text: String::new(), size: 11.0, bold: false, italic: false, span: 1, rtl: false, vmerge: VMerge::None, font: None }
    }
}

/// A table detected from the page's text layout: rows of cells, and the left edge of each
/// grid column (user space, y up) so the writers can size the columns.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub rect: [f64; 4],
    pub cols: Vec<f64>,
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    /// A right-to-left table: more of its cells with letters read right to left than left to
    /// right (numbers have no direction). Its first logical
    /// column is then the rightmost one.
    pub fn rtl(&self) -> bool {
        // Only cells with letters have a direction: numbers and symbols read the same either way.
        let (mut rtl, mut ltr) = (0usize, 0usize);
        for c in self.rows.iter().flatten() {
            if has_rtl(&c.text) {
                rtl += 1;
            } else if c.text.chars().any(|ch| ch.is_alphabetic()) {
                ltr += 1;
            }
        }
        rtl > ltr
    }

    /// A row's cells in the table's logical order: left to right for a left-to-right table;
    /// for a right-to-left one, padded to the full grid (holes stay where they are on the page)
    /// and then right to left.
    fn logical_row<'a>(&self, row: &'a [Cell]) -> std::borrow::Cow<'a, [Cell]> {
        if !self.rtl() {
            return std::borrow::Cow::Borrowed(row);
        }
        let mut cells = row.to_vec();
        let used: usize = row.iter().map(|c| c.span.max(1)).sum();
        for _ in used..self.cols.len() {
            cells.push(Cell { rtl: true, ..Cell::default() });
        }
        cells.reverse();
        std::borrow::Cow::Owned(cells)
    }
}

/// Whether `c` is written right to left (bidi class R or AL; Arabic-Indic digits, AN, go with
/// them in a run).
fn rtl_char(c: char) -> bool {
    use unicode_bidi::BidiClass::{AL, AN, R};
    matches!(unicode_bidi::bidi_class(c), R | AL | AN)
}

/// `text` cut into runs that are right to left or not, in a paragraph that reads right to left
/// (`rtl`) or not: each character goes where UAX #9 resolves it (odd embedding levels are right
/// to left), so a neutral between an Arabic word and a number ("القسم 1: ملخص") is in the run
/// Word places it with. European digits stay out of right-to-left runs (Word would otherwise
/// show them as Arabic-Indic digits under its "context" numeral setting); Arabic-Indic digits
/// stay in them. The runs keep logical order.
fn direction_runs(text: &str, rtl: bool) -> Vec<(bool, &str)> {
    use unicode_bidi::BidiClass::AN;
    let level = if rtl { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() };
    let info = unicode_bidi::BidiInfo::new(text, Some(level));
    let mut out: Vec<(bool, &str)> = Vec::new();
    let mut start = 0;
    let mut cur: Option<bool> = None;
    for (i, c) in text.char_indices() {
        let odd = info.levels.get(i).is_some_and(|l| l.is_rtl());
        let dir = odd || unicode_bidi::bidi_class(c) == AN;
        match cur {
            Some(a) if a != dir => {
                if let Some(t) = text.get(start..i) {
                    out.push((a, t));
                }
                start = i;
                cur = Some(dir);
            }
            None => cur = Some(dir),
            _ => {}
        }
    }
    if let Some(t) = text.get(start..).filter(|t| !t.is_empty()) {
        out.push((cur.unwrap_or(false), t));
    }
    out
}

fn has_rtl(text: &str) -> bool {
    text.chars().any(rtl_char)
}

/// What goes into the output, in order.
enum Item<'a> {
    /// A paragraph, its heading level (0: body text) and the space above it on the page
    /// (points, from the item above; 0 at the top of a page).
    Para(&'a Block, u8, f64),
    /// Owned: tables are detected per page inside [`items`], so they cannot borrow.
    Table(Table),
    Img(&'a Image),
    /// The end of page `n` (0-based) and the start of the next.
    PageBreak(usize),
}

/// The body text size: the size most of the document's characters are set in.
fn body_size(pages: &[Page]) -> f64 {
    let mut by: Vec<(i64, usize)> = Vec::new();
    for b in pages.iter().flat_map(|p| &p.blocks) {
        let k = (b.size * 2.0).round() as i64;
        match by.iter_mut().find(|e| e.0 == k) {
            Some(e) => e.1 += b.text.len(),
            None => by.push((k, b.text.len())),
        }
    }
    by.into_iter().max_by_key(|e| e.1).map_or(11.0, |e| e.0 as f64 / 2.0)
}

/// Heading level for a block: 1 for ≥ 1.6× the body size, 2 for ≥ 1.25×, 0 for body text.
fn level(b: &Block, body: f64) -> u8 {
    let short = b.text.len() < 200;
    if short && b.size >= body * 1.6 {
        1
    } else if short && (b.size >= body * 1.25 || (b.bold && b.size >= body && b.text.len() < 80 && !b.text.ends_with('.'))) {
        2
    } else {
        0
    }
}

/// How far two cell left edges may drift (points) and still be the same grid column.
const COL_TOL: f64 = 4.0;
/// Room after the text of a borderless table's last column (points): the gap the other columns
/// get from the next column's start, at least Word's cell margins (0.08 in each side), so the
/// widest text of the last column doesn't wrap in Word.
const END_PAD: f64 = 12.0;
/// More grid columns than this isn't a table. It also bounds a table's size: every row is
/// materialized to all its columns, so a hostile page laid out as a staircase of blocks would
/// otherwise make (blocks / 2)² cells.
const MAX_COLS: usize = 64;
/// A cell is short: taller blocks are body text (or a multi-column layout), not table cells.
fn is_cell_like(b: &Block) -> bool {
    (b.rect[3] - b.rect[1]) <= b.size * 5.0 && !b.text.trim().is_empty()
}

/// Detect tables in a page's blocks from their text layout: columns are left edges shared by
/// several short blocks, rows are blocks whose vertical spans overlap. Returns the tables and,
/// per block, whether a table consumed it (so `items` can drop it from the paragraph flow).
fn tables(blocks: &[Block]) -> (Vec<Table>, Vec<bool>) {
    let mut consumed = vec![false; blocks.len()];
    // Candidate columns: cluster the left edges of cell-like blocks.
    let mut xs: Vec<(f64, usize)> = blocks.iter().enumerate().filter(|(_, b)| is_cell_like(b)).map(|(i, b)| (b.rect[0], i)).collect();
    xs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut cols: Vec<(f64, Vec<usize>)> = Vec::new(); // (left edge, member block indices)
    for (x, i) in xs {
        match cols.last_mut() {
            Some((e, m)) if (x - *e).abs() <= COL_TOL => {
                *e = (*e * m.len() as f64 + x) / (m.len() + 1) as f64;
                m.push(i);
            }
            _ => cols.push((x, vec![i])),
        }
    }
    let cols: Vec<(f64, Vec<usize>)> = cols.into_iter().filter(|(_, m)| m.len() >= 2).collect();
    if cols.len() < 2 {
        return (Vec::new(), consumed);
    }
    // Each block's column (an index into `cols`), so lookups stay linear on pages with many blocks.
    let mut col_of: Vec<Option<usize>> = vec![None; blocks.len()];
    for (ci, (_, members)) in cols.iter().enumerate() {
        for &i in members {
            if let Some(c) = col_of.get_mut(i) {
                *c = Some(ci);
            }
        }
    }
    // The column whose edge is within tolerance of `x` (edges are in ascending order).
    let column_at = |x: f64| -> Option<usize> {
        let k = cols.partition_point(|(e, _)| *e < x - COL_TOL);
        cols.get(k).filter(|(e, _)| (*e - x).abs() <= COL_TOL).map(|_| k)
    };
    // Assign every cell-like block to its nearest column (within tolerance).
    let mut rows: Vec<Vec<(usize, usize)>> = Vec::new(); // (column index, block index), top to bottom
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    order.sort_by(|a, b| blocks[*b].rect[3].total_cmp(&blocks[*a].rect[3]).then(blocks[*a].rect[0].total_cmp(&blocks[*b].rect[0])));
    for &bi in &order {
        let b = &blocks[bi];
        if !is_cell_like(b) {
            continue;
        }
        let Some(ci) = column_at(b.rect[0]) else { continue };
        let joins = rows
            .last()
            .and_then(|row| row.first())
            .map(|(_, fi)| {
                let rb = &blocks[*fi];
                let overlap = rb.rect[3].min(b.rect[3]) - rb.rect[1].max(b.rect[1]);
                overlap > 0.5 * (rb.rect[3] - rb.rect[1]).min(b.rect[3] - b.rect[1])
            })
            .unwrap_or(false);
        if joins {
            if let Some(row) = rows.last_mut() {
                row.push((ci, bi));
            }
        } else {
            rows.push(vec![(ci, bi)]);
        }
    }
    // Runs of consecutive rows that have two or more distinct columns are tables.
    let mut out = Vec::new();
    let flush = |run: &mut Vec<Vec<(usize, usize)>>, out: &mut Vec<Table>, consumed: &mut Vec<bool>| {
        if run.len() < 2 {
            run.clear();
            return;
        }
        let mut used: Vec<usize> = run.iter().flatten().map(|(_, i)| *i).collect();
        used.sort_unstable();
        used.dedup();
        let mut edges: Vec<f64> = used.iter().filter_map(|&i| col_of.get(i).copied().flatten()).filter_map(|ci| cols.get(ci)).map(|c| c.0).collect();
        edges.sort_by(|a, b| a.total_cmp(b));
        edges.dedup_by(|a, b| (*a - *b).abs() <= COL_TOL);
        let ncols = edges.len();
        let ok = (ncols >= 3 || (ncols == 2 && run.len() >= 3)) && ncols <= MAX_COLS;
        if !ok {
            run.clear();
            return;
        }
        let right = used.iter().map(|&i| blocks[i].rect[2]).fold(f64::MIN, f64::max) + COL_TOL + END_PAD;
        let mut rows_out = Vec::new();
        let mut slots: Vec<Option<Cell>> = vec![None; ncols];
        for row in run.drain(..) {
            for slot in slots.iter_mut() {
                *slot = None;
            }
            for (_ci, bi) in row {
                let b = &blocks[bi];
                let Some(slot) = edges.iter().position(|e| (*e - b.rect[0]).abs() <= COL_TOL) else { continue };
                let span = edges[slot + 1..].iter().filter(|e| **e > b.rect[0] + COL_TOL && **e < b.rect[2] - COL_TOL).count() + 1;
                let cell = Cell {
                    text: b.text.trim().to_string(),
                    size: b.size,
                    bold: b.bold,
                    italic: b.italic,
                    span,
                    rtl: b.rtl,
                    font: b.font.clone(),
                    ..Cell::default()
                };
                match &mut slots[slot] {
                    Some(c) => {
                        c.text.push(' ');
                        c.text.push_str(&cell.text);
                    }
                    None => slots[slot] = Some(cell),
                }
                consumed[bi] = true;
            }
            let mut materialized = Vec::with_capacity(ncols);
            let mut column = 0;
            while column < ncols {
                match slots[column].take() {
                    Some(mut cell) => {
                        cell.span = cell.span.min(ncols - column).max(1);
                        column += cell.span;
                        materialized.push(cell);
                    }
                    None => {
                        column += 1;
                        materialized.push(Cell::default());
                    }
                }
            }
            while materialized.last().is_some_and(|cell| cell.text.is_empty()) && materialized.iter().any(|cell| cell.span > 1) {
                materialized.pop();
            }
            rows_out.push(materialized);
        }
        let rect = [
            edges[0],
            used.iter().map(|&i| blocks[i].rect[1]).fold(f64::MAX, f64::min),
            right,
            used.iter().map(|&i| blocks[i].rect[3]).fold(f64::MIN, f64::max),
        ];
        out.push(Table { rect, cols: edges, rows: rows_out });
    };
    // Runs of consecutive multi-column rows are tables. Rows on either side of a run that
    // hold a single cell wide enough to span two columns (a header band, a totals row) join
    // the table; other single-column rows (captions, following text) stay paragraphs.
    let spans_run = |edges: &[f64], bi: usize| -> bool {
        let b = &blocks[bi];
        edges.iter().any(|e| (*e - b.rect[0]).abs() <= COL_TOL) && edges.iter().any(|e| *e > b.rect[0] + COL_TOL && *e < b.rect[2] - COL_TOL)
    };
    let run_edges = |run: &[Vec<(usize, usize)>]| -> Vec<f64> {
        let mut es: Vec<f64> =
            run.iter().flatten().filter_map(|(_, i)| col_of.get(*i).copied().flatten()).filter_map(|ci| cols.get(ci)).map(|c| c.0).collect();
        es.sort_by(|a, b| a.total_cmp(b));
        es.dedup_by(|a, b| (*a - *b).abs() <= COL_TOL);
        es
    };
    let qual: Vec<bool> = rows
        .iter()
        .map(|row| {
            let mut cs: Vec<usize> = row.iter().map(|(c, _)| *c).collect();
            cs.sort_unstable();
            cs.dedup();
            cs.len() >= 2
        })
        .collect();
    let mut i = 0;
    while i < rows.len() {
        if !qual[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && qual[i] {
            i += 1;
        }
        let end = i; // run = rows[start..end]
        let mut pieces: Vec<Vec<(usize, usize)>> = Vec::new();
        if start > 0 && rows[start - 1].len() == 1 && spans_run(&run_edges(&rows[start..end]), rows[start - 1][0].1) {
            pieces.push(rows[start - 1].clone());
        }
        pieces.extend(rows[start..end].iter().cloned());
        if end < rows.len() && rows[end].len() == 1 && spans_run(&run_edges(&rows[start..end]), rows[end][0].1) {
            pieces.push(rows[end].clone());
        }
        flush(&mut pieces, &mut out, &mut consumed);
    }
    (out, consumed)
}

/// The tables on a page and, per block, whether a table holds its text: tables drawn with rules
/// (from the page's fragments, see `grid`), then tables laid out in columns without rules (from
/// the blocks, by their start edges).
fn page_tables(p: &Page) -> (Vec<Table>, Vec<bool>) {
    let ruled: Vec<Table> =
        if p.fragments.is_empty() { Vec::new() } else { grid::ruled_tables(&p.rules, &p.fragments).into_iter().map(|(t, _)| t).collect() };
    let inside: Vec<bool> = p.blocks.iter().map(|b| ruled.iter().any(|t| mostly_inside(b.rect, t.rect))).collect();
    let rest: Vec<(usize, &Block)> = p.blocks.iter().enumerate().filter(|(i, _)| !inside.get(*i).copied().unwrap_or(false)).collect();
    let rest_blocks: Vec<Block> = rest.iter().map(|(_, b)| (*b).clone()).collect();
    let (unruled, consumed_rest) = tables_by_direction(&rest_blocks);
    let mut consumed = inside;
    for ((i, _), c) in rest.iter().zip(consumed_rest) {
        if c && let Some(slot) = consumed.get_mut(*i) {
            *slot = true;
        }
    }
    let mut all = ruled;
    all.extend(unruled);
    (all, consumed)
}

/// Whether most of `r` lies inside `table` (a paragraph that is a table's text).
fn mostly_inside(r: [f64; 4], table: [f64; 4]) -> bool {
    let w = (r[2].min(table[2] + 1.0) - r[0].max(table[0] - 1.0)).max(0.0);
    let h = (r[3].min(table[3] + 1.0) - r[1].max(table[1] - 1.0)).max(0.0);
    let area = (r[2] - r[0]).max(0.0) * (r[3] - r[1]).max(0.0);
    if area <= f64::EPSILON {
        let (cx, cy) = ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0);
        return cx >= table[0] && cx <= table[2] && cy >= table[1] && cy <= table[3];
    }
    w * h > 0.5 * area
}

/// [`tables`], with columns found by their start edge: on a page of mostly right-to-left text,
/// cells line up on the right (their left edges are ragged), so the page is mirrored, its
/// tables found, and the tables mirrored back.
fn tables_by_direction(blocks: &[Block]) -> (Vec<Table>, Vec<bool>) {
    let rtl: usize = blocks.iter().filter(|b| b.rtl).map(|b| b.text.chars().count()).sum();
    let all: usize = blocks.iter().map(|b| b.text.chars().count()).sum();
    if rtl * 2 <= all {
        return tables(blocks);
    }
    let mirror = |r: [f64; 4]| [-r[2], r[1], -r[0], r[3]];
    let mirrored: Vec<Block> = blocks.iter().map(|b| Block { rect: mirror(b.rect), ..b.clone() }).collect();
    let (found, consumed) = tables(&mirrored);
    let back = found
        .into_iter()
        .map(|t| {
            // Column j spans [cols[j], cols[j+1] or the right edge] mirrored; back on the page
            // the columns run the other way.
            let n = t.cols.len();
            let ends: Vec<f64> = (0..n).map(|j| t.cols.get(j + 1).copied().unwrap_or(t.rect[2])).collect();
            let mut cols: Vec<f64> = ends.iter().map(|e| -e).collect();
            cols.sort_by(f64::total_cmp);
            let rows = t
                .rows
                .into_iter()
                .map(|mut row| {
                    // Pad to the full grid, then read it the other way (left to right on the page).
                    let used: usize = row.iter().map(|c| c.span.max(1)).sum();
                    for _ in used..n {
                        row.push(Cell::default());
                    }
                    row.reverse();
                    row
                })
                .collect();
            Table { rect: mirror(t.rect), cols, rows }
        })
        .collect();
    (back, consumed)
}

fn items(pages: &[Page]) -> Vec<Item<'_>> {
    let body = body_size(pages);
    let mut out = Vec::new();
    for (i, p) in pages.iter().enumerate() {
        if i > 0 {
            out.push(Item::PageBreak(i - 1));
        }
        let (tables, consumed) = page_tables(p);
        // Blocks and images by their top edge, top to bottom.
        let mut parts: Vec<([f64; 4], Item)> = p
            .blocks
            .iter()
            .enumerate()
            .filter(|(bi, _)| !consumed.get(*bi).copied().unwrap_or(false))
            .map(|(_, b)| (b.rect, Item::Para(b, level(b, body), 0.0)))
            .collect();
        parts.extend(tables.into_iter().map(|t| (t.rect, Item::Table(t))));
        parts.extend(p.images.iter().map(|im| (im.rect, Item::Img(im))));
        parts.sort_by(|a, b| b.0[3].total_cmp(&a.0[3]).then(a.0[0].total_cmp(&b.0[0])));
        // The gap above each paragraph: from the bottom of the item above it.
        let mut above: Option<f64> = None;
        for (rect, mut item) in parts {
            if let (Item::Para(_, _, gap), Some(bottom)) = (&mut item, above) {
                let g = bottom - rect[3];
                *gap = if g.is_finite() { g.max(0.0) } else { 0.0 };
            }
            above = Some(above.map_or(rect[1], |a| a.min(rect[1])));
            out.push(item);
        }
    }
    out
}

/// Whether a character may appear in the output. XML 1.0 forbids the C0 controls other than tab,
/// line feed and carriage return, and U+FFFE/U+FFFF. Text extracted from PDFs often holds them
/// (fonts without a usable `/ToUnicode`), and Word refuses a whole document over one (#72).
fn allowed(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || !(c.is_ascii_control() || c == '\u{FFFE}' || c == '\u{FFFF}')
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars().filter(|c| allowed(*c)) {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(A[(n >> 18) as usize & 63] as char);
        s.push(A[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    s
}

/// One HTML file: headings and paragraphs, tables as real `<table>` elements, images inline, a
/// rule between pages.
pub fn html(pages: &[Page], title: &str) -> String {
    let mut s = format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n<style>body{{max-width:46em;margin:2em auto;font-family:sans-serif;line-height:1.45}}\
img{{max-width:100%}}hr{{border:0;border-top:1px solid #ccc;margin:2em 0}}\
table{{border-collapse:collapse;margin:1em 0}}td,th{{border:1px solid #999;padding:2px 6px;vertical-align:top}}</style>\n</head>\n<body>\n",
        esc(title)
    );
    for it in items(pages) {
        match it {
            Item::Para(b, lvl, _) => {
                let mut t = esc(&b.text);
                if b.italic {
                    t = format!("<em>{t}</em>");
                }
                if b.bold && lvl == 0 {
                    t = format!("<strong>{t}</strong>");
                }
                let dir = if b.rtl { " dir=\"rtl\"" } else { "" };
                match lvl {
                    0 => s.push_str(&format!("<p{dir}>{t}</p>\n")),
                    l => s.push_str(&format!("<h{l}{dir}>{t}</h{l}>\n")),
                }
            }
            Item::Table(t) => {
                let rtl = t.rtl();
                s.push_str(if rtl { "<table dir=\"rtl\" style=\"margin-left:auto\">\n" } else { "<table>\n" });
                let rows: Vec<Vec<Cell>> = t.rows.iter().map(|r| t.logical_row(r).into_owned()).collect();
                // The grid column each cell starts at, per row (for rows spanned from above).
                let starts: Vec<Vec<usize>> = rows
                    .iter()
                    .map(|r| {
                        let mut at = 0usize;
                        r.iter()
                            .map(|c| {
                                let here = at;
                                at = at.saturating_add(c.span.max(1));
                                here
                            })
                            .collect()
                    })
                    .collect();
                let continues = |ri: usize, col: usize| -> bool {
                    rows.get(ri).zip(starts.get(ri)).is_some_and(|(r, st)| r.iter().zip(st).any(|(c, s)| *s == col && c.vmerge == VMerge::Continue))
                };
                for (ri, row) in rows.iter().enumerate() {
                    s.push_str("<tr>");
                    for (c, col) in row.iter().zip(starts.get(ri).map(Vec::as_slice).unwrap_or_default()) {
                        if c.vmerge == VMerge::Continue {
                            continue; // covered by the rowspan of the cell above
                        }
                        let mut down = 1usize;
                        if c.vmerge == VMerge::Restart {
                            while continues(ri + down, *col) {
                                down += 1;
                            }
                        }
                        let rowspan = if down > 1 { format!(" rowspan=\"{down}\"") } else { String::new() };
                        let mut body = esc(&c.text).replace('\n', "<br>");
                        if c.italic {
                            body = format!("<em>{body}</em>");
                        }
                        if c.bold {
                            body = format!("<strong>{body}</strong>");
                        }
                        let span = if c.span > 1 { format!(" colspan=\"{}\"", c.span) } else { String::new() };
                        let dir = match (c.rtl, rtl) {
                            _ if c.text.is_empty() => "",
                            (true, false) => " dir=\"rtl\"",
                            (false, true) => " dir=\"ltr\"",
                            _ => "",
                        };
                        s.push_str(&format!("<td{span}{rowspan}{dir}>{body}</td>"));
                    }
                    s.push_str("</tr>\n");
                }
                s.push_str("</table>\n");
            }
            Item::Img(im) => {
                let mime = if im.ext == "jpg" { "image/jpeg" } else { "image/png" };
                s.push_str(&format!("<p><img alt=\"\" src=\"data:{mime};base64,{}\"></p>\n", base64(&im.bytes)));
            }
            Item::PageBreak(_) => s.push_str("<hr>\n"),
        }
    }
    s.push_str("</body>\n</html>\n");
    s
}

/// A formatted text run (shared by paragraphs and table cells).
/// Text with right-to-left letters is split into runs by direction; the right-to-left ones are
/// marked `w:rtl` and carry the complex-script bold, italic and size Word uses for them, and
/// their language (Arabic or Hebrew, for digits and proofing). `font` names the family for every
/// script (Word picks the complex-script font for right-to-left text); without one the
/// document's defaults apply.
///
/// In a paragraph with right-to-left text every run carries the complex-script size, bold and
/// italic too: word processors format weak characters (digits, punctuation) next to
/// right-to-left text with them.
fn run_xml(text: &str, rtl_para: bool, size: f64, bold: bool, italic: bool, font: Option<&str>) -> String {
    let half_points = if size.is_finite() { (size * 2.0).round().clamp(2.0, 3276.0) as i64 } else { 24 };
    let fonts = font.map(|f| format!("<w:rFonts w:ascii=\"{0}\" w:hAnsi=\"{0}\" w:cs=\"{0}\"/>", esc(f))).unwrap_or_default();
    let one = |text: &str, rtl: bool, cs: bool| {
        let mut rpr = fonts.clone();
        if bold {
            rpr.push_str(if cs { "<w:b/><w:bCs/>" } else { "<w:b/>" });
        }
        if italic {
            rpr.push_str(if cs { "<w:i/><w:iCs/>" } else { "<w:i/>" });
        }
        rpr.push_str(&format!("<w:sz w:val=\"{half_points}\"/>"));
        if cs {
            rpr.push_str(&format!("<w:szCs w:val=\"{half_points}\"/>"));
        }
        if rtl {
            rpr.push_str("<w:rtl/>");
            let hebrew = text.chars().find(|c| rtl_char(*c)).is_some_and(|c| matches!(c, '\u{0590}'..='\u{05FF}' | '\u{FB1D}'..='\u{FB4F}'));
            rpr.push_str(if hebrew { "<w:lang w:bidi=\"he-IL\"/>" } else { "<w:lang w:bidi=\"ar-SA\"/>" });
        }
        format!("<w:r><w:rPr>{rpr}</w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r>", esc(text))
    };
    if !has_rtl(text) {
        return one(text, false, false);
    }
    direction_runs(text, rtl_para).into_iter().map(|(rtl, t)| one(t, rtl, true)).collect()
}

/// One Word table: explicit single borders (so it renders without a table style), a grid sized
/// from the detected column edges, `gridSpan` for spanning cells, empty cells for holes.
fn docx_table(t: &Table, max_width: i64) -> String {
    let mut edges = t.cols.clone();
    edges.push(t.rect[2]);
    let rtl = t.rtl();
    let mut widths: Vec<i64> = edges.windows(2).map(|w| ((w[1] - w[0]).max(20.0) * 20.0).round().clamp(60.0, 31680.0) as i64).collect();
    // Columns keep their widths from the page (a fixed layout: Word's autofit squeezes columns
    // to their text), scaled down together if the table is wider than the text area.
    let total: i64 = widths.iter().sum();
    if max_width > 0 && total > max_width {
        let k = max_width as f64 / total as f64;
        for w in &mut widths {
            *w = ((*w as f64 * k).round() as i64).max(60);
        }
    }
    let total: i64 = widths.iter().sum();
    if rtl {
        // A `bidiVisual` table lists its grid from the right.
        widths.reverse();
    }
    let border = "<w:top w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"999999\"/>";
    let mut s = format!("<w:tbl><w:tblPr>{}<w:tblW w:w=\"{total}\" w:type=\"dxa\"/><w:tblBorders>", if rtl { "<w:bidiVisual/>" } else { "" });
    for b in [
        border,
        "<w:left w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"999999\"/>",
        "<w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"999999\"/>",
        "<w:right w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"999999\"/>",
        "<w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"999999\"/>",
        "<w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"999999\"/>",
    ] {
        s.push_str(b);
    }
    s.push_str("</w:tblBorders><w:tblLayout w:type=\"fixed\"/></w:tblPr><w:tblGrid>");
    for w in &widths {
        s.push_str(&format!("<w:gridCol w:w=\"{w}\"/>"));
    }
    s.push_str("</w:tblGrid>");
    for row in &t.rows {
        s.push_str("<w:tr>");
        let row = t.logical_row(row);
        let mut column = 0usize;
        for c in row.iter() {
            let span = if c.span > 1 { format!("<w:gridSpan w:val=\"{}\"/>", c.span) } else { String::new() };
            let vmerge = match c.vmerge {
                VMerge::None => "",
                VMerge::Restart => "<w:vMerge w:val=\"restart\"/>",
                VMerge::Continue => "<w:vMerge/>",
            };
            let w: i64 = widths.iter().skip(column).take(c.span.max(1)).sum();
            column = column.saturating_add(c.span.max(1));
            let ppr = if c.rtl { "<w:pPr><w:bidi/></w:pPr>" } else { "" };
            // One paragraph per line of the cell; a cell needs at least one.
            let paras: String = if c.text.is_empty() {
                format!("<w:p>{ppr}</w:p>")
            } else {
                c.text
                    .split('\n')
                    .map(|line| format!("<w:p>{ppr}{}</w:p>", run_xml(line, c.rtl, c.size, c.bold, c.italic, c.font.as_deref())))
                    .collect()
            };
            s.push_str(&format!("<w:tc><w:tcPr><w:tcW w:w=\"{w}\" w:type=\"dxa\"/>{span}{vmerge}</w:tcPr>{paras}</w:tc>"));
        }
        // Pad the grid so every row covers all columns (Word rejects short rows).
        let used: usize = row.iter().map(|c| c.span).sum();
        for _ in used..t.cols.len() {
            s.push_str("<w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr><w:p/></w:tc>");
        }
        s.push_str("</w:tr>");
    }
    s.push_str("</w:tbl>");
    s
}

/// Font names listed in a Word file's font table.
const MAX_FONTS: usize = 256;

/// A Word section's page: size and margins in twips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Section {
    w: i64,
    h: i64,
    top: i64,
    right: i64,
    bottom: i64,
    left: i64,
}

impl Section {
    /// Page `p`'s size, within what Word accepts (0.1 to 22 in; a long receipt is taller). Each
    /// margin is the PDF's own (the room its content leaves on that side), at most 1 in (less on
    /// small pages, so text keeps room) and at least 1/4 in: text never gets narrower than the
    /// PDF's, which would wrap more lines and push content onto extra pages.
    fn of(p: &Page) -> Section {
        let twips = |pt: f64| if pt.is_finite() { (pt * 20.0).round().clamp(144.0, 31680.0) as i64 } else { 12240 };
        let (w, h) = (twips(p.width), twips(p.height));
        let (dx, dy) = ((w / 8).min(1440), (h / 8).min(1440));
        // The content's box, clipped to the page (content off the page says nothing).
        let (pw, ph) = (if p.width.is_finite() { p.width } else { 0.0 }, if p.height.is_finite() { p.height } else { 0.0 });
        let rects = p.blocks.iter().map(|b| b.rect).chain(p.images.iter().map(|i| i.rect)).chain(p.rules.iter().copied());
        let mut bbox: Option<[f64; 4]> = None;
        for r in rects {
            let c = [r[0].max(0.0), r[1].max(0.0), r[2].min(pw), r[3].min(ph)];
            if !c.iter().all(|v| v.is_finite()) || c[2] < c[0] || c[3] < c[1] {
                continue;
            }
            bbox = Some(bbox.map_or(c, |b| [b[0].min(c[0]), b[1].min(c[1]), b[2].max(c[2]), b[3].max(c[3])]));
        }
        let margin = |room: Option<f64>, default: i64| match room {
            Some(pt) => ((pt * 20.0).round() as i64).clamp(default.min(360), default),
            None => default,
        };
        Section {
            w,
            h,
            top: margin(bbox.map(|b| ph - b[3]), dy),
            right: margin(bbox.map(|b| pw - b[2]), dx),
            bottom: margin(bbox.map(|b| b[1]), dy),
            left: margin(bbox.map(|b| b[0]), dx),
        }
    }

    /// How paragraph `b` sits between this page's margins, when that isn't its direction's
    /// start: centred, or (several lines filling the width) justified. Word aligns a paragraph to
    /// the start of its direction (right for `w:bidi`) by itself.
    fn alignment(&self, b: &Block) -> Option<&'static str> {
        let (left, right) = (self.left as f64 / 20.0, (self.w - self.right) as f64 / 20.0);
        let (x0, x1) = (b.rect[0], b.rect[2]);
        let width = right - left;
        if ![x0, x1, width].iter().all(|v| v.is_finite()) || width <= 0.0 || x1 <= x0 {
            return None;
        }
        let size = if b.size.is_finite() { b.size.max(1.0) } else { 11.0 };
        let lines = (b.rect[3] - b.rect[1]) / (size * 1.1);
        let (gap_left, gap_right) = (x0 - left, right - x1);
        if lines >= 1.8 && x1 - x0 >= 0.95 * width {
            return Some("both");
        }
        if x1 - x0 < 0.8 * width && gap_left > 0.05 * width && (gap_left - gap_right).abs() <= 0.04 * width.max(1.0) {
            return Some("center");
        }
        None
    }

    /// The text width, in twips.
    fn text_width(&self) -> i64 {
        (self.w - self.left - self.right).max(144)
    }

    fn xml(&self) -> String {
        let orient = if self.w > self.h { " w:orient=\"landscape\"" } else { "" };
        format!(
            "<w:sectPr><w:pgSz w:w=\"{}\" w:h=\"{}\"{orient}/><w:pgMar w:top=\"{}\" w:right=\"{}\" w:bottom=\"{}\" w:left=\"{}\" w:header=\"{}\" w:footer=\"{}\" w:gutter=\"0\"/></w:sectPr>",
            self.w,
            self.h,
            self.top,
            self.right,
            self.bottom,
            self.left,
            self.top / 2,
            self.bottom / 2
        )
    }
}

/// A Word document (.docx, Office Open XML): Heading 1/2 and Normal paragraphs, images inline at
/// their size on the page, one section per PDF page with that page's size and margins.
pub fn docx(pages: &[Page], title: &str) -> Vec<u8> {
    let sections: Vec<Section> = pages.iter().map(Section::of).collect();
    let letter = Section { w: 12240, h: 15840, top: 1440, right: 1440, bottom: 1440, left: 1440 };
    let section = |i: usize| sections.get(i).copied().unwrap_or(letter);
    let mut current = section(0);
    let mut body = String::new();
    let mut media: Vec<(String, &Image)> = Vec::new();
    // Word needs a paragraph between adjacent tables and after the last one in the body.
    let mut after_table = false;
    let run = |b: &Block| run_xml(&b.text, b.rtl, b.size, b.bold, b.italic, b.font.as_deref());
    for it in items(pages) {
        match &it {
            Item::Para(b, lvl, gap) => {
                let mut ppr = match lvl {
                    1 => "<w:pStyle w:val=\"Heading1\"/>".to_string(),
                    2 => "<w:pStyle w:val=\"Heading2\"/>".to_string(),
                    _ => String::new(),
                };
                if b.rtl {
                    ppr.push_str("<w:bidi/>");
                }
                // The space the PDF leaves above the paragraph, less the leading Word adds to
                // a line anyway; none after (the next paragraph says its own).
                if *gap > 0.0 {
                    let size = if b.size.is_finite() { b.size.clamp(1.0, 200.0) } else { 11.0 };
                    let before = ((gap - 0.2 * size).clamp(0.0, 72.0) * 20.0).round() as i64;
                    ppr.push_str(&format!("<w:spacing w:before=\"{before}\" w:after=\"0\"/>"));
                }
                if let Some(jc) = current.alignment(b) {
                    ppr.push_str(&format!("<w:jc w:val=\"{jc}\"/>"));
                }
                let ppr = if ppr.is_empty() { ppr } else { format!("<w:pPr>{ppr}</w:pPr>") };
                body.push_str(&format!("<w:p>{ppr}{}</w:p>", run(b)));
            }
            Item::Table(t) => {
                if after_table {
                    body.push_str("<w:p/>");
                }
                body.push_str(&docx_table(t, current.text_width()));
            }
            Item::Img(im) => {
                let n = media.len() + 1;
                let name = format!("image{n}.{}", im.ext);
                // Size on the page, in EMU (12700 per point), at most the text width.
                let text_w = current.text_width() as f64 / 20.0;
                let (w, h) = ((im.rect[2] - im.rect[0]).max(1.0), (im.rect[3] - im.rect[1]).max(1.0));
                let k = (text_w / w).min(1.0);
                let (cx, cy) = ((w * k * 12700.0) as i64, (h * k * 12700.0) as i64);
                body.push_str(&format!(
                    "<w:p><w:r><w:drawing><wp:inline><wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:docPr id=\"{n}\" name=\"Picture {n}\"/>\
<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:nvPicPr><pic:cNvPr id=\"{n}\" name=\"{name}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip r:embed=\"rIdImg{n}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>\
</a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"
                ));
                media.push((name, im));
            }
            Item::PageBreak(n) => {
                // A section ends with a paragraph carrying its properties; the next one starts
                // on a new page with its own size.
                body.push_str(&format!("<w:p><w:pPr>{}</w:pPr></w:p>", section(*n).xml()));
                current = section(n.saturating_add(1));
            }
        }
        after_table = matches!(it, Item::Table(_));
    }
    if after_table {
        body.push_str("<w:p/>");
    }
    let doc = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\"><w:body>{body}{}</w:body></w:document>",
        current.xml()
    );
    let mut types = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Default Extension=\"png\" ContentType=\"image/png\"/><Default Extension=\"jpg\" ContentType=\"image/jpeg\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/word/fontTable.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>",
    );
    types.push_str("</Types>");
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/></Relationships>";
    let mut doc_rels = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rIdStyles\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
<Relationship Id=\"rIdFonts\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/fontTable\" Target=\"fontTable.xml\"/>",
    );
    for (i, (name, _)) in media.iter().enumerate() {
        doc_rels.push_str(&format!(
            "<Relationship Id=\"rIdImg{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/{name}\"/>",
            i + 1
        ));
    }
    doc_rels.push_str("</Relationships>");
    let styles = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Arial\" w:hAnsi=\"Arial\" w:cs=\"Times New Roman\"/></w:rPr></w:rPrDefault></w:docDefaults>\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:pPr><w:spacing w:after=\"120\"/></w:pPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:pPr><w:keepNext/><w:spacing w:before=\"240\"/><w:outlineLvl w:val=\"0\"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading2\"><w:name w:val=\"heading 2\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:pPr><w:keepNext/><w:spacing w:before=\"200\"/><w:outlineLvl w:val=\"1\"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>\
</w:styles>";
    // The fonts the document asks for, by name only (nothing of a font is embedded).
    let mut families: std::collections::BTreeSet<&str> = ["Arial", "Times New Roman"].into_iter().collect();
    for b in pages.iter().flat_map(|p| p.blocks.iter().chain(&p.fragments)) {
        if let Some(f) = b.font.as_deref()
            && families.len() < MAX_FONTS
        {
            families.insert(f);
        }
    }
    let mut font_table = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:fonts xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
    );
    for f in &families {
        font_table.push_str(&format!("<w:font w:name=\"{}\"/>", esc(f)));
    }
    font_table.push_str("</w:fonts>");
    let core = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{}</dc:title></cp:coreProperties>",
        esc(title)
    );
    let mut z = Zip::default();
    z.add("[Content_Types].xml", types.as_bytes(), true);
    z.add("_rels/.rels", rels.as_bytes(), true);
    z.add("docProps/core.xml", core.as_bytes(), true);
    z.add("word/document.xml", doc.as_bytes(), true);
    z.add("word/styles.xml", styles.as_bytes(), true);
    z.add("word/fontTable.xml", font_table.as_bytes(), true);
    z.add("word/_rels/document.xml.rels", doc_rels.as_bytes(), true);
    for (name, im) in &media {
        z.add(&format!("word/media/{name}"), &im.bytes, false);
    }
    z.finish()
}

fn rtf_text(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars().filter(|c| allowed(*c)) {
        match c {
            '\\' | '{' | '}' => {
                o.push('\\');
                o.push(c);
            }
            c if (c as u32) < 128 => o.push(c),
            c => {
                // \uN with a ? fallback; UTF-16 units as signed 16-bit numbers.
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    o.push_str(&format!("\\u{}?", *u as i16));
                }
            }
        }
    }
    o
}

/// RTF text with right-to-left runs marked `\rtlch` and given the associated (complex-script)
/// size, bold and italic RTF readers use for them; text without right-to-left letters is as before.
fn rtf_runs(text: &str, rtl_para: bool, size: i64, bold: bool, italic: bool) -> String {
    if !has_rtl(text) {
        return rtf_text(text);
    }
    let mut assoc = format!("\\afs{size}");
    if bold {
        assoc.push_str("\\ab");
    }
    if italic {
        assoc.push_str("\\ai");
    }
    direction_runs(text, rtl_para)
        .into_iter()
        .map(|(rtl, t)| if rtl { format!("{{\\rtlch{assoc} {}}}", rtf_text(t)) } else { format!("{{\\ltrch {}}}", rtf_text(t)) })
        .collect()
}

/// Rich Text Format: paragraphs with their sizes and bold/italic, real table rows, page breaks
/// between pages (images are left out).
pub fn rtf(pages: &[Page]) -> String {
    let mut s = String::from("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Helvetica;}}\n");
    for it in items(pages) {
        match it {
            Item::Para(b, _, _) => {
                let size = (b.size * 2.0).round() as i64;
                let mut fmt = format!("\\fs{size}");
                if b.bold {
                    fmt.push_str("\\b");
                }
                if b.italic {
                    fmt.push_str("\\i");
                }
                // RTF alignment is absolute: a right-to-left paragraph starts on the right.
                let dir = if b.rtl { "\\rtlpar\\qr" } else { "" };
                s.push_str(&format!("{{\\pard{dir}{fmt} {}\\par}}\n", rtf_runs(&b.text, b.rtl, size, b.bold, b.italic)));
            }
            Item::Table(t) => {
                let mut edges = t.cols.clone();
                edges.push(t.rect[2]);
                let rtl = t.rtl();
                // A right-to-left row (`\rtlrow`) lists its cells from the right, each boundary
                // measured from the table's right edge.
                let cellx: Vec<i64> = if rtl {
                    edges.iter().rev().skip(1).map(|e| ((t.rect[2] - e) * 20.0).round() as i64).collect()
                } else {
                    edges.iter().skip(1).map(|e| (e * 20.0).round() as i64).collect()
                };
                for row in &t.rows {
                    let row = t.logical_row(row);
                    s.push_str(if rtl { "\\trowd\\rtlrow\\trgaph108" } else { "\\trowd\\trgaph108" });
                    // The row's cell boundaries come first: each cell ends at the right edge of
                    // the last grid column it spans.
                    let mut column = 0;
                    for c in row.iter() {
                        column += c.span.max(1);
                        // A cell merged down several rows: its first row and the rows below.
                        match c.vmerge {
                            VMerge::None => {}
                            VMerge::Restart => s.push_str("\\clvmgf"),
                            VMerge::Continue => s.push_str("\\clvmrg"),
                        }
                        if let Some(x) = cellx.get(column.min(cellx.len()).saturating_sub(1)) {
                            s.push_str(&format!("\\cellx{x}"));
                        }
                    }
                    for c in row.iter() {
                        let size = (c.size * 2.0).round() as i64;
                        let mut fmt = format!("\\intbl{}\\fs{size}", if c.rtl { "\\rtlpar\\qr" } else { "" });
                        if c.bold {
                            fmt.push_str("\\b");
                        }
                        if c.italic {
                            fmt.push_str("\\i");
                        }
                        // Lines of the cell break with `\line` inside one cell paragraph.
                        let text: Vec<String> = c.text.split('\n').map(|l| rtf_runs(l, c.rtl, size, c.bold, c.italic)).collect();
                        s.push_str(&format!("{{{fmt} {}}}\\cell", text.join("\\line ")));
                    }
                    s.push_str("\\row\n");
                }
            }
            Item::Img(_) => {}
            Item::PageBreak(_) => s.push_str("\\page\n"),
        }
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Page {
        let b = |t: &str, y: f64, size: f64, bold: bool| Block {
            text: t.into(),
            rect: [72.0, y, 500.0, y + size],
            size,
            bold,
            italic: false,
            rtl: false,
            font: None,
        };
        Page {
            width: 612.0,
            height: 792.0,
            blocks: vec![
                b("Body text after the picture, which is long enough to be the main text size of the page.", 400.0, 11.0, false),
                b("Annual Report", 700.0, 24.0, true),
                b("Overview", 650.0, 14.0, true),
                b("Some <body> text & more of it so that eleven points is the body size here.", 620.0, 11.0, false),
            ],
            images: vec![Image {
                ext: "png",
                bytes: b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x03".to_vec(),
                rect: [72.0, 450.0, 272.0, 600.0],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn html_has_headings_paragraphs_and_images_in_order() {
        let h = html(&[page(), page()], "Report");
        let order: Vec<usize> = ["<h1>Annual Report</h1>", "<h2>Overview</h2>", "<p>Some &lt;body&gt; text &amp;", "<img", "<p>Body text after"]
            .iter()
            .map(|x| h.find(x).unwrap_or_else(|| panic!("{x} missing in {h}")))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        assert_eq!(h.matches("<hr>").count(), 1);
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
    }

    #[test]
    fn docx_is_a_word_package() {
        let d = docx(&[page()], "Report");
        assert!(d.starts_with(b"PK"));
        let names: Vec<&str> =
            ["[Content_Types].xml", "word/document.xml", "word/styles.xml", "word/_rels/document.xml.rels", "word/media/image1.png", "_rels/.rels"]
                .to_vec();
        for n in names {
            assert!(d.windows(n.len()).any(|w| w == n.as_bytes()), "{n}");
        }
    }

    /// One part of a package written by [`Zip`], inflated.
    fn part(zip: &[u8], name: &str) -> String {
        let at = |i: usize, n: usize| zip[i..i + n].iter().rev().fold(0usize, |v, b| v << 8 | *b as usize);
        let mut i = 0;
        while zip[i..].starts_with(b"PK\x03\x04") {
            let (size, name_len) = (at(i + 18, 4), at(i + 26, 2));
            let data = i + 30 + name_len;
            if &zip[i + 30..data] == name.as_bytes() {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut flate2::read::DeflateDecoder::new(&zip[data..data + size]), &mut s).unwrap();
                return s;
            }
            i = data + size;
        }
        panic!("{name} missing")
    }

    #[test]
    fn word_opens_text_with_control_characters_and_long_pages() {
        // #72: text extracted without a usable /ToUnicode holds C0 controls, which XML forbids;
        // Word then refused the whole file. A till receipt is also taller than Word's 22 in.
        let mut p = page();
        p.height = 2400.0;
        p.blocks.push(Block {
            text: "Total\u{0}\u{3}\u{c} 4.50\u{FFFF}\tGBP".into(),
            rect: [72.0, 100.0, 300.0, 110.0],
            size: f64::NAN,
            bold: false,
            italic: false,
            rtl: false,
            font: None,
        });
        let xml = part(&docx(&[p.clone()], "Receipt\u{1}"), "word/document.xml");
        assert!(xml.contains(">Total 4.50\tGBP</w:t>"), "{xml}");
        assert!(!xml.chars().any(|c| !allowed(c)));
        assert!(xml.contains("<w:pgSz w:w=\"12240\" w:h=\"31680\"/>"), "clamped to 22 in: {xml}");
        assert!(xml.contains("<w:sz w:val=\"24\"/>"), "a non-finite size falls back to 12 pt");
        assert!(rtf(&[p]).contains("Total 4.50\tGBP"));
        // A narrow page keeps room for text: margins shrink.
        let narrow = Page { width: 100.0, height: 200.0, ..page() };
        let xml = part(&docx(&[narrow], "Narrow"), "word/document.xml");
        assert!(xml.contains("<w:pgMar w:top=\"500\" w:right=\"250\" w:bottom=\"500\" w:left=\"250\""), "{xml}");
    }

    /// Each PDF page is a Word section with its own size, orientation and margins (the room
    /// the PDF's content leaves, never more than 1 in), so pages don't reflow onto others.
    #[test]
    fn every_page_is_a_section_with_its_size_and_margins() {
        let block = |text: &str, rect: [f64; 4]| Block { text: text.into(), rect, size: 12.0, ..Block::default() };
        // A4 portrait, text from 36 pt to 559 pt across and up to 806 pt.
        let a4 = Page {
            width: 595.0,
            height: 842.0,
            blocks: vec![block("First", [36.0, 790.0, 559.0, 806.0]), block("Second", [36.0, 740.0, 300.0, 752.0])],
            ..Default::default()
        };
        // Letter landscape, a 2 in margin on the left (more than 1 in: capped).
        let wide = Page { width: 792.0, height: 612.0, blocks: vec![block("Wide", [144.0, 500.0, 700.0, 512.0])], ..Default::default() };
        let xml = part(&docx(&[a4, wide, Page { width: 612.0, height: 792.0, ..Default::default() }], "Sections"), "word/document.xml");
        assert_eq!(xml.matches("<w:sectPr>").count(), 3, "{xml}");
        assert!(!xml.contains("w:type=\"page\""), "sections, not page breaks: {xml}");
        assert!(
            xml.contains(
                "<w:p><w:pPr><w:sectPr><w:pgSz w:w=\"11900\" w:h=\"16840\"/><w:pgMar w:top=\"720\" w:right=\"720\" w:bottom=\"1440\" w:left=\"720\""
            ),
            "{xml}"
        );
        assert!(xml.contains("<w:pgSz w:w=\"15840\" w:h=\"12240\" w:orient=\"landscape\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\""), "{xml}");
        // An empty page keeps the defaults and is the body's last section.
        assert!(xml.ends_with("<w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/></w:sectPr></w:body></w:document>"), "{xml}");
        // The gap between the two paragraphs (38 pt less 0.2 em of leading) is space before
        // the second; the first on a page has none.
        assert!(
            xml.contains(
                "<w:spacing w:before=\"712\" w:after=\"0\"/></w:pPr><w:r><w:rPr><w:sz w:val=\"24\"/></w:rPr><w:t xml:space=\"preserve\">Second"
            ),
            "{xml}"
        );
        assert!(xml.contains("<w:p><w:r><w:rPr><w:sz w:val=\"24\"/></w:rPr><w:t xml:space=\"preserve\">First"), "{xml}");
        // Hostile sizes still give a valid section.
        let odd = Page { width: f64::NAN, height: -5.0, blocks: vec![block("x", [f64::INFINITY, 0.0, 1.0, f64::NAN])], ..Default::default() };
        let xml = part(&docx(&[odd], "Odd"), "word/document.xml");
        assert!(xml.contains("<w:pgSz w:w=\"12240\" w:h=\"144\" w:orient=\"landscape\"/>"), "{xml}");
    }

    /// Runs name their font family for every script, right-to-left runs say their language, the
    /// font table lists the names with the fallbacks, and paragraphs keep a centred or justified
    /// placement (start-aligned ones are left to their direction).
    #[test]
    fn word_runs_name_fonts_languages_and_alignment() {
        let block = |text: &str, rect: [f64; 4], font: Option<&str>| Block {
            text: text.into(),
            rect,
            size: 12.0,
            rtl: has_rtl(text),
            font: font.map(str::to_string),
            ..Block::default()
        };
        let p = Page {
            width: 612.0,
            height: 792.0,
            blocks: vec![
                // Centred on the 72–540 text area.
                block("تقرير", [276.0, 700.0, 336.0, 714.0], Some("Sakkal Majalla")),
                // Right-aligned (the start of right-to-left text): no jc.
                block("שלום עולם", [400.0, 650.0, 540.0, 664.0], None),
                // Three lines filling the width: justified.
                block("نص طويل يملأ السطر كله", [72.0, 560.0, 540.0, 600.0], Some("Sakkal Majalla")),
            ],
            ..Default::default()
        };
        let d = docx(&[p], "Fonts");
        let xml = part(&d, "word/document.xml");
        assert!(
            xml.contains("<w:r><w:rPr><w:rFonts w:ascii=\"Sakkal Majalla\" w:hAnsi=\"Sakkal Majalla\" w:cs=\"Sakkal Majalla\"/><w:sz w:val=\"24\"/><w:szCs w:val=\"24\"/><w:rtl/><w:lang w:bidi=\"ar-SA\"/></w:rPr><w:t xml:space=\"preserve\">تقرير"),
            "{xml}"
        );
        assert!(xml.contains("<w:lang w:bidi=\"he-IL\"/>"), "{xml}");
        assert_eq!(xml.matches("<w:jc w:val=\"center\"/>").count(), 1, "{xml}");
        assert_eq!(xml.matches("<w:jc w:val=\"both\"/>").count(), 1, "{xml}");
        assert_eq!(xml.matches("<w:jc ").count(), 2, "{xml}");
        let fonts = part(&d, "word/fontTable.xml");
        assert!(fonts.contains("<w:font w:name=\"Arial\"/><w:font w:name=\"Sakkal Majalla\"/><w:font w:name=\"Times New Roman\"/>"), "{fonts}");
        assert!(part(&d, "word/styles.xml").contains("<w:rFonts w:ascii=\"Arial\" w:hAnsi=\"Arial\" w:cs=\"Times New Roman\"/>"));
        assert!(part(&d, "word/_rels/document.xml.rels").contains("Target=\"fontTable.xml\""));
        assert!(part(&d, "[Content_Types].xml").contains("/word/fontTable.xml"));
        // A name that would break the XML is escaped.
        let odd = Page { blocks: vec![block("x", [72.0, 700.0, 80.0, 712.0], Some("A&B\"<"))], ..Default::default() };
        assert!(part(&docx(&[odd], "Odd"), "word/fontTable.xml").contains("A&amp;B&quot;&lt;"));
    }

    /// Runs follow UAX #9: in a right-to-left heading "القسم 1: ملخص", the colon after the number
    /// is between a number (R for neutrals) and Arabic, so it is right to left; in a
    /// left-to-right paragraph an Arabic word and the spaces inside it form the only
    /// right-to-left run.
    #[test]
    fn direction_runs_follow_the_bidi_algorithm() {
        assert_eq!(direction_runs("القسم 1: ملخص الأداء", true), [(true, "القسم "), (false, "1"), (true, ": ملخص الأداء")]);
        assert_eq!(direction_runs("See سلام عليكم now", false), [(false, "See "), (true, "سلام عليكم"), (false, " now")]);
        assert_eq!(direction_runs("الكمية ١٢ قطعة", true), [(true, "الكمية ١٢ قطعة")]);
        assert_eq!(direction_runs("", true), []);
    }

    #[test]
    fn rtf_escapes_and_sizes() {
        let mut p = page();
        p.blocks.push(Block {
            text: "Café {x}".into(),
            rect: [72.0, 100.0, 200.0, 110.0],
            size: 10.0,
            bold: false,
            italic: true,
            rtl: false,
            font: None,
        });
        let r = rtf(&[p]);
        assert!(r.starts_with("{\\rtf1") && r.ends_with('}'));
        assert!(r.contains("\\fs48\\b Annual Report"));
        assert!(r.contains("\\fs20\\i Caf\\u233? \\{x\\}"), "{r}");
    }

    /// A 3-column table with a spanning header row and a hole in the last row.
    fn table_page() -> Page {
        let cell = |t: &str, x: f64, y: f64, w: f64| Block {
            text: t.into(),
            rect: [x, y, x + w, y + 12.0],
            size: 11.0,
            bold: false,
            italic: false,
            rtl: false,
            font: None,
        };
        Page {
            width: 612.0,
            height: 792.0,
            blocks: vec![
                cell("Meter", 72.0, 620.0, 120.0),
                cell("Unit", 232.0, 620.0, 80.0),
                cell("Reading", 392.0, 620.0, 100.0),
                cell("A1", 72.0, 590.0, 120.0),
                cell("kWh", 232.0, 590.0, 80.0),
                cell("37414.00", 392.0, 590.0, 100.0),
                cell("A2", 72.0, 560.0, 120.0),
                cell("kW", 232.0, 560.0, 80.0),
                // A2's row is missing its third cell (an empty cell in the grid).
                cell("Totals", 72.0, 530.0, 240.0),
                Block {
                    text: "Notes follow the table and are long enough to be the body text size of the page here.".into(),
                    rect: [72.0, 460.0, 500.0, 471.0],
                    size: 11.0,
                    bold: false,
                    italic: false,
                    rtl: false,
                    font: None,
                },
            ],
            images: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn grid_text_becomes_a_real_table_in_every_format() {
        let p = table_page();
        let h = html(std::slice::from_ref(&p), "Bill");
        assert!(h.contains("<table>"), "{h}");
        assert!(h.contains("<td colspan=\"2\">Totals</td>"), "spanning cell: {h}");
        assert_eq!(h.matches("<tr>").count(), 4, "{h}");
        assert!(h.contains("<td></td>"), "the hole is an empty cell: {h}");
        assert!(h.contains("<p>Notes follow"), "the paragraph after the table stays: {h}");

        let d = docx(std::slice::from_ref(&p), "Bill");
        assert!(d.starts_with(b"PK"));
        assert!(d.windows(b"word/document.xml".len()).any(|w| w == b"word/document.xml"));
        let (tables, _) = tables(&p.blocks);
        assert_eq!(tables.len(), 1);
        let table_xml = docx_table(&tables[0], 0);
        assert!(table_xml.contains("<w:tblGrid>"));
        assert!(table_xml.contains("<w:gridSpan w:val=\"2\"/>"), "{table_xml}");
        assert_eq!(table_xml.matches("<w:tr>").count(), 4);

        let r = rtf(&[p]);
        assert!(r.contains("\\trowd"), "{r}");
        assert_eq!(r.matches("\\row").count(), 4);
        // Boundaries precede the cells; "Totals" spans two columns, so it ends at the third edge.
        assert!(r.contains("\\trowd\\trgaph108\\cellx4640\\cellx7840\\cellx9920{"), "{r}");
        assert!(r.contains("\\trowd\\trgaph108\\cellx7840{\\intbl\\fs22 Totals}\\cell\\row"), "{r}");
    }

    #[test]
    fn a_staircase_of_blocks_is_not_a_huge_table() {
        // Row k holds blocks in columns k and k + 1: every column has two members and every row
        // two columns, so without a cap this would be one table of (n / 2)² cells.
        let n = 4000;
        let blocks: Vec<Block> = (0..n)
            .map(|i| {
                let (row, col) = (i / 2, i / 2 + i % 2);
                let (x, y) = (10.0 + col as f64 * 20.0, 10_000.0 - row as f64 * 14.0);
                Block { text: "x".into(), rect: [x, y, x + 8.0, y + 12.0], size: 11.0, bold: false, italic: false, rtl: false, font: None }
            })
            .collect();
        let started = std::time::Instant::now();
        let (tables, _) = tables(&blocks);
        assert!(tables.iter().all(|t| t.cols.len() <= MAX_COLS), "{} columns", tables.iter().map(|t| t.cols.len()).max().unwrap_or(0));
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[test]
    fn two_column_body_text_is_not_mistaken_for_a_table() {
        let para = |t: &str, x: f64, y: f64| Block {
            text: t.into(),
            rect: [x, y, x + 200.0, y + 90.0], // nine lines tall: body text, not a cell
            size: 11.0,
            bold: false,
            italic: false,
            rtl: false,
            font: None,
        };
        let p = Page {
            width: 612.0,
            height: 792.0,
            blocks: vec![
                para("Left column text of the page, long enough to be a flowing paragraph.", 72.0, 500.0),
                para("Right column text of the page, long enough to be a flowing paragraph.", 320.0, 500.0),
                para("Second left block, still a tall flowing paragraph rather than a cell.", 72.0, 400.0),
                para("Second right block, still a tall flowing paragraph rather than a cell.", 320.0, 400.0),
            ],
            images: Vec::new(),
            ..Default::default()
        };
        let h = html(&[p], "Paper");
        assert!(!h.contains("<table>"), "tall two-column text must stay paragraphs: {h}");
    }
}
