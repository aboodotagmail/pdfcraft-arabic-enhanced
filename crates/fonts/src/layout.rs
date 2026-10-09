//! Paragraph layout for shaped text: Unicode bidi (UAX #9) per paragraph, line breaking at
//! spaces, and visual runs per line, each shaped in its own direction.
//!
//! The bidi levels are resolved once for the whole paragraph (not per line), so an embedded
//! left-to-right phrase or number keeps its place when the paragraph wraps. Lines break after
//! spaces; a word wider than the box breaks between clusters. Everything is in font units of
//! the face; the PDF writer scales by the font size.

use unicode_bidi::{Level, ParagraphBidiInfo};

use crate::shaping::{Cluster, ShapeError, ShapingFace};

/// The most text one layout accepts, in bytes (a page of text is far less).
pub const MAX_LAYOUT_BYTES: usize = 256 * 1024;

/// The paragraph direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseDirection {
    /// From the paragraph's first strong character (UAX #9 P2–P3); left to right if none.
    #[default]
    Auto,
    Ltr,
    Rtl,
}

/// How lines sit in the box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineAlign {
    /// Left for left-to-right paragraphs, right for right-to-left ones.
    #[default]
    Start,
    Left,
    Center,
    Right,
    /// Every line but a paragraph's last stretched to the box (at its spaces), start-aligned last.
    Justify,
}

/// One directional run of a line: clusters in drawing order.
#[derive(Clone, Debug, PartialEq)]
pub struct LaidRun {
    pub rtl: bool,
    pub clusters: Vec<Cluster>,
    /// The run's characters in logical order.
    pub text: String,
}

impl LaidRun {
    pub fn advance(&self) -> i64 {
        self.clusters.iter().map(|c| i64::from(c.advance)).sum()
    }
}

/// One line, in drawing order.
#[derive(Clone, Debug, PartialEq)]
pub struct LaidLine {
    pub runs: Vec<LaidRun>,
    /// The line's characters in logical order (trailing spaces dropped).
    pub text: String,
    /// Whether the paragraph the line belongs to is right to left.
    pub rtl: bool,
    /// Whether this is the last line of its paragraph (not stretched when justifying).
    pub last_in_paragraph: bool,
    /// Pen start, in font units from the box's left edge.
    pub x: i64,
    /// Extra space added at each space cluster when justified, in font units.
    pub word_spacing: f64,
}

impl LaidLine {
    pub fn advance(&self) -> i64 {
        self.runs.iter().map(LaidRun::advance).sum()
    }

    /// Number of space clusters (where justification stretches).
    pub fn spaces(&self) -> usize {
        self.runs.iter().flat_map(|r| r.clusters.iter()).filter(|c| c.text == " ").count()
    }
}

/// Lay `text` out in a box `width` font units wide. `\n` (and the other paragraph separators)
/// start new paragraphs; an empty paragraph is an empty line.
pub fn layout(face: &ShapingFace, text: &str, width: i64, direction: BaseDirection, align: LineAlign) -> Result<Vec<LaidLine>, ShapeError> {
    if text.len() > MAX_LAYOUT_BYTES {
        return Err(ShapeError::TooLong);
    }
    if let Some(c) = text.split(is_paragraph_separator).find_map(|p| face.first_missing(p)) {
        return Err(ShapeError::Missing(c));
    }
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split(is_paragraph_separator) {
        let lines = layout_paragraph(face, para, width, direction)?;
        out.extend(lines);
    }
    for line in &mut out {
        let free = width - line.advance();
        let effective = match align {
            LineAlign::Start | LineAlign::Justify => {
                if line.rtl {
                    LineAlign::Right
                } else {
                    LineAlign::Left
                }
            }
            a => a,
        };
        line.x = match effective {
            LineAlign::Center => free / 2,
            LineAlign::Right => free,
            _ => 0,
        };
        if align == LineAlign::Justify && !line.last_in_paragraph && free > 0 {
            let spaces = line.spaces();
            if spaces > 0 {
                line.word_spacing = free as f64 / spaces as f64;
                line.x = 0;
            }
        }
    }
    Ok(out)
}

/// Bidi paragraph separators (class B).
fn is_paragraph_separator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{1C}'..='\u{1E}' | '\u{85}' | '\u{2029}')
}

fn base_level(direction: BaseDirection) -> Option<Level> {
    match direction {
        BaseDirection::Auto => None,
        BaseDirection::Ltr => Some(Level::ltr()),
        BaseDirection::Rtl => Some(Level::rtl()),
    }
}

/// Byte ranges of `para` that may end a line: each word with the spaces after it.
fn segments(para: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (i, c) in para.char_indices() {
        let space = c == ' ' || c == '\u{3000}';
        if in_space && !space {
            out.push(start..i);
            start = i;
        }
        in_space = space;
    }
    if start < para.len() {
        out.push(start..para.len());
    }
    out
}

/// Level runs of `range` (maximal stretches of one bidi level), logical order.
fn level_runs(levels: &[Level], range: std::ops::Range<usize>) -> Vec<(std::ops::Range<usize>, bool)> {
    let mut out: Vec<(std::ops::Range<usize>, bool)> = Vec::new();
    let mut i = range.start;
    while i < range.end {
        let level = levels.get(i).copied().unwrap_or_else(Level::ltr);
        let mut j = i + 1;
        while j < range.end && levels.get(j).copied().unwrap_or_else(Level::ltr) == level {
            j += 1;
        }
        out.push((i..j, level.is_rtl()));
        i = j;
    }
    out
}

/// Width of `range` of the paragraph, shaped run by run in logical order.
fn measure(face: &ShapingFace, para: &str, levels: &[Level], range: std::ops::Range<usize>) -> Result<i64, ShapeError> {
    let mut w = 0i64;
    for (r, rtl) in level_runs(levels, range) {
        let Some(s) = para.get(r) else { continue };
        w = w.saturating_add(face.shape(s, rtl)?.iter().map(|c| i64::from(c.advance)).sum::<i64>());
    }
    Ok(w)
}

fn trim_trailing_spaces(para: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let s = para.get(range.clone()).unwrap_or_default();
    let trimmed = s.trim_end_matches([' ', '\u{3000}']);
    range.start..range.start + trimmed.len()
}

fn layout_paragraph(face: &ShapingFace, para: &str, width: i64, direction: BaseDirection) -> Result<Vec<LaidLine>, ShapeError> {
    let bidi = ParagraphBidiInfo::new(para, base_level(direction));
    let rtl = bidi.paragraph_level.is_rtl();
    let levels = &bidi.levels;
    // Line ranges, logical order.
    let mut lines: Vec<std::ops::Range<usize>> = Vec::new();
    let mut cur: Option<std::ops::Range<usize>> = None;
    let mut cur_w = 0i64;
    for seg in segments(para) {
        let visible = trim_trailing_spaces(para, seg.clone());
        let seg_w = measure(face, para, levels, seg.clone())?;
        let visible_w = measure(face, para, levels, visible.clone())?;
        match cur.take() {
            Some(line) if cur_w.saturating_add(visible_w) > width => {
                lines.push(line);
                // The new line starts with this segment (split when it alone is too wide).
                let (head, rest_w) = split_wide(face, para, levels, seg.clone(), width, &mut lines)?;
                cur = Some(head);
                cur_w = rest_w;
            }
            Some(line) => {
                cur = Some(line.start..seg.end);
                cur_w = cur_w.saturating_add(seg_w);
            }
            None => {
                let (head, rest_w) = split_wide(face, para, levels, seg.clone(), width, &mut lines)?;
                cur = Some(head);
                cur_w = rest_w;
            }
        }
    }
    if let Some(line) = cur {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(0..0);
    }
    let count = lines.len();
    let mut out = Vec::with_capacity(count);
    for (i, line) in lines.into_iter().enumerate() {
        let line = trim_trailing_spaces(para, line);
        let text = para.get(line.clone()).unwrap_or_default().to_string();
        let mut runs = Vec::new();
        if !line.is_empty() {
            let (_, visual) = bidi.visual_runs(line.clone());
            for run in visual {
                let Some(s) = para.get(run.clone()) else { continue };
                let run_rtl = levels.get(run.start).is_some_and(Level::is_rtl);
                runs.push(LaidRun { rtl: run_rtl, clusters: face.shape(s, run_rtl)?, text: s.to_string() });
            }
        }
        out.push(LaidLine { runs, text, rtl, last_in_paragraph: i + 1 == count, x: 0, word_spacing: 0.0 });
    }
    Ok(out)
}

/// When `seg` alone is wider than `width`, push whole-width pieces of it as lines and return the
/// remaining piece; otherwise return `seg`. Pieces break between characters.
fn split_wide(
    face: &ShapingFace,
    para: &str,
    levels: &[Level],
    seg: std::ops::Range<usize>,
    width: i64,
    lines: &mut Vec<std::ops::Range<usize>>,
) -> Result<(std::ops::Range<usize>, i64), ShapeError> {
    let visible = trim_trailing_spaces(para, seg.clone());
    let w = measure(face, para, levels, visible.clone())?;
    if w <= width {
        return Ok((seg.clone(), measure(face, para, levels, seg)?));
    }
    let mut start = seg.start;
    let mut piece_w = 0i64;
    let mut prev = seg.start;
    let text = para.get(seg.clone()).unwrap_or_default();
    for (off, c) in text.char_indices().skip(1) {
        let at = seg.start + off;
        // Never split before a combining mark (it belongs to the character before it).
        if is_mark(c) {
            continue;
        }
        let w = measure(face, para, levels, prev..at)?;
        if piece_w > 0 && piece_w.saturating_add(w) > width {
            lines.push(start..prev);
            start = prev;
            piece_w = 0;
        }
        piece_w = piece_w.saturating_add(w);
        prev = at;
    }
    let last = measure(face, para, levels, prev..seg.end)?;
    if piece_w > 0 && piece_w.saturating_add(last) > width {
        lines.push(start..prev);
        start = prev;
        piece_w = 0;
    }
    Ok((start..seg.end, piece_w.saturating_add(last)))
}

fn is_mark(c: char) -> bool {
    crate::shaping::is_combining_mark(c)
}

/// The writing direction of a glyph's text, for line reordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Right-to-left letters and their marks.
    R,
    /// Left-to-right letters.
    L,
    /// Digits (European and Arabic-Indic): always read left to right.
    D,
    /// Spaces and punctuation.
    N,
}

pub fn dir_class(text: &str) -> Dir {
    let digit = |c: char| c.is_ascii_digit() || matches!(c, '\u{0660}'..='\u{0669}' | '\u{06F0}'..='\u{06F9}');
    if text.chars().any(digit) && !text.chars().any(|c| crate::shaping::is_rtl_char(c) && !digit(c)) {
        Dir::D
    } else if text.chars().any(crate::shaping::is_rtl_char) {
        Dir::R
    } else if text.chars().any(char::is_alphabetic) {
        Dir::L
    } else {
        Dir::N
    }
}

/// Most number runs per line whose context (left-to-right or right-to-left) is tried both ways
/// when reading it back; beyond that, the preferred context is used.
const MAX_NUMBER_CHOICES: usize = 8;

/// Candidate logical orders of a line drawn left to right (`classes` in drawing order) for a
/// paragraph of the given direction, as positions into it, most likely first. Each candidate
/// assigns every drawn unit an embedding level as UAX #9 would have resolved it:
/// right-to-left letters 1, left-to-right letters 0 (2 in a right-to-left paragraph), each run
/// of digits either in the left-to-right context (EN after a Latin letter: level 0, or 2) or
/// the right-to-left one (AN, or EN after an Arabic letter, W2: level 2), and neutrals between
/// them by rules N1–N2 (numbers count as right to left there when in that context); then rule
/// L2 is undone exactly (the reversals applied from the lowest odd level up). [`read_line`]
/// keeps a candidate only if UAX #9 lays it out exactly as drawn. Numbers next to right-to-left
/// letters are tried in the right-to-left context first.
fn candidate_orders(classes: &[Dir], rtl_base: bool) -> Vec<Vec<usize>> {
    let n = classes.len();
    // Digit runs (maximal, separators between digits included).
    let mut runs: Vec<std::ops::Range<usize>> = Vec::new();
    let mut i = 0;
    while i < n {
        if classes.get(i) == Some(&Dir::D) {
            let mut j = i + 1;
            while j < n && (classes.get(j) == Some(&Dir::D) || (classes.get(j) == Some(&Dir::N) && classes.get(j + 1) == Some(&Dir::D) && j + 1 < n))
            {
                j += 1;
            }
            runs.push(i..j);
            i = j;
        } else {
            i += 1;
        }
    }
    // The nearest strong letter on either side of a run (through neutrals and digits).
    let near_rtl = |r: &std::ops::Range<usize>| {
        let left = (0..r.start).rev().find_map(|k| match classes.get(k) {
            Some(Dir::R) => Some(true),
            Some(Dir::L) => Some(false),
            _ => None,
        });
        let right = (r.end..n).find_map(|k| match classes.get(k) {
            Some(Dir::R) => Some(true),
            Some(Dir::L) => Some(false),
            _ => None,
        });
        left == Some(true) || right == Some(true)
    };
    let preferred: Vec<bool> = runs.iter().map(near_rtl).collect();
    let free = runs.len().min(MAX_NUMBER_CHOICES);
    let mut out = Vec::new();
    for mask in 0u32..(1u32 << free) {
        // Bit k flips run k away from its preferred context.
        let rtl_number: Vec<bool> = preferred.iter().enumerate().map(|(k, p)| if k < free && mask & (1 << k) != 0 { !p } else { *p }).collect();
        // Direction of each unit for neutral resolution, and levels of strong units.
        let base_level: u8 = u8::from(rtl_base);
        let ltr_level: u8 = if rtl_base { 2 } else { 0 };
        let mut dir: Vec<Option<bool>> = classes
            .iter()
            .map(|c| match c {
                Dir::R => Some(true),
                Dir::L => Some(false),
                _ => None,
            })
            .collect();
        let mut level: Vec<Option<u8>> = classes
            .iter()
            .map(|c| match c {
                Dir::R => Some(1),
                Dir::L => Some(ltr_level),
                _ => None,
            })
            .collect();
        for (r, rtl_ctx) in runs.iter().zip(&rtl_number) {
            for k in r.clone() {
                if let Some(d) = dir.get_mut(k) {
                    *d = Some(*rtl_ctx);
                }
                if let Some(l) = level.get_mut(k) {
                    // Numbers: level 2 except EN in a left-to-right paragraph's L context.
                    *l = Some(if *rtl_ctx || rtl_base { 2 } else { 0 });
                }
            }
        }
        // Neutrals: same direction on both sides (start and end of line count as the
        // paragraph's), that direction's level; otherwise the paragraph's level.
        let mut k = 0;
        while k < n {
            if level.get(k).is_some_and(Option::is_some) {
                k += 1;
                continue;
            }
            let mut j = k;
            while j < n && level.get(j).is_some_and(Option::is_none) {
                j += 1;
            }
            let before = if k == 0 { Some(rtl_base) } else { dir.get(k - 1).copied().flatten() };
            let after = if j >= n { Some(rtl_base) } else { dir.get(j).copied().flatten() };
            let lv = match (before, after) {
                (Some(true), Some(true)) => 1,
                (Some(false), Some(false)) => ltr_level,
                _ => base_level,
            };
            for m in k..j {
                if let Some(l) = level.get_mut(m) {
                    *l = Some(lv);
                }
            }
            k = j;
        }
        let levels: Vec<u8> = level.iter().map(|l| l.unwrap_or(base_level)).collect();
        // Undo L2: the same reversals, lowest odd level first.
        let mut order: Vec<usize> = (0..n).collect();
        let max = levels.iter().copied().max().unwrap_or(0);
        let mut lv = 1;
        while lv <= max {
            let mut a = 0;
            while a < order.len() {
                if levels.get(order[a]).is_some_and(|l| *l >= lv) {
                    let mut b = a;
                    while b < order.len() && levels.get(order[b]).is_some_and(|l| *l >= lv) {
                        b += 1;
                    }
                    if let Some(run) = order.get_mut(a..b) {
                        run.reverse();
                    }
                    a = b;
                } else {
                    a += 1;
                }
            }
            lv += 1;
        }
        if !out.contains(&order) {
            out.push(order);
        }
    }
    out
}

/// The display order UAX #9 gives a line of `units` (each one or more characters, in logical
/// order) in a paragraph of the given direction: for each display position, the unit shown
/// there. Levels come from the Unicode bidi algorithm (unicode-bidi: rules P–W–N–I and L1);
/// rule L2 reverses whole units, so a ligature's or a letter's marks stay with it.
pub fn uax9_display_order(units: &[&str], rtl_base: bool) -> Vec<usize> {
    let text: String = units.concat();
    if text.is_empty() {
        return (0..units.len()).collect();
    }
    let base = if rtl_base { Level::rtl() } else { Level::ltr() };
    let info = ParagraphBidiInfo::new(&text, Some(base));
    let levels = info.reordered_levels(0..text.len());
    let mut at = 0usize;
    let unit_levels: Vec<u8> = units
        .iter()
        .map(|u| {
            let l = levels.get(at).map_or(base.number(), |l| l.number());
            at += u.len();
            l
        })
        .collect();
    let mut order: Vec<usize> = (0..units.len()).collect();
    let max = unit_levels.iter().copied().max().unwrap_or(0);
    let min_odd = unit_levels.iter().copied().filter(|l| l % 2 == 1).min().unwrap_or(max + 1);
    // L2: from the highest level down to the lowest odd one, reverse every run at or above it.
    let mut level = max;
    while level >= min_odd && level > 0 {
        let mut i = 0;
        while i < order.len() {
            if unit_levels.get(order[i]).is_some_and(|l| *l >= level) {
                let mut j = i;
                while j < order.len() && unit_levels.get(order[j]).is_some_and(|l| *l >= level) {
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
        level -= 1;
    }
    order
}

/// The paragraph direction UAX #9 rules P2–P3 give `text` (its first strong letter); `None`
/// without strong letters.
pub fn uax9_paragraph_rtl(text: &str) -> Option<bool> {
    match unicode_bidi::get_base_direction(text) {
        unicode_bidi::Direction::Rtl => Some(true),
        unicode_bidi::Direction::Ltr => Some(false),
        unicode_bidi::Direction::Mixed => None,
    }
}

/// How a line drawn left to right reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineReading {
    /// Drawn positions in reading (logical) order.
    pub order: Vec<usize>,
    /// The paragraph direction the reading was made in; laying `order` out again in this
    /// direction draws the line exactly as it was.
    pub rtl: bool,
    /// The drawn line alone doesn't determine its reading: another logical text or another
    /// paragraph direction displays it the same way (or no reading does), so `order` and
    /// `rtl` follow the known direction, the hint or the default.
    pub ambiguous: bool,
}

/// Read a line drawn left to right: `units` are its glyphs' texts in drawing order.
///
/// A reading (a logical order and a paragraph direction) is *consistent* when the Unicode
/// bidi algorithm (UAX #9), laying that logical text out in that direction, draws exactly
/// `units`; it is *natural* when rules P2–P3 (the first strong letter) give that direction too.
/// The paragraph direction is `known` when the document says (PdfCraft's own text records it);
/// otherwise natural consistent readings are preferred to the rest. When the preferred
/// readings disagree on the direction, `hint` (where the line sits: right-aligned lines are
/// right to left) decides, and without a hint a line with right-to-left letters is read right
/// to left. More than one preferred reading marks the result ambiguous.
pub fn read_line(units: &[&str], known: Option<bool>, hint: Option<bool>) -> LineReading {
    let classes: Vec<Dir> = units.iter().map(|u| dir_class(u)).collect();
    if !classes.contains(&Dir::R) {
        return LineReading { order: (0..units.len()).collect(), rtl: known.unwrap_or(false), ambiguous: false };
    }
    // Consistent readings in direction `rtl`, natural ones first, one per distinct text.
    let readings = |rtl: bool| {
        let mut natural: Vec<(Vec<usize>, String)> = Vec::new();
        let mut other: Vec<(Vec<usize>, String)> = Vec::new();
        for order in candidate_orders(&classes, rtl) {
            let logical: Vec<&str> = order.iter().filter_map(|i| units.get(*i).copied()).collect();
            let shown = uax9_display_order(&logical, rtl);
            let consistent = shown.len() == units.len() && shown.iter().enumerate().all(|(pos, li)| logical.get(*li) == units.get(pos));
            if !consistent {
                continue;
            }
            let text = logical.concat();
            if natural.iter().chain(&other).any(|(_, t)| *t == text) {
                continue;
            }
            if uax9_paragraph_rtl(&text) == Some(rtl) {
                natural.push((order, text));
            } else {
                other.push((order, text));
            }
        }
        (natural, other)
    };
    // No consistent reading (text drawn in an order UAX #9 never produces): the first
    // candidate in the chosen direction, flagged.
    let fallback = |rtl: bool| LineReading {
        order: candidate_orders(&classes, rtl).into_iter().next().unwrap_or_else(|| (0..units.len()).collect()),
        rtl,
        ambiguous: true,
    };
    if let Some(rtl) = known {
        let (natural, other) = readings(rtl);
        let count = natural.len() + other.len();
        return match natural.into_iter().chain(other).next() {
            Some((order, _)) => LineReading { order, rtl, ambiguous: count > 1 },
            None => fallback(rtl),
        };
    }
    let (rtl_natural, rtl_other) = readings(true);
    let (ltr_natural, ltr_other) = readings(false);
    let (rtl_pool, ltr_pool) = if rtl_natural.is_empty() && ltr_natural.is_empty() { (rtl_other, ltr_other) } else { (rtl_natural, ltr_natural) };
    let count = rtl_pool.len() + ltr_pool.len();
    let rtl = match (rtl_pool.is_empty(), ltr_pool.is_empty()) {
        (false, true) => true,
        (true, false) => false,
        _ => hint.unwrap_or(true),
    };
    let pool = if rtl { rtl_pool } else { ltr_pool };
    match pool.into_iter().next() {
        Some((order, _)) => LineReading { order, rtl, ambiguous: count > 1 },
        None => fallback(rtl),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face() -> Option<&'static ShapingFace> {
        crate::shaping::tests::face_or_skip()
    }

    /// The text of a line as drawn, left to right, cluster by cluster.
    fn drawn(line: &LaidLine) -> String {
        line.runs.iter().flat_map(|r| r.clusters.iter()).map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn arabic_paragraph_is_right_to_left_and_right_aligned() {
        let Some(f) = face() else { return };
        let lines = layout(f, "مرحبا بالعالم", 100_000, BaseDirection::Auto, LineAlign::Start).unwrap();
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert!(l.rtl);
        assert_eq!(l.text, "مرحبا بالعالم");
        assert_eq!(l.x, 100_000 - l.advance(), "right aligned");
        // Drawn from the last letter of the last word.
        assert!(drawn(l).starts_with('م') && drawn(l).ends_with('م'));
        let first_word_drawn_last: String = drawn(l).chars().rev().take(5).collect();
        assert_eq!(first_word_drawn_last, "مرحبا");
    }

    #[test]
    fn numbers_and_latin_keep_left_to_right_order_inside_arabic() {
        let Some(f) = face() else { return };
        let lines = layout(f, "رقم 123 ABC", 100_000, BaseDirection::Auto, LineAlign::Start).unwrap();
        let l = &lines[0];
        assert!(l.rtl);
        let d = drawn(l);
        // Visual order, left to right: "ABC 123 مقر" — the Latin and the number read left to
        // right, and the Arabic word is on the right.
        assert!(d.starts_with("ABC"), "{d}");
        assert!(d.contains("123"), "digits stay in order: {d}");
        assert!(d.ends_with('ر'), "{d}");
        assert!(l.runs.iter().any(|r| !r.rtl && r.text.contains("123")));
        // Arabic-Indic digits are also left to right.
        let l = &layout(f, "العدد ١٢٣", 100_000, BaseDirection::Auto, LineAlign::Start).unwrap()[0];
        assert!(drawn(l).starts_with("١٢٣"), "{}", drawn(l));
    }

    #[test]
    fn latin_paragraph_with_arabic_phrase() {
        let Some(f) = face() else { return };
        let l = &layout(f, "The word سلام means peace", 100_000, BaseDirection::Auto, LineAlign::Start).unwrap()[0];
        assert!(!l.rtl);
        assert_eq!(l.x, 0);
        let d = drawn(l);
        assert!(d.starts_with("The word ") && d.contains("مالس") && d.ends_with("peace"), "{d}");
    }

    #[test]
    fn forced_direction() {
        let Some(f) = face() else { return };
        let l = &layout(f, "abc", 100_000, BaseDirection::Rtl, LineAlign::Start).unwrap()[0];
        assert!(l.rtl && l.x > 0, "a forced RTL paragraph is right aligned");
        let l = &layout(f, "سلام", 100_000, BaseDirection::Ltr, LineAlign::Start).unwrap()[0];
        assert!(!l.rtl && l.x == 0);
    }

    #[test]
    fn wrapping_keeps_logical_order_across_lines() {
        let Some(f) = face() else { return };
        let text = "واحد اثنان ثلاثة أربعة خمسة ستة سبعة ثمانية";
        let one = layout(f, text, 1_000_000, BaseDirection::Auto, LineAlign::Start).unwrap();
        let w = one[0].advance() / 3;
        let lines = layout(f, text, w, BaseDirection::Auto, LineAlign::Start).unwrap();
        assert!(lines.len() >= 3, "{}", lines.len());
        // Lines hold consecutive words in reading order.
        let joined: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(joined.join(" "), text);
        assert!(lines.iter().all(|l| l.advance() <= w && l.rtl));
        assert!(lines.last().unwrap().last_in_paragraph && !lines[0].last_in_paragraph);
    }

    #[test]
    fn long_word_breaks_between_characters_not_marks() {
        let Some(f) = face() else { return };
        let word = "بِسْمِاللّٰهِالرَّحْمٰنِالرَّحِيمِ";
        let lines = layout(f, word, 2000, BaseDirection::Auto, LineAlign::Start).unwrap();
        assert!(lines.len() > 1);
        assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<String>(), word);
        for l in &lines {
            assert!(!l.text.starts_with(is_mark), "a line starts with a mark: {:?}", l.text);
        }
    }

    #[test]
    fn justify_and_alignment() {
        let Some(f) = face() else { return };
        let text = "كلمة كلمة كلمة كلمة كلمة كلمة";
        let one = layout(f, text, 1_000_000, BaseDirection::Auto, LineAlign::Start).unwrap();
        let w = one[0].advance() * 2 / 3;
        let lines = layout(f, text, w, BaseDirection::Auto, LineAlign::Justify).unwrap();
        assert!(lines.len() >= 2);
        let first = &lines[0];
        assert!(first.word_spacing > 0.0);
        let stretched = first.advance() as f64 + first.word_spacing * first.spaces() as f64;
        assert!((stretched - w as f64).abs() < 1.0, "{stretched} vs {w}");
        let last = lines.last().unwrap();
        assert_eq!(last.word_spacing, 0.0);
        assert_eq!(last.x, w - last.advance(), "the last line is start (right) aligned");
        let c = &layout(f, "سلام", 10_000, BaseDirection::Auto, LineAlign::Center).unwrap()[0];
        assert_eq!(c.x, (10_000 - c.advance()) / 2);
        let l = &layout(f, "سلام", 10_000, BaseDirection::Auto, LineAlign::Left).unwrap()[0];
        assert_eq!(l.x, 0);
    }

    #[test]
    fn paragraphs_and_empty_lines() {
        let Some(f) = face() else { return };
        let lines = layout(f, "سطر\n\nline", 100_000, BaseDirection::Auto, LineAlign::Start).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].rtl && lines[1].runs.is_empty() && !lines[2].rtl);
        assert!(layout(f, "", 1000, BaseDirection::Auto, LineAlign::Start).unwrap().len() == 1);
    }

    #[test]
    fn errors_are_reported_not_drawn() {
        let Some(f) = face() else { return };
        assert_eq!(layout(f, "سلام 日本", 100_000, BaseDirection::Auto, LineAlign::Start), Err(ShapeError::Missing('日')));
        assert_eq!(layout(f, &"a".repeat(MAX_LAYOUT_BYTES + 1), 1000, BaseDirection::Auto, LineAlign::Start), Err(ShapeError::TooLong));
    }

    /// Units of `text` as a shaper makes them: each character, with combining marks kept on
    /// the character before them.
    fn units_of(text: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in text.chars() {
            match out.last_mut() {
                Some(u) if crate::shaping::is_combining_mark(c) => u.push(c),
                _ => out.push(c.to_string()),
            }
        }
        out
    }

    /// The drawn (visual) units of logical `text` in a paragraph of direction `rtl`, by UAX #9.
    fn uax9_drawn(text: &str, rtl: bool) -> Vec<String> {
        let units = units_of(text);
        let refs: Vec<&str> = units.iter().map(String::as_str).collect();
        uax9_display_order(&refs, rtl).iter().map(|i| units[*i].clone()).collect()
    }

    #[test]
    fn display_order_matches_unicode_bidi_reordering() {
        // Independent check of the unit-level L2 against unicode-bidi's own line reordering, with
        // one character per unit (where both must agree exactly).
        for (text, rtl) in [
            ("مرحبا بالعالم", true),
            ("رقم 123 ABC", true),
            ("The word سلام means peace.", false),
            ("(تجربة) العدد ١٢٣", true),
            ("قال: \"Hello world\" ثم ذهب", true),
            ("Room غرفة ٣ is free", false),
            ("السعر 1,234.50 ريال", true),
            ("abc", true),
        ] {
            let units: Vec<String> = text.chars().map(String::from).collect();
            let refs: Vec<&str> = units.iter().map(String::as_str).collect();
            let ours: String = uax9_display_order(&refs, rtl).iter().map(|i| units[*i].as_str()).collect();
            let info = ParagraphBidiInfo::new(text, Some(if rtl { Level::rtl() } else { Level::ltr() }));
            let theirs = info.reorder_line(0..text.len());
            assert_eq!(ours, theirs, "{text}");
        }
    }

    #[test]
    fn lines_are_read_back_in_the_order_they_were_typed() {
        // Each logical line, displayed by UAX #9 in its natural direction (P2–P3), is read back
        // from the drawn order alone: same text, same direction.
        for text in [
            "مرحبا بالعالم",
            "بِسْمِ اللهِ الرَّحْمٰنِ الرَّحِيمِ",
            "(تجربة) العدد ١٢٣",
            "قال: «مرحبا» ثم ذهب",
            "The word سلام means peace.",
            "Room غرفة ٣ is free",
            "السعر 1,234.50 ريال",
        ] {
            let rtl = uax9_paragraph_rtl(text).unwrap();
            let visual = uax9_drawn(text, rtl);
            let refs: Vec<&str> = visual.iter().map(String::as_str).collect();
            for known in [Some(rtl), None] {
                let r = read_line(&refs, known, None);
                let back: String = r.order.iter().map(|i| refs[*i]).collect();
                assert_eq!(back, text, "known {known:?}");
                assert_eq!(r.rtl, rtl, "{text}: known {known:?}");
            }
        }
    }

    /// Every logical text that UAX #9 draws as `visual` in a paragraph of direction `rtl`, found
    /// by brute force independently of `read_line`: the drawn line is cut into tokens (runs of
    /// one strong direction or of digits, and single neutrals), and every order of the tokens,
    /// each kept or reversed, is laid out again and compared with the drawing. Lines of more
    /// than seven tokens are out of its reach (`None`).
    fn oracle_readings(visual: &[String], rtl: bool) -> Option<std::collections::BTreeSet<String>> {
        let mut tokens: Vec<Vec<String>> = Vec::new();
        let mut last: Option<Dir> = None;
        for u in visual {
            let c = dir_class(u);
            match tokens.last_mut() {
                Some(t) if c != Dir::N && last == Some(c) => t.push(u.clone()),
                _ => tokens.push(vec![u.clone()]),
            }
            last = Some(c);
        }
        if tokens.len() > 7 {
            return None;
        }
        fn permute(items: &mut Vec<usize>, k: usize, out: &mut Vec<Vec<usize>>) {
            if k == items.len() {
                out.push(items.clone());
                return;
            }
            for i in k..items.len() {
                items.swap(k, i);
                permute(items, k + 1, out);
                items.swap(k, i);
            }
        }
        let mut perms = Vec::new();
        permute(&mut (0..tokens.len()).collect(), 0, &mut perms);
        let mut found = std::collections::BTreeSet::new();
        for perm in perms {
            for flips in 0u32..(1 << tokens.len()) {
                let logical: String = perm
                    .iter()
                    .map(|t| {
                        let tok = &tokens[*t];
                        if flips & (1 << t) != 0 { tok.iter().rev().cloned().collect::<String>() } else { tok.concat() }
                    })
                    .collect();
                if uax9_drawn(&logical, rtl) == visual {
                    found.insert(logical);
                }
            }
        }
        Some(found)
    }

    /// Mixed lines, each drawn by UAX #9 from a logical text in a given direction; some can't be
    /// told apart from other texts drawn the same way.
    const MIXED: [(&str, bool); 10] = [
        ("رقم 123 ABC", true),
        ("رقم ABC 123", true),
        ("ABC 123 رقم", false),
        ("ABC رقم 123", false),
        ("Total: عام 2026", false),
        ("مرحبا بالعالم", true),
        ("العدد ١٢٣", true),
        ("Room غرفة ٣", false),
        ("سلام abc", true),
        ("abc سلام", false),
    ];

    #[test]
    fn the_oracle_sees_the_known_ambiguities() {
        // Worked by hand from UAX #9: in a right-to-left paragraph "رقم 123 ABC" and
        // "رقم ABC 123" both draw "ABC 123 مقر" (the digits after ABC take its L direction by
        // W7; after رقم they are AN by W2 and stay left of the Latin); in a left-to-right one
        // "ABC 123 رقم" and "ABC رقم 123" draw it too (123 after رقم is AN, which N1 joins to
        // the Arabic run: reversed, it lands left of مقر). A plain Arabic line has one reading.
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<std::collections::BTreeSet<_>>();
        let visual: Vec<String> = units_of("ABC 123 مقر");
        assert_eq!(uax9_drawn("رقم 123 ABC", true), visual);
        assert_eq!(oracle_readings(&visual, true), Some(set(&["رقم 123 ABC", "رقم ABC 123"])));
        assert_eq!(oracle_readings(&visual, false), Some(set(&["ABC 123 رقم", "ABC رقم 123"])));
        let visual = uax9_drawn("مرحبا بالعالم", true);
        assert_eq!(oracle_readings(&visual, true), Some(set(&["مرحبا بالعالم"])));
    }

    #[test]
    fn readings_are_the_ones_uax9_allows() {
        for (text, rtl) in MIXED {
            let visual = uax9_drawn(text, rtl);
            let refs: Vec<&str> = visual.iter().map(String::as_str).collect();
            let readings = |dir| oracle_readings(&visual, dir).expect("short line");
            let read = |known, hint| {
                let r = read_line(&refs, known, hint);
                (r.order.iter().map(|i| refs[*i]).collect::<String>(), r.rtl, r.ambiguous)
            };
            // The paragraph direction is known: a reading in it, ambiguous exactly when UAX #9
            // allows several.
            let valid = readings(rtl);
            assert!(valid.contains(text), "{text}");
            let (got, got_rtl, ambiguous) = read(Some(rtl), None);
            assert!(got_rtl == rtl && valid.contains(&got), "{text}: read {got:?}, valid {valid:?}");
            assert_eq!(ambiguous, valid.len() > 1, "{text}: {valid:?}");
            if valid.len() == 1 {
                assert_eq!(got, text);
            }
            // Unknown direction: the reading is valid in the direction it reports, and a hint
            // only decides between directions UAX #9 leaves open.
            for hint in [None, Some(true), Some(false)] {
                let (got, got_rtl, ambiguous) = read(None, hint);
                assert!(readings(got_rtl).contains(&got), "{text}: {got:?} rtl {got_rtl} hint {hint:?}");
                let natural = |dir: bool| readings(dir).iter().any(|t| uax9_paragraph_rtl(t) == Some(dir));
                if natural(true) != natural(false) {
                    assert_eq!(got_rtl, natural(true), "{text}: only one direction reads naturally");
                } else if let Some(h) = hint {
                    assert_eq!(got_rtl, h, "{text}: both directions possible, the hint decides");
                    assert!(ambiguous);
                }
            }
        }
    }

    #[test]
    fn ambiguous_lines_follow_the_known_direction_or_the_hint() {
        // "رقم 123 ABC" in a right-to-left paragraph and "ABC 123 رقم" in a left-to-right one are
        // drawn identically (see the oracle test): the drawing can't tell the direction.
        let a = uax9_drawn("رقم 123 ABC", true);
        assert_eq!(a, uax9_drawn("ABC 123 رقم", false));
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        for (known, hint, rtl) in
            [(Some(true), None, true), (Some(false), None, false), (None, Some(true), true), (None, Some(false), false), (None, None, true)]
        {
            let r = read_line(&refs, known, hint);
            assert_eq!(r.rtl, rtl, "known {known:?} hint {hint:?}");
            assert!(r.ambiguous);
            // Whatever is chosen is displayed exactly as drawn.
            let logical: Vec<&str> = r.order.iter().map(|i| refs[*i]).collect();
            let shown: Vec<&str> = uax9_display_order(&logical, r.rtl).iter().map(|i| logical[*i]).collect();
            assert_eq!(shown, refs);
        }
    }

    #[test]
    fn odd_input_never_panics() {
        let Some(f) = face() else { return };
        for s in [
            "\u{202E}abc\u{202C} سلام",
            "\u{2067}سلام\u{2069} x",
            "\u{064E}\u{064E}",
            "   ",
            " سلام ",
            "\u{200F}\u{200E}",
            "a\u{2029}b\u{85}c\rd\u{1C}e",
            "(سلام [abc] {١٢})",
        ] {
            for dir in [BaseDirection::Auto, BaseDirection::Ltr, BaseDirection::Rtl] {
                for w in [1, 500, 100_000] {
                    let _ = layout(f, s, w, dir, LineAlign::Justify);
                }
            }
        }
        // Deep embeddings beyond the UAX #9 limit.
        let deep = "\u{202B}".repeat(200) + "سلام";
        assert!(layout(f, &deep, 10_000, BaseDirection::Auto, LineAlign::Start).is_ok());
        let long = "بسم الله الرحمن الرحيم ".repeat(500);
        assert!(layout(f, &long, 20_000, BaseDirection::Auto, LineAlign::Justify).is_ok());
    }
}
