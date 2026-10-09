//! Content-stream text for laid-out lines in an embedded subset font: two-byte glyph codes in
//! drawing order, one code per shaped cluster.
//!
//! Right-to-left text is drawn in visual order with an exact per-code ToUnicode map, the way
//! Chromium, Word and LibreOffice write Arabic; PDF text extractors (Acrobat, Poppler, MuPDF,
//! pdf.js, PdfCraft) restore logical order from that. `/ActualText` spans are optional
//! ([`PaintOptions::actual_text`]) and off by default: Poppler spreads an ActualText string
//! left to right over the span and then reads right-to-left text from the right, which reverses
//! the word (checked with pdftotext 24.02 against a Chromium-made reference file).

use pdfcraft_cos::{Document, ObjRef};

use crate::embed::{EmbedError, FontSubset, actual_text_bdc, hex_codes};
use crate::layout::{BaseDirection, LaidLine, LaidRun, LineAlign, layout};
use crate::shaping::{ShapeError, ShapingFace};

/// Why a block of text could not be written. Nothing has been added to the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextError {
    Shape(ShapeError),
    Embed(EmbedError),
}

impl std::fmt::Display for TextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextError::Shape(e) => e.fmt(f),
            TextError::Embed(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for TextError {}

impl From<ShapeError> for TextError {
    fn from(e: ShapeError) -> Self {
        TextError::Shape(e)
    }
}

impl From<EmbedError> for TextError {
    fn from(e: EmbedError) -> Self {
        TextError::Embed(e)
    }
}

/// Lay `text` out at `size` points in a box `width` points wide.
pub fn layout_points(
    face: &ShapingFace,
    text: &str,
    size: f64,
    width: f64,
    direction: BaseDirection,
    align: LineAlign,
) -> Result<Vec<LaidLine>, ShapeError> {
    let upem = f64::from(face.units_per_em().max(1));
    let size = if size.is_finite() && size > 0.0 { size } else { 12.0 };
    let units = if width.is_finite() { (width * upem / size).clamp(1.0, 1e9) } else { 1e9 };
    // In range: clamped above.
    layout(face, text, units as i64, direction, align)
}

/// Single lines drawn with one shared embedded subset of the Arabic face, for generated
/// appearances that place each line themselves (marks, signatures, stamps). Lay lines out and
/// paint them first; [`UnicodeLines::write`] the font once everything is known to be drawable.
#[derive(Debug)]
pub struct UnicodeLines {
    subset: FontSubset<'static>,
}

impl UnicodeLines {
    /// [`ShapeError::NoFont`] when the build has no Arabic face.
    pub fn new() -> Result<UnicodeLines, TextError> {
        let face = ShapingFace::arabic().ok_or(TextError::Shape(ShapeError::NoFont))?;
        Ok(UnicodeLines { subset: FontSubset::new(face) })
    }

    /// `text` laid out as one line (paragraph separators start more lines; only the first is
    /// returned), with its width in points at `size`.
    pub fn line(&self, text: &str, size: f64) -> Result<(LaidLine, f64), TextError> {
        self.line_in(text, size, BaseDirection::Auto)
    }

    /// [`UnicodeLines::line`] in a paragraph of the given direction (`Auto`: UAX #9 P2–P3).
    pub fn line_in(&self, text: &str, size: f64, direction: BaseDirection) -> Result<(LaidLine, f64), TextError> {
        let face = self.subset.face();
        let mut lines = layout_points(face, text, size, 1e7, direction, LineAlign::Left)?;
        let mut line = if lines.is_empty() { return Err(TextError::Shape(ShapeError::TooLong)) } else { lines.swap_remove(0) };
        line.x = 0;
        let w = line.advance() as f64 * size / f64::from(face.units_per_em().max(1));
        Ok((line, w))
    }

    /// `BT … ET` drawing `line` in the font named `font` with its left end at (`x`, `y`).
    pub fn ops(&mut self, line: &LaidLine, font: &str, x: f64, y: f64, size: f64) -> Result<String, TextError> {
        let opts = PaintOptions { font, size, left: x, baseline: y, leading: size, fake_bold: false, slant: 0.0, actual_text: false };
        Ok(paint_lines(&mut self.subset, std::slice::from_ref(line), &opts)?)
    }

    /// The two-byte code of a laid-out cluster (added to the subset if new).
    pub fn code(&mut self, cluster: &crate::shaping::Cluster) -> Result<u16, TextError> {
        Ok(self.subset.code(cluster)?)
    }

    /// The face's units per em (laid-out positions are in these units).
    pub fn units_per_em(&self) -> u16 {
        self.subset.face().units_per_em()
    }

    /// `text` laid out as a paragraph `width` points wide at `size` points.
    pub fn paragraph(&self, text: &str, size: f64, width: f64, direction: BaseDirection, align: LineAlign) -> Result<Vec<LaidLine>, TextError> {
        Ok(layout_points(self.subset.face(), text, size, width, direction, align)?)
    }

    /// Write the font into `doc`; nothing is added on error.
    pub fn write(&self, doc: &mut Document) -> Result<ObjRef, TextError> {
        self.subset.font_program()?;
        Ok(self.subset.write(doc)?)
    }
}

/// A block of text written with an embedded subset font: the content operators and the Type0
/// font to put in the resources under `opts.font`.
#[derive(Clone, Debug)]
pub struct Block {
    pub ops: String,
    pub font: ObjRef,
}

/// Paint `lines` and write their font into `doc`. Everything that can fail is done before the
/// first object is added, so on error `doc` is unchanged.
pub fn write_block(doc: &mut Document, face: &ShapingFace, lines: &[LaidLine], opts: &PaintOptions<'_>) -> Result<Block, TextError> {
    let mut subset = FontSubset::new(face);
    let ops = paint_lines(&mut subset, lines, opts)?;
    // Built (and checked) before anything is added; `write` adds nothing if this fails.
    subset.font_program()?;
    let font = subset.write(doc)?;
    Ok(Block { ops, font })
}

/// Where and how the lines are drawn. Text space is the content stream's user space.
#[derive(Clone, Debug)]
pub struct PaintOptions<'a> {
    /// The font's name in the resources (without the slash).
    pub font: &'a str,
    pub size: f64,
    /// Left edge of the box the lines were laid out in.
    pub left: f64,
    /// Baseline of the first line.
    pub baseline: f64,
    /// Distance between baselines.
    pub leading: f64,
    /// Draw with a stroked outline too, to look bold (the face has one weight). The caller sets
    /// the stroke colour (`RG`) to the fill colour.
    pub fake_bold: bool,
    /// Slant (shear) for a synthetic italic, as the tangent of the angle; 0 for upright.
    pub slant: f64,
    /// Wrap each word in a `/Span` with `/ActualText` (its characters in logical order).
    pub actual_text: bool,
}

fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let r = (v * 1000.0).round() / 1000.0;
    if r == r.trunc() { format!("{}", r as i64) } else { format!("{r}") }
}

/// The `BT … ET` block drawing `lines`. Adds the clusters to `subset`.
pub fn paint_lines(subset: &mut FontSubset<'_>, lines: &[LaidLine], o: &PaintOptions<'_>) -> Result<String, EmbedError> {
    let upem = f64::from(subset.face().units_per_em().max(1));
    let to_pt = o.size / upem;
    let mut out = format!("BT\n/{} {} Tf\n", o.font, num(o.size));
    if o.fake_bold {
        out.push_str(&format!("2 Tr {} w\n", num(o.size * 0.035)));
    }
    for (i, line) in lines.iter().enumerate() {
        let x = o.left + line.x as f64 * to_pt;
        let y = o.baseline - i as f64 * o.leading;
        out.push_str(&format!("1 0 {} 1 {} {} Tm\n", num(o.slant), num(x), num(y)));
        // Justification: extra space after each space cluster, in thousandths of the font size.
        let stretch = if line.word_spacing > 0.0 { -(line.word_spacing / upem * 1000.0) } else { 0.0 };
        for run in &line.runs {
            paint_run(subset, run, stretch, o.actual_text, &mut out)?;
        }
    }
    out.push_str("ET\n");
    Ok(out)
}

fn paint_run(subset: &mut FontSubset<'_>, run: &LaidRun, stretch: f64, actual_text: bool, out: &mut String) -> Result<(), EmbedError> {
    // Words and the spaces between them, in drawing order.
    let mut i = 0;
    let clusters = &run.clusters;
    while i < clusters.len() {
        let space = clusters.get(i).is_some_and(|c| is_space(&c.text));
        let mut j = i;
        while j < clusters.len() && clusters.get(j).is_some_and(|c| is_space(&c.text)) == space {
            j += 1;
        }
        let Some(part) = clusters.get(i..j) else { break };
        let mut codes = Vec::with_capacity(part.len());
        for c in part {
            codes.push(subset.code(c)?);
        }
        if space {
            if stretch != 0.0 {
                out.push('[');
                for code in &codes {
                    out.push_str(&hex_codes(&[*code]));
                    out.push(' ');
                    out.push_str(&num(stretch));
                    out.push(' ');
                }
                out.push_str("] TJ\n");
            } else {
                out.push_str(&format!("{} Tj\n", hex_codes(&codes)));
            }
        } else if !actual_text {
            out.push_str(&format!("{} Tj\n", hex_codes(&codes)));
        } else {
            // The word as typed: clusters hold logical text; a right-to-left run is drawn from its end.
            let word: String =
                if run.rtl { part.iter().rev().map(|c| c.text.as_str()).collect() } else { part.iter().map(|c| c.text.as_str()).collect() };
            out.push_str(&actual_text_bdc(&word));
            out.push_str(&format!(" {} Tj EMC\n", hex_codes(&codes)));
        }
        i = j;
    }
    Ok(())
}

fn is_space(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| c == ' ' || c == '\u{00A0}' || c == '\u{3000}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{BaseDirection, LineAlign, layout};
    use crate::shaping::tests::face_or_skip;

    fn opts() -> PaintOptions<'static> {
        PaintOptions { font: "PCAr1", size: 12.0, left: 72.0, baseline: 700.0, leading: 14.4, fake_bold: false, slant: 0.0, actual_text: false }
    }

    #[test]
    fn words_carry_actual_text_in_logical_order_when_asked() {
        let Some(face) = face_or_skip() else { return };
        let lines = layout(face, "مرحبا بالعالم 2026", 1_000_000, BaseDirection::Auto, LineAlign::Start).unwrap();
        let mut s = FontSubset::new(face);
        let plain = paint_lines(&mut s, &lines, &opts()).unwrap();
        assert!(!plain.contains("BDC") && !plain.contains("ActualText"), "off by default: {plain}");
        let ops = paint_lines(&mut s, &lines, &PaintOptions { actual_text: true, ..opts() }).unwrap();
        assert!(ops.starts_with("BT\n/PCAr1 12 Tf\n") && ops.ends_with("ET\n"));
        for word in ["مرحبا", "بالعالم", "2026"] {
            assert!(ops.contains(&actual_text_bdc(word)), "{word}: {ops}");
        }
        // Drawn right to left: the last Arabic word first, after the number.
        let at = |w: &str| ops.find(&actual_text_bdc(w)).unwrap();
        assert!(at("2026") < at("بالعالم") && at("بالعالم") < at("مرحبا"), "{ops}");
        assert_eq!(ops.matches("BDC").count(), ops.matches("EMC").count());
    }

    #[test]
    fn justified_lines_stretch_their_spaces() {
        let Some(face) = face_or_skip() else { return };
        let text = "كلمة كلمة كلمة كلمة كلمة كلمة";
        let w = layout(face, text, 1_000_000, BaseDirection::Auto, LineAlign::Start).unwrap()[0].advance() * 2 / 3;
        let lines = layout(face, text, w, BaseDirection::Auto, LineAlign::Justify).unwrap();
        let mut s = FontSubset::new(face);
        let ops = paint_lines(&mut s, &lines, &opts()).unwrap();
        assert!(ops.contains("] TJ"), "{ops}");
        assert!(ops.contains(" -"), "negative TJ adjustments widen the spaces: {ops}");
    }

    #[test]
    fn bold_and_slant() {
        let Some(face) = face_or_skip() else { return };
        let lines = layout(face, "سلام", 100_000, BaseDirection::Auto, LineAlign::Start).unwrap();
        let mut s = FontSubset::new(face);
        let ops = paint_lines(&mut s, &lines, &PaintOptions { fake_bold: true, slant: 0.2, ..opts() }).unwrap();
        assert!(ops.contains("2 Tr") && ops.contains("1 0 0.2 1 "), "{ops}");
    }

    #[test]
    fn numbers_format() {
        assert_eq!(num(12.0), "12");
        assert_eq!(num(1.23456), "1.235");
        assert_eq!(num(f64::NAN), "0");
    }
}
