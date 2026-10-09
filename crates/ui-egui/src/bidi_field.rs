//! Text fields that hold right-to-left script (Arabic, Hebrew) mixed with Latin and numbers.
//!
//! egui's `TextEdit` keeps the text in typing (logical) order and maps character `i` to glyph `i`
//! of its galley. It shapes each font run in the script's direction, so an Arabic word joins and
//! reads right to left, but it neither reorders the runs of a line (UAX #9) nor knows that glyph
//! order and character order differ inside a right-to-left run: the caret, a click, a selection
//! and the arrow keys land on the wrong letters.
//!
//! [`show`] keeps `TextEdit` for everything it does well (typing, IME, clipboard, undo, scrolling)
//! and gives it a galley drawn in display order: every line is laid out by the Unicode bidi
//! algorithm with the paragraph's own direction (its first strong letter, rules P2–P3), and every
//! glyph is mapped back to the character it shows. With that map the field draws its own caret
//! and selection, turns clicks and drags into character positions, and moves the caret with the
//! arrow keys in the direction they point. Text without right-to-left letters is laid out exactly
//! as `TextEdit` would, and nothing else changes.

use std::sync::Arc;

use egui::text::{CCursor, CCursorRange, LayoutJob};
use egui::{Color32, FontId, Galley, Pos2, Rect, Ui, pos2, vec2};
use unicode_bidi::{BidiClass, Level, ParagraphBidiInfo};

/// Longest text (bytes) laid out by the bidi path; longer text is laid out as `TextEdit` would.
const MAX_BIDI_BYTES: usize = 32 * 1024;

/// One character as drawn: its row, its horizontal extent in the row, and whether it reads right
/// to left (odd bidi level).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharBox {
    pub row: usize,
    pub x0: f32,
    pub x1: f32,
    pub rtl: bool,
    /// A combining mark: drawn with the letter before it, never a caret stop of its own.
    pub mark: bool,
}

/// A row of the field: its first character (index into the whole text), the number of characters
/// it shows, its vertical extent, and its paragraph's direction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowSpan {
    pub start: usize,
    pub len: usize,
    pub y0: f32,
    pub y1: f32,
    pub rtl: bool,
    /// Where the row's text starts and ends on screen (galley coordinates).
    pub left: f32,
    pub right: f32,
}

/// Where each character of a field's text is drawn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BidiMap {
    pub chars: Vec<CharBox>,
    pub rows: Vec<RowSpan>,
}

impl BidiMap {
    fn row_of(&self, index: usize) -> Option<usize> {
        // The row holding the caret position `index`: the last row starting at or before it.
        let mut found = None;
        for (r, row) in self.rows.iter().enumerate() {
            if row.start <= index {
                found = Some(r);
            }
        }
        found
    }

    /// The caret's x for "before character `index`" (or the end of its row), in galley
    /// coordinates, and its row.
    pub fn caret_x(&self, index: usize) -> Option<(usize, f32)> {
        let r = self.row_of(index)?;
        let row = self.rows.get(r)?;
        let end = row.start.saturating_add(row.len);
        if index < end
            && let Some(b) = self.chars.get(index)
        {
            return Some((r, if b.rtl { b.x1 } else { b.x0 }));
        }
        // After the row's last character, on that character's trailing side.
        if row.len > 0
            && let Some(b) = end.checked_sub(1).and_then(|i| self.chars.get(i))
        {
            return Some((r, if b.rtl { b.x0 } else { b.x1 }));
        }
        Some((r, if row.rtl { row.right } else { row.left }))
    }

    /// The caret position nearest to `pos` (galley coordinates): the row under it, then the
    /// character under it, on the side the point is (right half of a right-to-left letter is
    /// before it).
    pub fn hit(&self, pos: Pos2) -> usize {
        let Some(first) = self.rows.first() else { return 0 };
        let r = self.rows.iter().position(|row| pos.y < row.y1).unwrap_or_else(|| self.rows.len().saturating_sub(1));
        let row = self.rows.get(r).copied().unwrap_or(*first);
        if row.len == 0 {
            return row.start;
        }
        let mut best: Option<(f32, usize)> = None;
        for i in row.start..row.start.saturating_add(row.len) {
            let Some(b) = self.chars.get(i) else { continue };
            if b.mark {
                continue;
            }
            let mid = (b.x0 + b.x1) / 2.0;
            let caret = match (pos.x < mid, b.rtl) {
                (true, false) | (false, true) => i,
                (true, true) | (false, false) => i + 1,
            };
            let d = if pos.x < b.x0 {
                b.x0 - pos.x
            } else if pos.x > b.x1 {
                pos.x - b.x1
            } else {
                0.0
            };
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, caret));
            }
        }
        // Never between a letter and its marks.
        let mut at = best.map_or(row.start, |(_, c)| c);
        while self.chars.get(at).is_some_and(|b| b.mark) {
            at += 1;
        }
        at
    }

    /// The caret position one step to the left (`left`) or right of `index` on screen, within its
    /// row; `None` at the row's edge.
    pub fn step(&self, index: usize, left: bool) -> Option<usize> {
        let (r, x) = self.caret_x(index)?;
        let row = self.rows.get(r)?;
        let mut best: Option<(f32, usize)> = None;
        for i in row.start..=row.start.saturating_add(row.len) {
            if i == index || self.chars.get(i).is_some_and(|b| b.mark) {
                continue;
            }
            let Some((ri, xi)) = self.caret_x(i) else { continue };
            if ri != r {
                continue;
            }
            let ahead = if left { x - xi } else { xi - x };
            if ahead > 0.5 && best.is_none_or(|(d, _)| ahead < d) {
                best = Some((ahead, i));
            }
        }
        best.map(|(_, i)| i)
    }

    /// Rectangles (galley coordinates) covering the characters `range` on screen.
    pub fn selection_rects(&self, range: std::ops::Range<usize>) -> Vec<Rect> {
        let mut out: Vec<Rect> = Vec::new();
        for i in range {
            let Some(b) = self.chars.get(i) else { continue };
            if b.mark {
                continue;
            }
            let Some(row) = self.rows.get(b.row) else { continue };
            let rect = Rect::from_min_max(pos2(b.x0.min(b.x1), row.y0), pos2(b.x0.max(b.x1), row.y1));
            match out.iter_mut().find(|o| (o.min.y - rect.min.y).abs() < 0.5 && o.max.x + 0.5 >= rect.min.x && rect.max.x + 0.5 >= o.min.x) {
                Some(o) => *o = o.union(rect),
                None => out.push(rect),
            }
        }
        out
    }
}

/// Whether `text` needs the bidi path: it holds right-to-left letters.
pub fn needs_bidi(text: &str) -> bool {
    text.chars().any(is_rtl_script)
}

/// Characters of the right-to-left scripts' blocks (letters, marks, the scripts' punctuation and
/// digits), which egui shapes with a right-to-left face run.
fn is_rtl_script(c: char) -> bool {
    matches!(u32::from(c), 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFC | 0x1_0800..=0x1_0FFF | 0x1_E800..=0x1_EFFF)
}

/// A combining mark (Arabic harakat, Hebrew points...), bidi class NSM.
fn is_mark(c: char) -> bool {
    unicode_bidi::bidi_class(c) == BidiClass::NSM
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn mirrored(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        '‹' => '›',
        '›' => '‹',
        c => c,
    }
}

/// How a field's text is drawn: font, colour and line height as `TextEdit` resolves them.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldStyle {
    pub font: FontId,
    pub color: Color32,
    pub line_height: f32,
}

impl FieldStyle {
    /// The style a `TextEdit` with this `font` (or the default) uses in `ui`.
    pub fn resolve(ui: &Ui, font: Option<FontId>) -> Self {
        let font = font.unwrap_or_else(|| egui::FontSelection::default().resolve(ui.style()));
        let row_height = ui.fonts_mut(|f| f.row_height(&font));
        Self { font, color: Color32::PLACEHOLDER, line_height: row_height + ui.spacing().extra_text_line_spacing }
    }
}

/// The galley `TextEdit` lays out by default.
fn plain(ui: &Ui, text: &str, style: &FieldStyle, wrap: Option<f32>) -> Arc<Galley> {
    let mut job = match wrap {
        Some(w) => LayoutJob::simple(text.to_owned(), style.font.clone(), style.color, w),
        None => LayoutJob::simple_singleline(text.to_owned(), style.font.clone(), style.color),
    };
    job.keep_trailing_whitespace = true;
    for s in &mut job.sections {
        s.format.line_height = Some(style.line_height);
    }
    ui.fonts_mut(|f| f.layout_job(job))
}

/// Lay out `text` for a field: in display order with its map when it holds right-to-left
/// letters, otherwise exactly as `TextEdit` does (no map). `wrap` is the wrap width of a
/// multi-line field.
pub fn layout(ui: &Ui, text: &str, style: &FieldStyle, wrap: Option<f32>) -> (Arc<Galley>, Option<BidiMap>) {
    if !needs_bidi(text) || text.len() > MAX_BIDI_BYTES {
        return (plain(ui, text, style, wrap), None);
    }
    match bidi_galley(ui, text, style, wrap) {
        Some((g, m)) => (g, Some(m)),
        None => (plain(ui, text, style, wrap), None),
    }
}

/// A piece of a line handed to egui in one layout: its text and, for each of its characters in
/// that text's order, the character it stands for (index into the whole text).
struct Piece {
    text: String,
    index: Vec<usize>,
}

/// One line (row) of a paragraph: its characters (indices into the whole text) and their levels.
struct Line {
    chars: Vec<usize>,
    levels: Vec<Level>,
    rtl: bool,
    ends_paragraph_with_newline: bool,
}

/// The characters of a line in display order, as pieces egui can lay out: a run of right-to-left
/// letters is given in typing order (egui's shaper turns it round), everything else in display
/// order, with brackets mirrored in right-to-left runs.
fn pieces(text_chars: &[char], line: &Line) -> Vec<Piece> {
    let n = line.chars.len();
    // L2: display order of the line's positions.
    let mut order: Vec<usize> = (0..n).collect();
    let max = line.levels.iter().map(|l| l.number()).max().unwrap_or(0);
    let min_odd = line.levels.iter().map(|l| l.number()).filter(|l| l % 2 == 1).min().unwrap_or(max.saturating_add(1));
    let mut lv = max;
    while lv >= min_odd && lv > 0 {
        let mut i = 0;
        while i < n {
            if order.get(i).and_then(|k| line.levels.get(*k)).is_some_and(|l| l.number() >= lv) {
                let mut j = i;
                while j < n && order.get(j).and_then(|k| line.levels.get(*k)).is_some_and(|l| l.number() >= lv) {
                    j += 1;
                }
                if let Some(run) = order.get_mut(i..j) {
                    run.reverse();
                }
                i = j;
            } else {
                i += 1;
            }
        }
        lv -= 1;
    }
    let mut out: Vec<Piece> = Vec::new();
    // (kind, last position): kind 0 = left-to-right text, 1 = right-to-left letters.
    let mut kind: Option<(u8, usize)> = None;
    for &k in &order {
        let Some(&ci) = line.chars.get(k) else { continue };
        let Some(&c) = text_chars.get(ci) else { continue };
        let odd = line.levels.get(k).is_some_and(|l| l.is_rtl());
        if is_bidi_control(c) {
            // Not drawn: a zero-width piece keeps the map complete.
            out.push(Piece { text: String::new(), index: vec![ci] });
            kind = None;
            continue;
        }
        let script = is_rtl_script(c);
        if odd && script {
            // Right-to-left letters: consecutive in typing order backwards on screen.
            match (kind, out.last_mut()) {
                (Some((1, last)), Some(p)) if last == k + 1 => {
                    p.text.insert(0, c);
                    p.index.insert(0, ci);
                }
                _ => out.push(Piece { text: c.to_string(), index: vec![ci] }),
            }
            kind = Some((1, k));
        } else if !odd && !script {
            match (kind, out.last_mut()) {
                (Some((0, last)), Some(p)) if k == last + 1 => {
                    p.text.push(c);
                    p.index.push(ci);
                }
                _ => out.push(Piece { text: c.to_string(), index: vec![ci] }),
            }
            kind = Some((0, k));
        } else {
            // A neutral in a right-to-left run (mirrored), or a right-to-left script character
            // shown left to right (Arabic-Indic digits): alone, so the shaper can't turn it.
            let shown = if odd { mirrored(c) } else { c };
            out.push(Piece { text: shown.to_string(), index: vec![ci] });
            kind = None;
        }
    }
    out
}

/// The paragraphs of `text` broken into lines no wider than `wrap` (greedy, at spaces; a word
/// wider than the line stands alone).
fn lines(text: &str, chars: &[char], wrap: Option<f32>, measure: &mut dyn FnMut(&[char], &Line) -> f32) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start_char = 0usize;
    let paragraphs: Vec<&str> = text.split('\n').collect();
    let count = paragraphs.len();
    for (pi, para) in paragraphs.into_iter().enumerate() {
        let n = para.chars().count();
        let info = ParagraphBidiInfo::new(para, None);
        let rtl = info.paragraph_level.is_rtl();
        let byte_levels = &info.levels;
        // Per-character levels (the level of each character's first byte).
        let mut levels = Vec::with_capacity(n);
        for (b, _) in para.char_indices() {
            levels.push(byte_levels.get(b).copied().unwrap_or(info.paragraph_level));
        }
        // L1: trailing whitespace takes the paragraph level.
        for (k, c) in para.chars().enumerate().collect::<Vec<_>>().into_iter().rev() {
            if c.is_whitespace()
                || matches!(unicode_bidi::bidi_class(c), BidiClass::WS | BidiClass::FSI | BidiClass::LRI | BidiClass::RLI | BidiClass::PDI)
            {
                if let Some(l) = levels.get_mut(k) {
                    *l = info.paragraph_level;
                }
            } else {
                break;
            }
        }
        let newline = pi + 1 < count;
        let all: Vec<usize> = (start_char..start_char + n).collect();
        let whole = Line { chars: all.clone(), levels: levels.clone(), rtl, ends_paragraph_with_newline: newline };
        let fits = |m: &mut dyn FnMut(&[char], &Line) -> f32, l: &Line| wrap.is_none_or(|w| m(chars, l) <= w);
        if n == 0 || wrap.is_none() || fits(measure, &whole) {
            out.push(whole);
        } else {
            // Break after spaces: lines are runs of whole words.
            let para_chars: Vec<char> = para.chars().collect();
            let mut line_start = 0usize;
            while line_start < n {
                let mut end = line_start;
                let mut last_fit: Option<usize> = None;
                loop {
                    // Next break opportunity: after the next run of spaces.
                    let mut e = end;
                    while e < n && !para_chars.get(e).is_some_and(|c| *c == ' ') {
                        e += 1;
                    }
                    while e < n && para_chars.get(e).is_some_and(|c| *c == ' ') {
                        e += 1;
                    }
                    if e == end {
                        break;
                    }
                    let candidate = Line {
                        chars: all.get(line_start..e).map(<[usize]>::to_vec).unwrap_or_default(),
                        levels: levels.get(line_start..e).map(<[Level]>::to_vec).unwrap_or_default(),
                        rtl,
                        ends_paragraph_with_newline: false,
                    };
                    if fits(measure, &candidate) || last_fit.is_none() {
                        last_fit = Some(e);
                        end = e;
                        if e >= n || !fits(measure, &candidate) {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                let e = last_fit.unwrap_or(n).max(line_start + 1).min(n);
                let mut line_levels = levels.get(line_start..e).map(<[Level]>::to_vec).unwrap_or_default();
                // L1 per line: trailing spaces take the paragraph level.
                for (k, l) in line_levels.iter_mut().enumerate().rev() {
                    if para_chars.get(line_start + k).is_some_and(|c| c.is_whitespace()) {
                        *l = info.paragraph_level;
                    } else {
                        break;
                    }
                }
                out.push(Line {
                    chars: all.get(line_start..e).map(<[usize]>::to_vec).unwrap_or_default(),
                    levels: line_levels,
                    rtl,
                    ends_paragraph_with_newline: e >= n && newline,
                });
                line_start = e;
            }
        }
        start_char = start_char + n + 1;
    }
    out
}

/// One line laid out: each piece by egui, placed left to right; the boxes of its characters.
struct LaidLine {
    galleys: Vec<(f32, Arc<Galley>, Vec<usize>)>,
    width: f32,
    height: f32,
}

fn lay_line(ui: &Ui, chars: &[char], line: &Line, style: &FieldStyle) -> LaidLine {
    let mut x = 0.0f32;
    let mut galleys = Vec::new();
    let mut height = style.line_height;
    for p in pieces(chars, line) {
        let g = plain(ui, &p.text, style, None);
        // The piece's advance: its glyphs' extent (a row's rounded size can fall short of it).
        let w = g.rows.first().map_or(0.0, |r| r.row.glyphs.iter().map(|gl| gl.pos.x + gl.advance_width).fold(r.row.size.x, f32::max));
        height = height.max(g.rect.height());
        galleys.push((x, g, p.index));
        x += w;
    }
    LaidLine { galleys, width: x, height }
}

/// The galley in display order and the character map, or `None` if egui's layout can't be
/// matched to the text (the caller then lays the text out plainly).
fn bidi_galley(ui: &Ui, text: &str, style: &FieldStyle, wrap: Option<f32>) -> Option<(Arc<Galley>, BidiMap)> {
    let chars: Vec<char> = text.chars().collect();
    let mut measure = |cs: &[char], l: &Line| lay_line(ui, cs, l, style).width;
    let lines = lines(text, &chars, wrap, &mut measure);
    let mut boxes: Vec<Option<CharBox>> = vec![None; chars.len()];
    let mut rows = Vec::new();
    let mut spans = Vec::new();
    let mut y = 0.0f32;
    let mut num_vertices = 0usize;
    let mut num_indices = 0usize;
    let mut mesh_bounds = Rect::NOTHING;
    let mut width = 0.0f32;
    // egui's own layout of the text: the galley and a glyph are cloned from it (their types
    // have private fields), and its size is close to the reordered one's.
    let template = plain(ui, text, style, wrap);
    let base_row = template.rows.first()?.row.clone();
    let spare_glyph = *template.rows.iter().find_map(|r| r.row.glyphs.first())?;
    for (ri, line) in lines.iter().enumerate() {
        let laid = lay_line(ui, &chars, line, style);
        // A right-to-left row of a wrapped field starts at the right.
        let offset = match wrap {
            Some(w) if line.rtl && w.is_finite() => (w - laid.width).max(0.0),
            _ => 0.0,
        };
        let mut row = (*base_row).clone();
        row.glyphs.clear();
        let mut visuals = egui::epaint::text::RowVisuals::default();
        let mut by_char: Vec<(usize, egui::epaint::text::Glyph)> = Vec::new();
        for (x, g, index) in &laid.galleys {
            let Some(prow) = g.rows.first() else { continue };
            let dx = offset + x;
            let mut mesh = prow.row.visuals.mesh.clone();
            mesh.translate(vec2(dx, 0.0));
            if visuals.mesh.is_empty() || visuals.mesh.texture_id == mesh.texture_id {
                visuals.mesh.append(mesh);
            }
            // Map egui's glyphs (display order inside a turned run) to the piece's characters.
            let glyphs = &prow.row.glyphs;
            let piece: Vec<char> = index.iter().filter_map(|i| chars.get(*i).copied()).collect();
            let turned = glyphs.len() > 1 && glyphs.first().map(|g| g.chr) == piece.last().copied() && piece.first() != piece.last();
            let mut used = vec![false; index.len()];
            // egui 0.36 adds zero-width "continuation" glyphs to right-to-left clusters (more
            // glyphs than characters): glyphs with an advance are matched first, zero-width ones
            // (marks) only to characters still unmatched; extra glyphs are left out.
            for pass in [true, false] {
                for gl in glyphs.iter().filter(|g| (g.advance_width > 0.01) == pass) {
                    let free = |k: &usize| !used.get(*k).copied().unwrap_or(true);
                    let same = |k: &usize| piece.get(*k).is_some_and(|c| *c == gl.chr || mirrored(gl.chr) == *c);
                    let pick =
                        if turned { (0..index.len()).rev().find(|k| free(k) && same(k)) } else { (0..index.len()).find(|k| free(k) && same(k)) };
                    let Some(k) = pick else { continue };
                    if let Some(u) = used.get_mut(k) {
                        *u = true;
                    }
                    let Some(&ci) = index.get(k) else { continue };
                    let mut glyph = *gl;
                    glyph.pos.x += dx;
                    by_char.push((ci, glyph));
                    let rtl = line.chars.iter().position(|c| *c == ci).and_then(|p| line.levels.get(p)).is_some_and(|l| l.is_rtl());
                    if let Some(b) = boxes.get_mut(ci) {
                        *b = Some(CharBox { row: ri, x0: glyph.pos.x, x1: glyph.pos.x + glyph.advance_width, rtl, mark: false });
                    }
                }
            }
            // Characters egui drew no glyph for (direction controls, dropped marks): at the
            // piece's start, zero width.
            for (k, u) in used.iter().enumerate() {
                if !*u
                    && let Some(&ci) = index.get(k)
                    && let Some(b) = boxes.get_mut(ci)
                {
                    let rtl = line.chars.iter().position(|c| *c == ci).and_then(|p| line.levels.get(p)).is_some_and(|l| l.is_rtl());
                    *b = Some(CharBox { row: ri, x0: dx, x1: dx, rtl, mark: false });
                    let mut g = spare_glyph;
                    g.chr = chars.get(ci).copied().unwrap_or(' ');
                    g.pos.x = dx;
                    g.advance_width = 0.0;
                    by_char.push((ci, g));
                }
            }
        }
        // Row glyphs in typing order, each at the caret position before it (for `TextEdit`'s
        // own use: IME placement and scrolling to the caret).
        by_char.sort_by_key(|(ci, _)| *ci);
        for (ci, mut g) in by_char {
            if let Some(b) = boxes.get(ci).copied().flatten() {
                g.pos.x = if b.rtl { b.x1 } else { b.x0 };
            }
            row.glyphs.push(g);
        }
        let row_w = laid.width + offset;
        row.size = vec2(row_w.max(wrap.filter(|w| w.is_finite()).unwrap_or(0.0)), laid.height);
        visuals.mesh_bounds = visuals.mesh.calc_bounds();
        visuals.glyph_index_start = 0;
        visuals.glyph_vertex_range = 0..visuals.mesh.vertices.len();
        num_vertices += visuals.mesh.vertices.len();
        num_indices += visuals.mesh.indices.len();
        if visuals.mesh_bounds.is_positive() {
            mesh_bounds = mesh_bounds.union(visuals.mesh_bounds.translate(vec2(0.0, y)));
        }
        row.visuals = visuals;
        width = width.max(row.size.x);
        spans.push(RowSpan {
            start: line.chars.first().copied().unwrap_or_else(|| line_start_index(&lines, ri, &chars)),
            len: line.chars.len(),
            y0: y,
            y1: y + laid.height,
            rtl: line.rtl,
            left: offset,
            right: offset + laid.width,
        });
        rows.push(egui::epaint::text::PlacedRow { pos: pos2(0.0, y), row: Arc::new(row), ends_with_newline: line.ends_paragraph_with_newline });
        y += laid.height;
    }
    // Every character must be placed, or the map would point at nothing.
    let chars_map: Vec<CharBox> = boxes
        .into_iter()
        .enumerate()
        .map(|(i, b)| b.unwrap_or(CharBox { row: row_of_char(&spans, i), x0: 0.0, x1: 0.0, rtl: false, mark: false }))
        .collect();
    // Marks sit on the trailing edge of their letter (egui's glyphs for them are unreliable in
    // right-to-left runs), so a cluster is one caret stop wide.
    let mut chars_map = chars_map;
    for i in 0..chars_map.len() {
        if !chars.get(i).copied().is_some_and(is_mark) {
            continue;
        }
        let base = (0..i).rev().find(|j| !chars.get(*j).copied().is_some_and(is_mark)).and_then(|j| chars_map.get(j).copied());
        if let (Some(base), Some(b)) = (base, chars_map.get_mut(i))
            && base.row == b.row
        {
            let edge = if base.rtl { base.x0 } else { base.x1 };
            *b = CharBox { row: base.row, x0: edge, x1: edge, rtl: base.rtl, mark: true };
        } else if let Some(b) = chars_map.get_mut(i) {
            b.mark = true;
        }
    }
    let mut job = LayoutJob::simple(text.to_owned(), style.font.clone(), style.color, wrap.unwrap_or(f32::INFINITY));
    job.keep_trailing_whitespace = true;
    let mut galley = (*template).clone();
    galley.job = Arc::new(job);
    galley.rows = rows;
    galley.elided = false;
    galley.rect = Rect::from_min_size(Pos2::ZERO, vec2(width, y));
    galley.mesh_bounds = mesh_bounds;
    galley.num_vertices = num_vertices;
    galley.num_indices = num_indices;
    Some((Arc::new(galley), BidiMap { chars: chars_map, rows: spans }))
}

/// The first character index of an empty line (a blank paragraph).
fn line_start_index(lines: &[Line], ri: usize, chars: &[char]) -> usize {
    // Characters of the lines before it, plus one newline per paragraph end.
    let mut at = 0usize;
    for l in lines.iter().take(ri) {
        at += l.chars.len() + usize::from(l.ends_paragraph_with_newline);
    }
    at.min(chars.len())
}

fn row_of_char(spans: &[RowSpan], i: usize) -> usize {
    spans.iter().rposition(|s| s.start <= i).unwrap_or(0)
}

/// What the field did this frame.
pub struct Shown {
    pub output: egui::text_edit::TextEditOutput,
    /// Where each character was drawn (`None` for text laid out as `TextEdit` does).
    #[cfg_attr(not(test), allow(dead_code))]
    pub map: Option<BidiMap>,
}

/// Show a text field on `text` with right-to-left text drawn, clicked, selected and moved
/// through in display order: a single- or multi-line `TextEdit` with id `id` and font `font`
/// (default when `None`), further set up by `build` (hint, width, rows...). Returns `TextEdit`'s
/// output.
pub fn show(
    ui: &mut Ui,
    text: &mut String,
    id: egui::Id,
    multiline: bool,
    font: Option<FontId>,
    build: impl for<'a> FnOnce(egui::TextEdit<'a>) -> egui::TextEdit<'a>,
) -> Shown {
    let style = FieldStyle::resolve(ui, font.clone());
    let bidi = needs_bidi(text) && text.len() <= MAX_BIDI_BYTES;
    let base = if multiline { egui::TextEdit::multiline(text) } else { egui::TextEdit::singleline(text) };
    let base = match font {
        Some(f) => base.font(f),
        None => base,
    };
    let edit = build(base).id(id);
    if !bidi {
        let output = edit.show(ui);
        return Shown { output, map: None };
    }
    // Arrow keys move on screen: handled here, before `TextEdit` sees them, from last frame's map.
    let previous: Option<BidiMap> = ui.ctx().data(|d| d.get_temp::<Arc<BidiMap>>(id)).map(|m| (*m).clone());
    if ui.memory(|m| m.has_focus(id))
        && let (Some(map), Some(mut state)) = (previous.as_ref(), egui::TextEdit::load_state(ui.ctx(), id))
        && let Some(range) = state.cursor.char_range()
    {
        let mut moved = false;
        for (key, left) in [(egui::Key::ArrowLeft, true), (egui::Key::ArrowRight, false)] {
            for shift in [false, true] {
                let mods = if shift { egui::Modifiers::SHIFT } else { egui::Modifiers::NONE };
                let pressed = ui.input(|i| {
                    i.events
                        .iter()
                        .any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, modifiers, .. } if *k == key && modifiers.matches_exact(mods)))
                });
                if !pressed {
                    continue;
                }
                let from = if !shift && !range.is_empty() {
                    // Collapse a selection to its edge on that side of the screen.
                    let [a, b] = range.sorted_cursors();
                    let xa = map.caret_x(a.index.0).map(|(_, x)| x).unwrap_or(0.0);
                    let xb = map.caret_x(b.index.0).map(|(_, x)| x).unwrap_or(0.0);
                    let pick = if (xa < xb) == left { a } else { b };
                    Some(pick.index.0)
                } else {
                    map.step(range.primary.index.0, left)
                };
                if let Some(to) = from {
                    ui.input_mut(|i| i.consume_key(mods, key));
                    let primary = CCursor::new(to);
                    let new = if shift { CCursorRange::two(range.secondary, primary) } else { CCursorRange::one(primary) };
                    state.cursor.set_char_range(Some(new));
                    moved = true;
                }
            }
        }
        // Word jumps stay `TextEdit`'s (in typing order); in a right-to-left paragraph the keys
        // point the other way.
        let rtl_row = map.row_of(range.primary.index.0).and_then(|r| map.rows.get(r)).is_some_and(|r| r.rtl);
        if rtl_row {
            ui.input_mut(|i| {
                for e in &mut i.events {
                    if let egui::Event::Key { key, modifiers, .. } = e
                        && (modifiers.alt || modifiers.command || modifiers.ctrl)
                    {
                        *key = match *key {
                            egui::Key::ArrowLeft => egui::Key::ArrowRight,
                            egui::Key::ArrowRight => egui::Key::ArrowLeft,
                            k => k,
                        };
                    }
                }
            });
        }
        if moved {
            state.store(ui.ctx(), id);
        }
    }
    // Selection and caret are drawn here (`TextEdit`'s own are hidden): the selection inside the
    // galley, behind the letters, the caret over them.
    let selection_fill = ui.visuals().selection.bg_fill;
    let mut map_out: Option<BidiMap> = None;
    let output = {
        let wrap_style = style.clone();
        let cache_id = id.with("bidi-layout");
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            // `TextEdit` lays its text out every frame: reuse the last layout while nothing it
            // depends on changed.
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                buf.as_str().hash(&mut h);
                multiline.then_some(wrap_width).map(f32::to_bits).hash(&mut h);
                wrap_style.font.hash(&mut h);
                wrap_style.line_height.to_bits().hash(&mut h);
                ui.ctx().pixels_per_point().to_bits().hash(&mut h);
                h.finish()
            };
            let cached = ui.ctx().data(|d| d.get_temp::<(u64, Arc<Galley>, Option<Arc<BidiMap>>)>(cache_id)).filter(|c| c.0 == key);
            let (mut g, m) = match cached {
                Some((_, g, m)) => (g, m.map(|m| (*m).clone())),
                None => {
                    let (g, m) = layout(ui, buf.as_str(), &wrap_style, multiline.then_some(wrap_width));
                    ui.ctx().data_mut(|d| d.insert_temp(cache_id, (key, g.clone(), m.clone().map(Arc::new))));
                    (g, m)
                }
            };
            if let Some(m) = &m
                && ui.memory(|mem| mem.has_focus(id))
                && let Some(range) = egui::TextEdit::load_state(ui.ctx(), id).and_then(|s| s.cursor.char_range())
            {
                let [a, b] = range.sorted_cursors();
                paint_selection(&mut g, &m.selection_rects(a.index.0..b.index.0), selection_fill);
            }
            map_out = m;
            g
        };
        ui.scope(|ui| {
            let v = ui.visuals_mut();
            v.selection.bg_fill = Color32::TRANSPARENT;
            v.selection.stroke.color = Color32::PLACEHOLDER;
            v.text_cursor.stroke.color = Color32::TRANSPARENT;
            edit.layouter(&mut layouter).show(ui)
        })
        .inner
    };
    let Some(map) = map_out else {
        ui.ctx().data_mut(|d| d.remove::<Arc<BidiMap>>(id));
        return Shown { output, map: None };
    };
    let mut output = output;
    let origin = output.galley_pos;
    let response = &output.response.response;
    // Clicks and drags: the character under the pointer from the map, not from glyph order.
    if let Some(p) = response.interact_pointer_pos() {
        let at = map.hit(Pos2::ZERO + (p - origin));
        let mut state = output.state.clone();
        let anchor_id = id.with("bidi-anchor");
        let new = if response.double_clicked() {
            let (a, b) = word_at(output.galley.text(), at);
            Some(CCursorRange::two(CCursor::new(a), CCursor::new(b)))
        } else if response.triple_clicked() {
            None
        } else if ui.input(|i| i.pointer.primary_pressed()) {
            let anchor = if ui.input(|i| i.modifiers.shift) { state.cursor.char_range().map_or(at, |r| r.secondary.index.0) } else { at };
            ui.ctx().data_mut(|d| d.insert_temp(anchor_id, anchor));
            Some(CCursorRange::two(CCursor::new(anchor), CCursor::new(at)))
        } else if response.dragged() {
            let anchor = ui.ctx().data(|d| d.get_temp::<usize>(anchor_id)).unwrap_or(at);
            Some(CCursorRange::two(CCursor::new(anchor), CCursor::new(at)))
        } else {
            None
        };
        if let Some(range) = new {
            state.cursor.set_char_range(Some(range));
            output.cursor_range = Some(range);
            state.clone().store(ui.ctx(), id);
            output.state = state;
            ui.ctx().request_repaint();
        }
    }
    let visuals = ui.visuals().clone();
    let painter = ui.painter().with_clip_rect(output.text_clip_rect);
    let focused = response.has_focus();
    let cursor = output.state.cursor.char_range().or(output.cursor_range);
    if let Some(range) = cursor
        && focused
        && let Some((r, x)) = map.caret_x(range.primary.index.0)
        && let Some(row) = map.rows.get(r)
    {
        let blink_on = !visuals.text_cursor.blink || {
            let t = ui.input(|i| i.time) as f32;
            let period = visuals.text_cursor.on_duration + visuals.text_cursor.off_duration;
            period <= 0.0 || (t % period) < visuals.text_cursor.on_duration
        };
        if blink_on {
            let top = origin + vec2(x, row.y0);
            let bottom = origin + vec2(x, row.y1);
            painter.line_segment([top, bottom], visuals.text_cursor.stroke);
        }
        if visuals.text_cursor.blink {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
    ui.ctx().data_mut(|d| d.insert_temp(id, Arc::new(map.clone())));
    Shown { output, map: Some(map) }
}

/// [`show`] for a field whose caller only needs its response.
pub fn field(
    ui: &mut Ui,
    text: &mut String,
    id: egui::Id,
    multiline: bool,
    font: Option<FontId>,
    build: impl for<'a> FnOnce(egui::TextEdit<'a>) -> egui::TextEdit<'a>,
) -> egui::Response {
    show(ui, text, id, multiline, font, build).output.response.response
}

/// Add selection rectangles (galley coordinates) to `galley`'s rows, behind their letters.
fn paint_selection(galley: &mut Arc<Galley>, rects: &[Rect], fill: Color32) {
    if rects.is_empty() {
        return;
    }
    let g = Arc::make_mut(galley);
    for placed in &mut g.rows {
        let y0 = placed.pos.y;
        let y1 = y0 + placed.row.size.y;
        let mine: Vec<Rect> =
            rects.iter().filter(|r| r.center().y >= y0 && r.center().y < y1).map(|r| r.translate(vec2(-placed.pos.x, -y0))).collect();
        if mine.is_empty() {
            continue;
        }
        let row = Arc::make_mut(&mut placed.row);
        let mesh = &mut row.visuals.mesh;
        let before = mesh.indices.len();
        for r in &mine {
            mesh.add_colored_rect(*r, fill);
        }
        // Draw them first: move the new triangles to the front.
        let added: Vec<u32> = mesh.indices.get(before..).map(<[u32]>::to_vec).unwrap_or_default();
        mesh.indices.truncate(before);
        mesh.indices.splice(0..0, added);
        row.visuals.mesh_bounds = mesh.calc_bounds();
    }
}

/// The word around caret position `at`: letters, marks and digits, in typing order.
fn word_at(text: &str, at: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let word = |c: &char| c.is_alphanumeric() || unicode_bidi::bidi_class(*c) == BidiClass::NSM;
    let mut a = at.min(chars.len());
    while a > 0 && chars.get(a - 1).is_some_and(word) {
        a -= 1;
    }
    let mut b = at.min(chars.len());
    while chars.get(b).is_some_and(word) {
        b += 1;
    }
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map for a one-row line from per-character extents.
    fn map(boxes: &[(f32, f32, bool)], rtl: bool) -> BidiMap {
        BidiMap {
            chars: boxes.iter().map(|&(x0, x1, r)| CharBox { row: 0, x0, x1, rtl: r, mark: false }).collect(),
            rows: vec![RowSpan {
                start: 0,
                len: boxes.len(),
                y0: 0.0,
                y1: 10.0,
                rtl,
                left: 0.0,
                right: boxes.iter().map(|b| b.1).fold(0.0, f32::max),
            }],
        }
    }

    #[test]
    fn caret_sits_on_the_reading_side_of_each_letter() {
        // "سلام" drawn right to left: س at the right (30–40), م at the left (0–10).
        let m = map(&[(30.0, 40.0, true), (20.0, 30.0, true), (10.0, 20.0, true), (0.0, 10.0, true)], true);
        assert_eq!(m.caret_x(0), Some((0, 40.0)), "before the first letter: its right edge");
        assert_eq!(m.caret_x(2), Some((0, 20.0)));
        assert_eq!(m.caret_x(4), Some((0, 0.0)), "after the last letter: the left end");
        // Left arrow moves on screen: forward in typing order.
        assert_eq!(m.step(0, true), Some(1));
        assert_eq!(m.step(4, true), None, "at the left edge");
        assert_eq!(m.step(4, false), Some(3));
        // A click on the right half of ل (20–30) is before it.
        assert_eq!(m.hit(pos2(27.0, 5.0)), 1);
        assert_eq!(m.hit(pos2(22.0, 5.0)), 2);
        assert_eq!(m.hit(pos2(-5.0, 5.0)), 4);
    }

    #[test]
    fn mixed_line_moves_and_selects_on_screen() {
        // "ab سل" in a left-to-right paragraph: a b space at 0–30, then س (40–50) ل (30–40).
        let m = map(&[(0.0, 10.0, false), (10.0, 20.0, false), (20.0, 30.0, false), (40.0, 50.0, true), (30.0, 40.0, true)], false);
        assert_eq!(m.caret_x(3), Some((0, 50.0)), "before س: its right edge");
        assert_eq!(m.caret_x(5), Some((0, 30.0)), "after ل: its left edge");
        // Selecting س alone covers 40–50 only.
        assert_eq!(m.selection_rects(3..4), vec![Rect::from_min_max(pos2(40.0, 0.0), pos2(50.0, 10.0))]);
        // Selecting "b س" is one span on screen (10–30 and 40–50 are not adjacent).
        assert_eq!(m.selection_rects(1..4).len(), 2);
        // Right arrow from after the space (x 30, before س is x 50): 30→40 is "after س"(4).
        assert_eq!(m.step(3, false), None, "x 50 is the right end");
        assert_eq!(m.step(3, true), Some(4));
    }

    #[test]
    fn words_include_marks() {
        assert_eq!(word_at("قال بِسْمِ هنا", 6), (4, 10));
        assert_eq!(word_at("abc", 0), (0, 3));
        assert_eq!(word_at("", 3), (0, 0));
    }

    #[test]
    fn display_pieces_follow_uax9() {
        // Independent expectation: unicode-bidi's own line reordering of each line.
        for text in ["سلام", "رقم 123 ABC", "The word سلام means", "(تجربة) العدد ١٢٣", "a\u{202B}ب\u{202C}c"] {
            let chars: Vec<char> = text.chars().collect();
            let mut measure = |_: &[char], _: &Line| 0.0;
            let ls = lines(text, &chars, None, &mut measure);
            assert_eq!(ls.len(), 1);
            let ps = pieces(&chars, &ls[0]);
            // What egui will draw: right-to-left letter pieces turned round by the shaper.
            let drawn: String = ps
                .iter()
                .map(|p| {
                    if p.text.chars().count() > 1 && p.text.chars().all(is_rtl_script) {
                        p.text.chars().rev().collect::<String>()
                    } else {
                        p.text.clone()
                    }
                })
                .collect();
            let info = ParagraphBidiInfo::new(text, None);
            let expected: String = info.reorder_line(0..text.len()).chars().filter(|c| !is_bidi_control(*c)).collect();
            // unicode-bidi doesn't mirror; compare with brackets mirrored in right-to-left runs.
            let mirror_free = |s: &str| s.chars().map(|c| if "()[]{}<>«»".contains(c) { '|' } else { c }).collect::<String>();
            assert_eq!(mirror_free(&drawn), mirror_free(&expected), "{text}");
            // Every character is in exactly one piece.
            let mut all: Vec<usize> = ps.iter().flat_map(|p| p.index.iter().copied()).collect();
            all.sort_unstable();
            assert_eq!(all, (0..chars.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn wrapping_breaks_at_spaces_in_typing_order() {
        let text = "واحد اثنان ثلاثة أربعة";
        let chars: Vec<char> = text.chars().collect();
        // Each character 1 unit wide: lines of at most 12 units.
        let mut measure = |_: &[char], l: &Line| l.chars.len() as f32;
        let ls = lines(text, &chars, Some(12.0), &mut measure);
        let texts: Vec<String> = ls.iter().map(|l| l.chars.iter().map(|i| chars[*i]).collect()).collect();
        assert_eq!(texts.concat(), text);
        assert!(ls.len() >= 2 && ls.iter().all(|l| l.rtl));
        assert!(texts.iter().all(|t| t.chars().count() <= 12), "{texts:?}");
    }

    /// A field in a real egui context with the interface fonts, its text, id and last map.
    struct Field {
        text: String,
        map: Option<BidiMap>,
        origin: Pos2,
    }

    fn harness(text: &str, multiline: bool) -> egui_kittest::Harness<'static, Field> {
        let mut h = egui_kittest::Harness::builder().with_size(vec2(400.0, 120.0)).build_ui_state(
            move |ui, f: &mut Field| {
                let shown = show(ui, &mut f.text, egui::Id::new("bidi-test"), multiline, None, |e| e.desired_width(360.0));
                f.map = shown.map;
                f.origin = shown.output.galley_pos;
            },
            Field { text: text.to_owned(), map: None, origin: Pos2::ZERO },
        );
        crate::theme::install_fonts(&h.ctx);
        h.run_steps(2);
        h
    }

    fn arabic_face() -> bool {
        !pdfcraft_fonts::CRAFT_FONTS.is_empty() && crate::theme::font_definitions().font_data.keys().any(|k| k.to_ascii_lowercase().contains("arab"))
    }

    fn focus_at(h: &mut egui_kittest::Harness<'static, Field>, index: usize) {
        let id = egui::Id::new("bidi-test");
        h.ctx.memory_mut(|m| m.request_focus(id));
        let mut st = egui::TextEdit::load_state(&h.ctx, id).unwrap_or_default();
        st.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(index))));
        st.store(&h.ctx, id);
        h.run_steps(2);
    }

    fn cursor(h: &egui_kittest::Harness<'static, Field>) -> (usize, usize) {
        let r = egui::TextEdit::load_state(&h.ctx, egui::Id::new("bidi-test")).and_then(|s| s.cursor.char_range()).unwrap();
        (r.secondary.index.0, r.primary.index.0)
    }

    #[test]
    fn characters_are_drawn_where_unicode_bidi_puts_them() {
        if !arabic_face() {
            eprintln!("skipping: built without the craft-fonts Arabic face (set CRAFT_FONTS_DIR)");
            return;
        }
        for text in ["سلام abc", "abc سلام def", "رقم 123 ABC", "(تجربة) العدد ١٢٣"] {
            let h = harness(text, false);
            let map = h.state().map.clone().expect("a right-to-left text has a map");
            // Independent expectation: unicode-bidi's visual order of the characters.
            let info = unicode_bidi::BidiInfo::new(text, None);
            let para = &info.paragraphs[0];
            let line = para.range.clone();
            let levels = info.reordered_levels_per_char(para, line);
            let visual = unicode_bidi::BidiInfo::reorder_visual(&levels);
            let mut by_x: Vec<usize> = (0..map.chars.len()).collect();
            by_x.sort_by(|a, b| map.chars[*a].x0.total_cmp(&map.chars[*b].x0).then(map.chars[*a].x1.total_cmp(&map.chars[*b].x1)));
            assert_eq!(by_x, visual, "{text}");
            // Each character is drawn to the right of the one before it on screen (boxes may
            // touch or overlap by a pixel: egui snaps glyphs to pixels and kerns).
            let mid = |i: usize| (map.chars[i].x0 + map.chars[i].x1) / 2.0;
            for w in by_x.windows(2) {
                assert!(mid(w[0]) < mid(w[1]), "{text}: {:?} {:?}", map.chars[w[0]], map.chars[w[1]]);
            }
        }
    }

    #[test]
    fn arrow_keys_click_and_drag_follow_the_screen() {
        if !arabic_face() {
            return;
        }
        // "سلام abc" reads right to left: on screen "abc مالس", the caret at the end of the
        // typed text sits right after "c".
        let mut h = harness("سلام abc", false);
        focus_at(&mut h, 8);
        h.key_press(egui::Key::ArrowLeft);
        h.run_steps(2);
        assert_eq!(cursor(&h), (7, 7), "left of c is before it");
        focus_at(&mut h, 0);
        h.key_press(egui::Key::ArrowLeft);
        h.run_steps(2);
        assert_eq!(cursor(&h), (1, 1), "left of س (the rightmost letter) is after it in typing order");
        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::ArrowLeft);
        h.run_steps(2);
        assert_eq!(cursor(&h), (1, 2), "shift extends the selection");
        // A click on the right half of س puts the caret before it.
        let map = h.state().map.clone().unwrap();
        let s = map.chars[0];
        let at = h.state().origin + vec2(s.x1 - 1.0, 5.0);
        h.event(egui::Event::PointerMoved(at));
        h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE });
        h.run_steps(1);
        // Drag to the left half of ا (index 2): the selection covers س and ل.
        let a = map.chars[2];
        let to = h.state().origin + vec2(a.x0 + 1.0, 5.0);
        h.event(egui::Event::PointerMoved(to));
        h.run_steps(1);
        h.event(egui::Event::PointerButton { pos: to, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
        h.run_steps(2);
        assert_eq!(cursor(&h), (0, 3), "from before س to after ا");
        // Typing replaces the selection in typing order.
        h.event(egui::Event::Text("X".into()));
        h.run_steps(2);
        assert_eq!(h.state().text, "Xم abc");
    }

    #[test]
    fn letters_keep_their_marks_and_words_read_right_to_left() {
        if !arabic_face() {
            return;
        }
        let text = "بِسْمِ لا إله";
        let mut h = harness(text, false);
        let map = h.state().map.clone().unwrap();
        let chars: Vec<char> = text.chars().collect();
        // Letters (not marks) in unicode-bidi's visual order from left to right.
        let info = unicode_bidi::BidiInfo::new(text, None);
        let para = &info.paragraphs[0];
        let visual = unicode_bidi::BidiInfo::reorder_visual(&info.reordered_levels_per_char(para, para.range.clone()));
        let letters: Vec<usize> = visual.into_iter().filter(|i| !is_mark(chars[*i])).collect();
        let mut by_x = letters.clone();
        by_x.sort_by(|a, b| ((map.chars[*a].x0 + map.chars[*a].x1) / 2.0).total_cmp(&((map.chars[*b].x0 + map.chars[*b].x1) / 2.0)));
        assert_eq!(by_x, letters);
        // Each mark is on its letter's trailing (left) edge, and the caret steps over it.
        assert!(map.chars[1].mark && map.chars[1].x0 == map.chars[0].x0);
        focus_at(&mut h, 0);
        h.key_press(egui::Key::ArrowLeft);
        h.run_steps(2);
        assert_eq!(cursor(&h), (2, 2), "past ب and its kasra");
    }

    #[test]
    #[ignore = "writes a screenshot for manual review"]
    fn screenshot_for_review() {
        let out = std::env::var("BIDI_SHOT").unwrap_or_default();
        let mut h = egui_kittest::Harness::builder().with_size(vec2(520.0, 200.0)).with_pixels_per_point(3.0).build_ui_state(
            |ui, t: &mut [String; 3]| {
                ui.label("Stock TextEdit:");
                ui.add(egui::TextEdit::singleline(&mut t[0]).desired_width(480.0));
                ui.label("Bidi field:");
                show(ui, &mut t[1], egui::Id::new("bidi-test"), false, None, |e| e.desired_width(480.0));
                show(ui, &mut t[2], egui::Id::new("bidi-test-2"), true, None, |e| e.desired_width(480.0));
            },
            ["مرحبا بالعالم رقم 123 ABC".to_owned(), "مرحبا بالعالم رقم 123 ABC".to_owned(), "سطر عربي (تجربة)\nEnglish then عربي".to_owned()],
        );
        crate::theme::install_fonts(&h.ctx);
        h.run_steps(2);
        let id = egui::Id::new("bidi-test");
        h.ctx.memory_mut(|m| m.request_focus(id));
        let mut st = egui::TextEdit::load_state(&h.ctx, id).unwrap_or_default();
        st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(6), CCursor::new(13))));
        st.store(&h.ctx, id);
        h.run_steps(3);
        if let Ok(img) = h.render() {
            let _ = img.save(&out);
        }
    }

    #[test]
    fn hostile_text_never_panics_and_maps_every_character() {
        let long = "سلام عليكم ".repeat(4000);
        let cases = [
            "\u{202E}abc\u{202C} سلام",
            "\u{2067}سلام\u{2069} x",
            "\u{064E}\u{064E}\u{0651}",
            "سلام\n\n\nabc\n",
            "\n",
            " ",
            "a\u{2029}ب\u{85}c\rد",
            "(سلام [abc] {١٢})",
            "ﻻ ﷲ ﺑﺴﻢ",
            "שלום עולם 123",
            long.as_str(),
        ];
        let mut h = egui_kittest::Harness::builder().with_size(vec2(300.0, 100.0)).build_ui(move |ui| {
            {
                let style = FieldStyle::resolve(ui, None);
                for text in cases {
                    for wrap in [None, Some(1.0), Some(40.0), Some(f32::INFINITY)] {
                        let (g, map) = layout(ui, text, &style, wrap);
                        let n = text.chars().count();
                        let Some(map) = map else {
                            assert!(!needs_bidi(text) || text.len() > MAX_BIDI_BYTES, "{text:?}");
                            continue;
                        };
                        assert_eq!(map.chars.len(), n, "{text:?}");
                        // The galley holds every character once, in typing order, row by row.
                        let in_rows: usize = g.rows.iter().map(|r| r.row.glyphs.len() + usize::from(r.ends_with_newline)).sum();
                        assert_eq!(in_rows, n, "{text:?} wrap {wrap:?}");
                        for i in 0..=n {
                            assert!(map.caret_x(i).is_some(), "{text:?} {i}");
                            let _ = map.step(i, true);
                            let _ = map.step(i, false);
                        }
                        for (x, y) in [(-100.0, -100.0), (0.0, 0.0), (5.0, 5.0), (1e6, 1e6), (f32::NAN, 3.0)] {
                            assert!(map.hit(pos2(x, y)) <= n);
                        }
                        let _ = map.selection_rects(0..n + 5);
                    }
                }
            }
        });
        crate::theme::install_fonts(&h.ctx);
        h.run_steps(1);
    }

    #[test]
    fn long_wrapped_text_lays_out_in_reasonable_time() {
        if !arabic_face() {
            return;
        }
        let unit = "هذه جملة عربية طويلة مع English words و 123 ";
        let text = unit.repeat(MAX_BIDI_BYTES / unit.len());
        assert!(text.len() <= MAX_BIDI_BYTES);
        let started = std::time::Instant::now();
        let h = harness(&text, true);
        let map = h.state().map.clone().expect("laid out by the bidi path");
        assert!(map.rows.len() > 10);
        eprintln!("{} bytes, {} rows: {:?}", text.len(), map.rows.len(), started.elapsed());
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn latin_fields_are_untouched() {
        let h = harness("plain text", false);
        assert!(h.state().map.is_none());
    }

    #[test]
    fn multiline_paragraphs_take_their_own_direction() {
        if !arabic_face() {
            return;
        }
        let h = harness("سطر عربي\nEnglish line", true);
        let map = h.state().map.clone().unwrap();
        assert_eq!(map.rows.len(), 2);
        assert!(map.rows[0].rtl && !map.rows[1].rtl);
        // The Arabic line starts on the right, the English one on the left.
        assert!(map.chars[0].x0 > map.chars[3].x0);
        assert!(map.chars[9].x0 < map.chars[10].x0);
    }
}
