//! What each glyph of an embedded TrueType font shows, read from the font program itself, for
//! codes a PDF leaves without a Unicode meaning.
//!
//! PDF producers often give Arabic text a `/ToUnicode` map that covers only some glyphs: the
//! contextual forms (initial, medial, final) and ligatures that shaping substitutes may be
//! missing, or mapped to private-use code points. Those glyphs are still in the embedded font,
//! and the font says what they are:
//! 1. its `cmap` maps characters to glyphs (read backwards: glyph → character);
//! 2. its `GSUB` substitutions turn those glyphs into forms and ligatures, which show the same
//!    characters (a single substitution keeps the text; a ligature joins its components' text);
//! 3. its `post` glyph names often spell the characters out (`uni0644_uni0627`, `alef.fina`).
//!
//! Text from a presentation-form or private-use code point ranks below text from an ordinary
//! character, so a form glyph mapped in `cmap` to U+FE8E still reads as alef.

use std::collections::HashMap;

use skrifa::MetadataProvider;
use skrifa::raw::tables::gsub::{SingleSubst, SubstitutionSubtables};
use skrifa::raw::{FontRef, TableProvider};

/// Rounds of substitution followed (forms of forms, ligatures of forms).
const MAX_PASSES: usize = 4;
/// Characters one glyph may stand for.
const MAX_CHARS: usize = 8;
/// GSUB lookups and subtables read per font, and ligatures per set: bounds the work a hostile
/// font can ask for.
const MAX_LOOKUPS: usize = 2048;
const MAX_LIGATURES: usize = 4096;

/// How trustworthy a glyph's text is: lower is better. Private-use and presentation-form code
/// points rank last; then letters that are the dotless or variant skeleton of more common ones
/// (dotless beh, qaf and feh, keheh, noon ghunna), then the extended Arabic letters. Fonts share
/// one glyph between letters whose forms look alike (medial kaf and keheh, medial yeh and yeh
/// barree, letters with dots drawn as a dotless base): the glyph itself can't say which was
/// typed, and the core Arabic letter is the likely one.
fn rank(text: &str) -> u8 {
    if text.chars().any(is_private_use) {
        4
    } else if text.chars().any(is_presentation_form) {
        3
    } else if text.chars().any(|c| matches!(c, '\u{066E}' | '\u{066F}' | '\u{06A1}' | '\u{06A9}' | '\u{06BA}')) {
        2
    } else if text.chars().any(|c| matches!(u32::from(c), 0x0671..=0x06D3 | 0x06FA..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF)) {
        1
    } else {
        0
    }
}

/// Store `text` for `key` unless a better-ranked text is already there. True if stored.
fn offer(sequences: &mut HashMap<Vec<u16>, String>, key: Vec<u16>, text: String) -> bool {
    match sequences.get(&key) {
        Some(have) if rank(have) <= rank(&text) => false,
        Some(_) => {
            sequences.insert(key, text);
            true
        }
        None if sequences.len() < MAX_SEQUENCES => {
            sequences.insert(key, text);
            true
        }
        None => false,
    }
}

/// Most glyph sequences remembered (they multiply as substitutions are followed).
const MAX_SEQUENCES: usize = 20_000;

pub fn is_private_use(c: char) -> bool {
    matches!(u32::from(c), 0xE000..=0xF8FF | 0xF_0000..=0x10_FFFF)
}

pub fn is_presentation_form(c: char) -> bool {
    matches!(u32::from(c), 0xFB50..=0xFDFF | 0xFE70..=0xFEFE)
}

/// Arabic presentation forms (positional letters and ligatures) as the letters they show
/// (compatibility decomposition); other text is unchanged.
pub fn normalize_presentation_forms(text: &str) -> std::borrow::Cow<'_, str> {
    use unicode_normalization::UnicodeNormalization;
    if !text.chars().any(is_presentation_form) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len().saturating_mul(2));
    for c in text.chars() {
        if is_presentation_form(c) {
            out.extend(std::iter::once(c).nfkc());
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// What a font program's glyphs show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GlyphTexts {
    /// Glyph id → its text.
    pub glyphs: HashMap<u16, String>,
    /// Glyph sequences a multiple substitution makes from one character (a letter decomposed into
    /// a dotless base and its dots, as `ccmp` does in some fonts) → that character. The base
    /// alone may read as another letter (U+066E dotless beh for the first part of ي).
    pub sequences: HashMap<Vec<u16>, String>,
}

/// Longest multiple-substitution output remembered as a sequence.
pub const MAX_SEQUENCE: usize = 4;

/// Glyph id → the text it shows, for every glyph the font program explains. Empty for data
/// that isn't a TrueType/OpenType font.
pub fn glyph_texts(bytes: &[u8]) -> HashMap<u16, String> {
    font_texts(bytes).glyphs
}

/// [`glyph_texts`] with the multiple-substitution sequences.
pub fn font_texts(bytes: &[u8]) -> GlyphTexts {
    let Ok(font) = FontRef::new(bytes) else { return GlyphTexts::default() };
    let mut sequences: HashMap<Vec<u16>, String> = HashMap::new();
    let mut out: HashMap<u16, (String, u8)> = HashMap::new();
    let mut set = |out: &mut HashMap<u16, (String, u8)>, gid: u16, text: String| -> bool {
        if text.is_empty() || text.chars().count() > MAX_CHARS {
            return false;
        }
        let r = rank(&text);
        match out.get(&gid) {
            Some((_, have)) if *have <= r => false,
            _ => {
                out.insert(gid, (text, r));
                true
            }
        }
    };
    // 1. cmap, backwards; among several characters for one glyph the best ranked, then the lowest.
    let mut pairs: Vec<(u32, u16)> = font.charmap().mappings().filter_map(|(cp, gid)| Some((cp, u16::try_from(gid.to_u32()).ok()?))).collect();
    pairs.sort_unstable();
    for (cp, gid) in pairs {
        if let Some(c) = char::from_u32(cp).filter(|c| !c.is_control()) {
            set(&mut out, gid, c.to_string());
        }
    }
    // 2. GSUB: what substitutions make from glyphs whose text is known.
    if let Ok(gsub) = font.gsub()
        && let Ok(list) = gsub.lookup_list()
    {
        for _ in 0..MAX_PASSES {
            let mut changed = false;
            for lookup in list.lookups().iter().take(MAX_LOOKUPS).flatten() {
                let Ok(subtables) = lookup.subtables() else { continue };
                changed |= follow(&subtables, &mut out, &mut set, &mut sequences);
            }
            if !changed {
                break;
            }
        }
    }
    // 3. post glyph names, for glyphs still unexplained.
    if let Ok(post) = font.post() {
        let count = font.maxp().map(|m| m.num_glyphs()).unwrap_or(0);
        for gid in 0..count {
            if out.contains_key(&gid) {
                continue;
            }
            if let Some(text) = post.glyph_name(gid.into()).and_then(text_from_glyph_name) {
                set(&mut out, gid, text);
            }
        }
    }
    GlyphTexts { glyphs: out.into_iter().map(|(g, (t, _))| (g, t)).collect(), sequences }
}

type Setter<'s> = dyn FnMut(&mut HashMap<u16, (String, u8)>, u16, String) -> bool + 's;

/// One lookup's substitutions: give each output glyph the text of what it replaces.
fn follow(
    subtables: &SubstitutionSubtables<'_>,
    out: &mut HashMap<u16, (String, u8)>,
    set: &mut Setter<'_>,
    sequences: &mut HashMap<Vec<u16>, String>,
) -> bool {
    let text = |out: &HashMap<u16, (String, u8)>, g: u16| out.get(&g).map(|(t, _)| t.clone());
    let mut changed = false;
    match subtables {
        SubstitutionSubtables::Single(s) => {
            for sub in s.iter().take(MAX_LOOKUPS).flatten() {
                match sub {
                    SingleSubst::Format1(f) => {
                        let Ok(cov) = f.coverage() else { continue };
                        let delta = f.delta_glyph_id();
                        for g in cov.iter() {
                            let from = g.to_u16();
                            let to = from.wrapping_add_signed(delta);
                            if let Some(t) = text(out, from) {
                                changed |= set(out, to, t);
                            }
                            changed |= carry(sequences, from, to);
                        }
                    }
                    SingleSubst::Format2(f) => {
                        let Ok(cov) = f.coverage() else { continue };
                        for (g, to) in cov.iter().zip(f.substitute_glyph_ids()) {
                            if let Some(t) = text(out, g.to_u16()) {
                                changed |= set(out, to.get().to_u16(), t);
                            }
                            changed |= carry(sequences, g.to_u16(), to.get().to_u16());
                        }
                    }
                }
            }
        }
        SubstitutionSubtables::Multiple(s) => {
            for sub in s.iter().take(MAX_LOOKUPS).flatten() {
                let Ok(cov) = sub.coverage() else { continue };
                for (g, seq) in cov.iter().zip(sub.sequences().iter()) {
                    let Ok(seq) = seq else { continue };
                    let glyphs: Vec<u16> = seq.substitute_glyph_ids().iter().map(|g| g.get().to_u16()).collect();
                    if let [one] = glyphs[..] {
                        changed |= carry(sequences, g.to_u16(), one);
                    }
                    let Some(t) = text(out, g.to_u16()) else { continue };
                    if let [one] = glyphs[..] {
                        // A one-glyph "multiple" substitution is a single one (fonts write
                        // positional forms this way): it applies inside sequences too.
                        changed |= set(out, one, t);
                    } else if (2..=MAX_SEQUENCE).contains(&glyphs.len()) && rank(&t) <= 1 {
                        // A character split into several glyphs (a dotless base and its dots): only
                        // the whole sequence shows it; the base alone is shared by several letters.
                        changed |= offer(sequences, glyphs, t);
                    }
                }
            }
        }
        SubstitutionSubtables::Alternate(s) => {
            for sub in s.iter().take(MAX_LOOKUPS).flatten() {
                let Ok(cov) = sub.coverage() else { continue };
                for (g, alts) in cov.iter().zip(sub.alternate_sets().iter()) {
                    let (Ok(alts), Some(t)) = (alts, text(out, g.to_u16())) else { continue };
                    for a in alts.alternate_glyph_ids() {
                        changed |= set(out, a.get().to_u16(), t.clone());
                    }
                }
            }
        }
        SubstitutionSubtables::Ligature(s) => {
            for sub in s.iter().take(MAX_LOOKUPS).flatten() {
                let Ok(cov) = sub.coverage() else { continue };
                for (g, ligs) in cov.iter().zip(sub.ligature_sets().iter()) {
                    let Ok(ligs) = ligs else { continue };
                    let first = text(out, g.to_u16());
                    for lig in ligs.ligatures().iter().take(MAX_LIGATURES).flatten() {
                        // Components that are a known sequence (a base and its dots) join into what
                        // the sequence shows.
                        let comps: Vec<u16> = std::iter::once(g.to_u16()).chain(lig.component_glyph_ids().iter().map(|c| c.get().to_u16())).collect();
                        if let Some(t) = sequences.get(&comps).cloned() {
                            changed |= set(out, lig.ligature_glyph().to_u16(), t);
                            continue;
                        }
                        let Some(mut t) = first.clone() else { continue };
                        let mut whole = true;
                        for c in lig.component_glyph_ids() {
                            match text(out, c.get().to_u16()) {
                                Some(ct) => t.push_str(&ct),
                                None => {
                                    whole = false;
                                    break;
                                }
                            }
                        }
                        if whole {
                            changed |= set(out, lig.ligature_glyph().to_u16(), t);
                        }
                    }
                }
            }
        }
        SubstitutionSubtables::Reverse(s) => {
            for sub in s.iter().take(MAX_LOOKUPS).flatten() {
                let Ok(cov) = sub.coverage() else { continue };
                for (g, to) in cov.iter().zip(sub.substitute_glyph_ids()) {
                    if let Some(t) = text(out, g.to_u16()) {
                        changed |= set(out, to.get().to_u16(), t);
                    }
                    changed |= carry(sequences, g.to_u16(), to.get().to_u16());
                }
            }
        }
        // Contextual lookups only point at other lookups, which are followed themselves.
        SubstitutionSubtables::Contextual(_) | SubstitutionSubtables::ChainContextual(_) | SubstitutionSubtables::EmptyExtension => {}
    }
    changed
}

/// A single substitution `from` → `to` applies inside known sequences too (a dotless base taking
/// its medial form keeps its dots): add the substituted copies. True if any was added.
fn carry(sequences: &mut HashMap<Vec<u16>, String>, from: u16, to: u16) -> bool {
    if from == to || sequences.len() >= MAX_SEQUENCES {
        return false;
    }
    let new: Vec<(Vec<u16>, String)> = sequences
        .iter()
        .filter(|(k, _)| k.contains(&from))
        .map(|(k, v)| (k.iter().map(|g| if *g == from { to } else { *g }).collect::<Vec<u16>>(), v.clone()))
        .collect();
    let mut added = false;
    for (k, v) in new {
        added |= offer(sequences, k, v);
    }
    added
}

/// The characters a glyph name spells (Adobe glyph naming): `uni0644_uni0627.fina` → "لا",
/// `u1EE00` → "𞸀", `alef` → "ا". Suffixes after `.` name a form of the same characters.
pub fn text_from_glyph_name(name: &str) -> Option<String> {
    let base = name.split('.').next().unwrap_or(name);
    if base.is_empty() || base == ".notdef" {
        return None;
    }
    let mut out = String::new();
    for part in base.split('_') {
        if let Some(hex) = part.strip_prefix("uni").filter(|h| !h.is_empty() && h.len() % 4 == 0 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            for i in (0..hex.len()).step_by(4) {
                let c = hex.get(i..i + 4).and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32)?;
                out.push(c);
            }
        } else if let Some(c) = part
            .strip_prefix('u')
            .filter(|h| (4..=6).contains(&h.len()) && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .and_then(char::from_u32)
        {
            out.push(c);
        } else {
            out.push(crate::pdf::glyph_unicode(part)?);
        }
    }
    (!out.is_empty() && !out.chars().any(char::is_control)).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_names_spell_their_characters() {
        assert_eq!(text_from_glyph_name("uni0644_uni0627.fina").as_deref(), Some("لا"));
        assert_eq!(text_from_glyph_name("uni06440627").as_deref(), Some("لا"));
        assert_eq!(text_from_glyph_name("u1EE00").as_deref(), Some("\u{1EE00}"));
        assert_eq!(text_from_glyph_name("A.sc").as_deref(), Some("A"));
        assert_eq!(text_from_glyph_name(".notdef"), None);
        assert_eq!(text_from_glyph_name("glyph123"), None);
        assert_eq!(text_from_glyph_name("uniZZZZ"), None);
    }

    #[test]
    fn presentation_forms_become_letters() {
        assert_eq!(normalize_presentation_forms("ﻻ"), "لا");
        assert_eq!(normalize_presentation_forms("ﺑﺴﻢ"), "بسم");
        assert_eq!(normalize_presentation_forms("plain سلام"), "plain سلام");
    }

    #[test]
    fn arabic_face_explains_its_contextual_forms() {
        let Some(face) = crate::shaping::ShapingFace::arabic() else { return };
        let texts = glyph_texts(face.bytes());
        // Shaping "سلام" uses initial, medial and final forms that aren't in cmap: every glyph
        // the shaper draws must read back as its letter.
        let clusters = face.shape("سلام", true).unwrap();
        for c in &clusters {
            for g in &c.glyphs {
                let t = texts.get(&g.gid).unwrap_or_else(|| panic!("glyph {} ({:?}) unexplained", g.gid, c.text));
                assert_eq!(normalize_presentation_forms(t), c.text, "glyph {}", g.gid);
            }
        }
        assert!(glyph_texts(b"not a font").is_empty());
    }
}
