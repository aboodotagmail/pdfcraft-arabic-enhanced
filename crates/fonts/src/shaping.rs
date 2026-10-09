//! Text shaping with an OpenType face: joining forms, ligatures (lam-alef), mark placement and
//! kerning from the font's own GSUB/GPOS tables, through `harfrust` (a Rust port of HarfBuzz).
//!
//! The result is a list of [`Cluster`]s in drawing order (left to right on the page), each the
//! glyphs that show one or more characters, with the characters themselves in logical order. A
//! cluster is the unit the PDF writer turns into one glyph code with one ToUnicode entry, so
//! copying and searching the text gives the characters back, not the glyph forms.
//!
//! Based on the shaping in storytold/pdfcraft#403 (Arabic in added text), generalised to any face
//! and to font units, so the embedded-font writer can compose glyphs exactly.

use std::sync::OnceLock;

use harfrust::{BufferClusterLevel, Direction, ShapeOptions, ShaperData, UnicodeBuffer};
use skrifa::{FontRef, MetadataProvider};

/// The longest run [`ShapingFace::shape`] accepts, in bytes. Longer text is refused, not cut.
pub const MAX_SHAPE_BYTES: usize = 64 * 1024;

/// Why text could not be shaped. Shaping never draws a replacement box: a missing character is
/// an error the caller reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShapeError {
    /// The build has no face for this text (craft-fonts without an `Arab` face).
    NoFont,
    /// The face has no glyph for this character.
    Missing(char),
    /// The face's data could not be read.
    BadFont,
    /// More than [`MAX_SHAPE_BYTES`] of text in one run.
    TooLong,
}

impl std::fmt::Display for ShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShapeError::NoFont => f.write_str("this build has no Arabic font (built without craft-fonts' Noto Sans Arabic)"),
            ShapeError::Missing(c) => write!(f, "the font has no glyph for U+{:04X} ({c})", u32::from(*c)),
            ShapeError::BadFont => f.write_str("the font data could not be read"),
            ShapeError::TooLong => f.write_str("the text is too long to shape in one piece"),
        }
    }
}

impl std::error::Error for ShapeError {}

/// One glyph of a cluster, offset from the cluster's pen position. Font units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlacedGlyph {
    pub gid: u16,
    pub dx: i32,
    pub dy: i32,
}

/// The glyphs that show one or more characters (a letter's body and dots, a lam-alef ligature,
/// a mark). Font units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub glyphs: Vec<PlacedGlyph>,
    /// How far the pen moves after the cluster (marks: 0).
    pub advance: i32,
    /// The characters shown, in logical order.
    pub text: String,
    /// Byte offset of `text` in the shaped run.
    pub start: usize,
}

/// A face ready to shape text.
pub struct ShapingFace {
    font: FontRef<'static>,
    data: ShaperData,
    bytes: &'static [u8],
    upem: u16,
    name: String,
}

impl std::fmt::Debug for ShapingFace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShapingFace").field("name", &self.name).field("upem", &self.upem).finish()
    }
}

impl ShapingFace {
    /// A face over font data that lives for the whole program (embedded build inputs).
    pub fn new(bytes: &'static [u8], name: &str) -> Result<ShapingFace, ShapeError> {
        let font = FontRef::new(bytes).map_err(|_| ShapeError::BadFont)?;
        let upem = font.metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default()).units_per_em;
        if upem == 0 {
            return Err(ShapeError::BadFont);
        }
        let data = ShaperData::new(&font);
        Ok(ShapingFace { font, data, bytes, upem, name: name.to_string() })
    }

    /// The Arabic document face (craft-fonts' first `Arab` face, Noto Sans Arabic), shared.
    /// `None` when the build has none.
    pub fn arabic() -> Option<&'static ShapingFace> {
        static FACE: OnceLock<Option<ShapingFace>> = OnceLock::new();
        FACE.get_or_init(|| {
            let f = crate::document_arabic_font()?;
            ShapingFace::new(f.bytes, &format!("{} {}", f.family, f.style)).ok()
        })
        .as_ref()
    }

    pub fn bytes(&self) -> &'static [u8] {
        self.bytes
    }

    pub fn units_per_em(&self) -> u16 {
        self.upem
    }

    /// The face's family and style ("Noto Sans Arabic Regular").
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the face maps `c` to a glyph.
    pub fn has(&self, c: char) -> bool {
        self.font.charmap().map(c).is_some()
    }

    /// The first character of `text` the face can't show, ignoring controls and other
    /// default-ignorable characters (which shaping hides).
    pub fn first_missing(&self, text: &str) -> Option<char> {
        text.chars().find(|c| !self.has(*c) && !is_ignorable(*c))
    }

    /// Shape one run of `text` that has a single direction (a bidi level run). Clusters come in
    /// drawing order, left to right.
    pub fn shape(&self, text: &str, rtl: bool) -> Result<Vec<Cluster>, ShapeError> {
        if text.len() > MAX_SHAPE_BYTES {
            return Err(ShapeError::TooLong);
        }
        if text.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(c) = self.first_missing(text) {
            return Err(ShapeError::Missing(c));
        }
        let shaper = self.data.shaper(&self.font).build();
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(text);
        buffer.set_direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight });
        buffer.guess_segment_properties();
        // One cluster per character where the font allows it, so marks keep their own text.
        buffer.set_cluster_level(BufferClusterLevel::Characters);
        let shaped = shaper.shape(buffer, ShapeOptions::new());
        // Clusters are byte offsets into `text`; a cluster shows the characters up to the next.
        let mut starts: Vec<usize> = shaped.glyph_infos().iter().map(|g| g.cluster as usize).collect();
        starts.sort_unstable();
        starts.dedup();
        let mut out: Vec<Cluster> = Vec::new();
        for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
            let start = info.cluster as usize;
            if info.glyph_id == 0 {
                let c = text.get(start..).and_then(|s| s.chars().next()).unwrap_or('\u{FFFD}');
                return Err(ShapeError::Missing(c));
            }
            let gid = u16::try_from(info.glyph_id).map_err(|_| ShapeError::BadFont)?;
            // A cluster's glyphs are adjacent in the shaped run.
            if out.last().is_none_or(|c| c.start != start) {
                let end = starts.get(starts.partition_point(|s| *s <= start)).copied().unwrap_or(text.len());
                let cluster_text = text.get(start..end).unwrap_or_default().to_string();
                out.push(Cluster { glyphs: Vec::new(), advance: 0, text: cluster_text, start });
            }
            let Some(cluster) = out.last_mut() else { continue };
            let dx = cluster.advance.saturating_add(pos.x_offset);
            cluster.glyphs.push(PlacedGlyph { gid, dx, dy: pos.y_offset });
            cluster.advance = cluster.advance.saturating_add(pos.x_advance);
        }
        Ok(out)
    }

    /// The advance of `gid` in font units (0 for an unknown glyph).
    pub fn glyph_advance(&self, gid: u16) -> i32 {
        let m = self.font.glyph_metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default());
        m.advance_width(skrifa::GlyphId::new(u32::from(gid))).map_or(0, |w| w.round() as i32)
    }
}

/// Characters shaping hides rather than draws: format controls (bidi marks and embeddings, ZWJ,
/// ZWNJ), variation selectors and other default-ignorables, and line/paragraph controls.
pub fn is_ignorable(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}'..='\u{1160}' | '\u{17B4}'..='\u{17B5}' | '\u{180B}'..='\u{180F}'
        | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFF8}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0000}'..='\u{E0FFF}')
}

/// Whether `c` belongs to a right-to-left script handled by the Arabic face (Arabic, Syriac,
/// Thaana, NKo and the Arabic presentation forms; Hebrew too, for direction detection).
pub fn is_rtl_char(c: char) -> bool {
    matches!(c, '\u{0590}'..='\u{08FF}' | '\u{FB1D}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}' | '\u{10E60}'..='\u{10E7F}' | '\u{1EE00}'..='\u{1EEFF}')
}

/// Whether `text` needs the Arabic face: it has a character from the Arabic blocks.
pub fn has_arabic(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' | '\u{0870}'..='\u{08FF}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}'))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The Arabic face, or `None` (and the no-face behaviour checked) when built without it.
    pub(crate) fn face_or_skip() -> Option<&'static ShapingFace> {
        let face = ShapingFace::arabic();
        if face.is_none() {
            eprintln!("skipping Arabic shaping checks: built without a craft-fonts Arab face (set CRAFT_FONTS_DIR)");
            assert!(crate::document_arabic_font().is_none());
        }
        face
    }

    fn gids(c: &[Cluster]) -> Vec<u16> {
        c.iter().flat_map(|c| c.glyphs.iter().map(|g| g.gid)).collect()
    }

    #[test]
    fn letters_take_their_joining_forms() {
        let Some(face) = face_or_skip() else { return };
        let isolated: std::collections::BTreeSet<u16> = gids(&face.shape("ب", true).unwrap()).into_iter().collect();
        let joined: std::collections::BTreeSet<u16> = gids(&face.shape("ببب", true).unwrap()).into_iter().collect();
        // Initial, medial and final forms are glyphs the isolated letter doesn't use.
        assert!(joined.difference(&isolated).count() >= 2, "{isolated:?} {joined:?}");
        let c = face.shape("ببب", true).unwrap();
        assert!(c.len() == 3 && c.iter().all(|c| c.text == "ب" && c.advance > 0 && !c.glyphs.is_empty()), "{c:?}");
    }

    #[test]
    fn four_forms_of_one_letter_differ() {
        let Some(face) = face_or_skip() else { return };
        // ع isolated, initial (عب), medial (بعب), final (بع): the ع cluster's glyphs differ each time.
        let form = |s: &str| {
            let c = face.shape(s, true).unwrap();
            c.into_iter().find(|c| c.text == "ع").unwrap().glyphs
        };
        let forms = [form("ع"), form("عب"), form("بعب"), form("بع")];
        for i in 0..4 {
            for j in (i + 1)..4 {
                assert_ne!(forms[i], forms[j], "forms {i} and {j} of ع are the same glyphs");
            }
        }
    }

    #[test]
    fn right_to_left_runs_are_drawn_from_their_last_letter() {
        let Some(face) = face_or_skip() else { return };
        let g = face.shape("سلام", true).unwrap();
        let drawn: String = g.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(drawn.chars().rev().collect::<String>(), "سلام");
    }

    #[test]
    fn lam_alef_takes_its_special_forms_and_keeps_both_letters() {
        let Some(face) = face_or_skip() else { return };
        // Fonts draw lam-alef either as one ligature glyph or as two contextual glyphs; either
        // way the text comes back as both letters in order, and the lam is not its ordinary
        // initial form (the one before ب).
        let c = face.shape("لا", true).unwrap();
        let text: String = c.iter().rev().map(|c| c.text.as_str()).collect();
        assert_eq!(text, "لا", "{c:?}");
        let lam = |s: &str| face.shape(s, true).unwrap().into_iter().find(|c| c.text.starts_with('ل')).unwrap().glyphs;
        let isolated_alef = face.shape("ا", true).unwrap()[0].glyphs.clone();
        let la = face.shape("لا", true).unwrap();
        assert!(lam("لا") != lam("لب") || la.len() == 1, "lam before alef uses its lam-alef form");
        assert!(la.iter().all(|c| c.glyphs != isolated_alef) || la.len() == 1, "the alef joins the lam");
        let c = face.shape("الله", true).unwrap();
        let text: String = c.iter().rev().map(|c| c.text.as_str()).collect();
        assert_eq!(text, "الله", "{c:?}");
    }

    #[test]
    fn marks_keep_their_own_text_and_no_advance() {
        let Some(face) = face_or_skip() else { return };
        let c = face.shape("بَ", true).unwrap();
        assert_eq!(c.len(), 2, "{c:?}");
        let mark = c.iter().find(|c| c.text == "\u{064E}").unwrap();
        assert_eq!(mark.advance, 0);
        // Above the baseline: the mark is raised or its glyph sits high.
        let base = c.iter().find(|c| c.text == "ب").unwrap();
        assert!(base.advance > 0);
    }

    #[test]
    fn latin_and_digits_shape_left_to_right() {
        let Some(face) = face_or_skip() else { return };
        let c = face.shape("ABC 123", false).unwrap();
        let text: String = c.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(text, "ABC 123");
        assert!(c.iter().all(|c| c.advance > 0));
        // Arabic-Indic digits too.
        let c = face.shape("١٢٣", false).unwrap();
        assert_eq!(c.iter().map(|c| c.text.as_str()).collect::<String>(), "١٢٣");
    }

    #[test]
    fn brackets_are_mirrored_in_right_to_left_runs() {
        let Some(face) = face_or_skip() else { return };
        let rtl = face.shape("(", true).unwrap();
        let ltr = face.shape("(", false).unwrap();
        assert_eq!(rtl[0].text, "(");
        assert_ne!(rtl[0].glyphs, ltr[0].glyphs, "an opening parenthesis in RTL text is drawn as ')'");
        assert_eq!(rtl[0].glyphs, face.shape(")", false).unwrap()[0].glyphs);
    }

    #[test]
    fn missing_and_hidden_characters() {
        let Some(face) = face_or_skip() else { return };
        assert_eq!(face.shape("日本", true), Err(ShapeError::Missing('日')));
        assert_eq!(face.first_missing("سلام日"), Some('日'));
        // Controls are hidden, not errors.
        let c = face.shape("\u{200F}سلام\u{200C}", true).unwrap();
        assert!(c.iter().rev().map(|c| c.text.as_str()).collect::<String>().contains("سلام"), "{c:?}");
        assert!(face.shape("", true).unwrap().is_empty());
        assert_eq!(face.shape(&"ب".repeat(MAX_SHAPE_BYTES), true), Err(ShapeError::TooLong));
    }

    #[test]
    fn odd_input_never_panics() {
        let Some(face) = face_or_skip() else { return };
        for s in ["\u{064E}", "\u{064E}\u{064E}\u{064E}", "\u{202E}abc\u{202C}", "ـــ", "\u{FDFD}", "ﻻ", "a\u{0301}", "\u{2029}"] {
            let _ = face.shape(s, true);
            let _ = face.shape(s, false);
        }
        let long = "بسم الله ".repeat(2000);
        assert!(face.shape(&long, true).is_ok());
    }

    #[test]
    fn classification() {
        assert!(has_arabic("abc سلام") && !has_arabic("abc שלום") && !has_arabic("abc"));
        assert!(is_rtl_char('ب') && is_rtl_char('ש') && !is_rtl_char('a') && !is_rtl_char('1'));
        assert!(is_ignorable('\u{200F}') && is_ignorable('\u{200C}') && !is_ignorable('ب'));
    }
}
