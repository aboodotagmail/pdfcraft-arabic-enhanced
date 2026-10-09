//! Content-stream text for laid-out lines in an embedded subset font: two-byte glyph codes in
//! drawing order, one code per shaped cluster.
//!
//! Right-to-left text is drawn in visual order with an exact per-code ToUnicode map, the way
//! Chromium, Word and LibreOffice write Arabic; PDF text extractors (Acrobat, Poppler, MuPDF,
//! pdf.js, PdfCraft) restore logical order from that. `/ActualText` spans are optional
//! ([`PaintOptions::actual_text`]) and off by default: Poppler spreads an ActualText string
//! left to right over the span and then reads right-to-left text from the right, which reverses
//! the word (checked with pdftotext 24.02 against a Chromium-made reference file).

use crate::embed::{EmbedError, FontSubset, actual_text_bdc, hex_codes};
use crate::layout::{LaidLine, LaidRun};

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
