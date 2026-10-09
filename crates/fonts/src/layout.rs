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
