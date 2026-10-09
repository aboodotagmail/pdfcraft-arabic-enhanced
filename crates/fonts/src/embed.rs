//! Embedded TrueType subsets for shaped text: a `Type0` font with a `CIDFontType2` descendant,
//! `Identity-H` encoding, a `FontFile2` subset of the face and a `ToUnicode` CMap.
//!
//! Every shaped [`Cluster`] (a letter with its dots, a lam-alef ligature, a mark, a space) becomes
//! ONE glyph code. When a cluster is drawn with several font glyphs, or with offsets from mark
//! positioning, the subset gets a TrueType composite glyph that places those glyphs exactly as
//! the shaper did. So each code has a single, unambiguous ToUnicode entry: the cluster's
//! characters in logical order. Copy and search give the text back, not the glyph forms, in any
//! PDF reader; the glyph outlines stay real font outlines (vector, hinted, selectable text).
//!
//! The subset keeps the face's default instance: variation tables (`fvar`, `gvar`, …) and the
//! shaping tables (`GSUB`, `GPOS`, `GDEF`, whose glyph ids no longer apply) are left out. Glyph
//! programs are copied byte for byte; composite glyphs get their component ids renumbered.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString, Stream};

use crate::shaping::{Cluster, PlacedGlyph, ShapingFace};

/// The most glyph codes one subset font holds (codes are two bytes; 0 is `.notdef`).
pub const MAX_CODES: usize = 60_000;
/// Composite nesting allowed in the source face (TrueType allows little; fonts use 1–2).
const MAX_COMPONENT_DEPTH: usize = 8;
/// Components in one cluster glyph.
const MAX_CLUSTER_GLYPHS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbedError {
    /// A table the subset needs is missing or malformed in the face.
    BadFont(&'static str),
    /// More distinct clusters than one font can hold ([`MAX_CODES`]).
    TooManyGlyphs,
    /// A cluster made of more glyphs than a composite may hold.
    ClusterTooComplex,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::BadFont(what) => write!(f, "the font's {what} table could not be read"),
            EmbedError::TooManyGlyphs => f.write_str("too many different characters for one embedded font"),
            EmbedError::ClusterTooComplex => f.write_str("a character is drawn with too many glyphs"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// What makes two clusters the same glyph code: the same glyphs at the same places, the same
/// advance, and the same characters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ClusterKey {
    glyphs: Vec<PlacedGlyph>,
    advance: i32,
    text: String,
}

/// A growing subset of one face: the clusters drawn so far, each with its code.
#[derive(Debug)]
pub struct FontSubset<'f> {
    face: &'f ShapingFace,
    /// Code `i + 1` is `clusters[i]`.
    clusters: Vec<ClusterKey>,
    codes: HashMap<ClusterKey, u16>,
}

impl<'f> FontSubset<'f> {
    pub fn new(face: &'f ShapingFace) -> Self {
        FontSubset { face, clusters: Vec::new(), codes: HashMap::new() }
    }

    pub fn face(&self) -> &'f ShapingFace {
        self.face
    }

    /// Number of codes in use (not counting `.notdef`).
    pub fn len(&self) -> usize {
        self.clusters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clusters.is_empty()
    }

    /// The code for `cluster`, adding it to the subset if new.
    pub fn code(&mut self, cluster: &Cluster) -> Result<u16, EmbedError> {
        if cluster.glyphs.len() > MAX_CLUSTER_GLYPHS {
            return Err(EmbedError::ClusterTooComplex);
        }
        let key = ClusterKey { glyphs: cluster.glyphs.clone(), advance: cluster.advance.max(0), text: cluster.text.clone() };
        if let Some(c) = self.codes.get(&key) {
            return Ok(*c);
        }
        if self.clusters.len() >= MAX_CODES {
            return Err(EmbedError::TooManyGlyphs);
        }
        // 1-based (0 is .notdef); fits: MAX_CODES < u16::MAX.
        let code = u16::try_from(self.clusters.len() + 1).map_err(|_| EmbedError::TooManyGlyphs)?;
        self.clusters.push(key.clone());
        self.codes.insert(key, code);
        Ok(code)
    }

    /// The width of `code` in thousandths of an em (the `/W` value).
    pub fn width(&self, code: u16) -> f64 {
        let Some(k) = usize::from(code).checked_sub(1).and_then(|i| self.clusters.get(i)) else { return 0.0 };
        f64::from(k.advance) * 1000.0 / f64::from(self.face.units_per_em().max(1))
    }

    /// The text a code stands for (its ToUnicode entry).
    pub fn text(&self, code: u16) -> Option<&str> {
        usize::from(code).checked_sub(1).and_then(|i| self.clusters.get(i)).map(|k| k.text.as_str())
    }

    /// The subset font program (a TrueType file) with glyph `i` = code `i`.
    pub fn font_program(&self) -> Result<Vec<u8>, EmbedError> {
        build_font(self.face.bytes(), &self.clusters)
    }

    /// The `ToUnicode` CMap: each code to its cluster's characters.
    pub fn to_unicode(&self) -> Vec<u8> {
        to_unicode_cmap(self.clusters.iter().map(|k| k.text.as_str()))
    }

    /// A six-letter subset tag (ISO 32000-2 §9.9.2), stable for the same set of clusters.
    pub fn tag(&self) -> String {
        // FNV-1a over the clusters, spelled in A–Z.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for k in &self.clusters {
            for b in k.text.bytes().chain(k.glyphs.iter().flat_map(|g| g.gid.to_be_bytes())) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        (0..6)
            .map(|i| {
                let v = (h >> (i * 8)) % 26;
                // v < 26, so the sum is an ASCII capital.
                char::from(b'A' + v as u8)
            })
            .collect()
    }

    /// Write the font into `doc`: FontFile2, FontDescriptor, CIDFont, ToUnicode and the Type0
    /// font, which is returned (put it in a page's `/Font` resources).
    pub fn write(&self, doc: &mut Document) -> Result<ObjRef, EmbedError> {
        let program = self.font_program()?;
        let m = FaceMetrics::read(self.face.bytes())?;
        let base = format!("{}+{}", self.tag(), m.postscript_name);
        let scale = 1000.0 / f64::from(m.upem.max(1));

        let mut ff = Dict::new();
        ff.set(b"Length1".to_vec(), Object::Int(i64::try_from(program.len()).unwrap_or(i64::MAX)));
        let font_file = doc.add(Stream::flate(ff, &program));

        // CIDSet: which CIDs the subset has (PDF/A asks for it).
        let n = self.clusters.len() + 1;
        let mut cidset = vec![0u8; n.div_ceil(8)];
        for cid in 0..n {
            if let Some(b) = cidset.get_mut(cid / 8) {
                *b |= 0x80 >> (cid % 8);
            }
        }
        let cid_set = doc.add(Stream::flate(Dict::new(), &cidset));

        let mut fd = Dict::new();
        fd.set(b"Type".to_vec(), Object::name("FontDescriptor"));
        fd.set(b"FontName".to_vec(), Object::Name(base.clone().into_bytes()));
        fd.set(b"Flags".to_vec(), Object::Int(4));
        fd.set(b"FontBBox".to_vec(), Object::Array(m.bbox.iter().map(|v| Object::Int((f64::from(*v) * scale).round() as i64)).collect()));
        fd.set(b"ItalicAngle".to_vec(), Object::Int(0));
        fd.set(b"Ascent".to_vec(), Object::Int((f64::from(m.ascent) * scale).round() as i64));
        fd.set(b"Descent".to_vec(), Object::Int((f64::from(m.descent) * scale).round() as i64));
        fd.set(b"CapHeight".to_vec(), Object::Int((f64::from(m.cap_height) * scale).round() as i64));
        fd.set(b"StemV".to_vec(), Object::Int(80));
        fd.set(b"FontFile2".to_vec(), Object::Ref(font_file));
        fd.set(b"CIDSet".to_vec(), Object::Ref(cid_set));
        let descriptor = doc.add(Object::Dict(fd));

        // One run: [1 [w1 w2 ...]].
        let w = vec![Object::Int(1), Object::Array((1..=self.clusters.len()).map(|c| real(self.width(u16::try_from(c).unwrap_or(0)))).collect())];
        let mut cid = Dict::new();
        cid.set(b"Type".to_vec(), Object::name("Font"));
        cid.set(b"Subtype".to_vec(), Object::name("CIDFontType2"));
        cid.set(b"BaseFont".to_vec(), Object::Name(base.clone().into_bytes()));
        let mut info = Dict::new();
        info.set(b"Registry".to_vec(), PdfString::literal(b"Adobe".to_vec()));
        info.set(b"Ordering".to_vec(), PdfString::literal(b"Identity".to_vec()));
        info.set(b"Supplement".to_vec(), Object::Int(0));
        cid.set(b"CIDSystemInfo".to_vec(), Object::Dict(info));
        cid.set(b"FontDescriptor".to_vec(), Object::Ref(descriptor));
        cid.set(b"DW".to_vec(), Object::Int(0));
        cid.set(b"W".to_vec(), Object::Array(w));
        cid.set(b"CIDToGIDMap".to_vec(), Object::name("Identity"));
        let cid_font = doc.add(Object::Dict(cid));

        let to_unicode = doc.add(Stream::flate(Dict::new(), &self.to_unicode()));

        let mut t0 = Dict::new();
        t0.set(b"Type".to_vec(), Object::name("Font"));
        t0.set(b"Subtype".to_vec(), Object::name("Type0"));
        t0.set(b"BaseFont".to_vec(), Object::Name(base.into_bytes()));
        t0.set(b"Encoding".to_vec(), Object::name("Identity-H"));
        t0.set(b"DescendantFonts".to_vec(), Object::Array(vec![Object::Ref(cid_font)]));
        t0.set(b"ToUnicode".to_vec(), Object::Ref(to_unicode));
        Ok(doc.add(Object::Dict(t0)))
    }
}

fn real(v: f64) -> Object {
    if v.fract() == 0.0 && v.abs() < 1e9 { Object::Int(v as i64) } else { Object::Real((v * 1000.0).round() / 1000.0) }
}

/// UTF-16BE hex of `s` (no BOM), for CMaps and content strings.
fn utf16_hex(s: &str) -> String {
    s.encode_utf16().map(|u| format!("{u:04X}")).collect()
}

/// A ToUnicode CMap mapping code `i + 1` to `texts[i]` (ISO 32000-2 §9.10.3).
pub fn to_unicode_cmap<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let entries: Vec<(usize, String)> = texts.into_iter().enumerate().filter(|(_, t)| !t.is_empty()).map(|(i, t)| (i + 1, utf16_hex(t))).collect();
    let mut out = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    for chunk in entries.chunks(100) {
        out.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (code, hex) in chunk {
            out.push_str(&format!("<{code:04X}> <{hex}>\n"));
        }
        out.push_str("endbfchar\n");
    }
    out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out.into_bytes()
}

// ---- TrueType reading -----------------------------------------------------------------------

fn be16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]))
}

fn be_i16(d: &[u8], at: usize) -> Option<i16> {
    be16(d, at).map(|v| v as i16)
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*d.get(at)?, *d.get(at + 1)?, *d.get(at + 2)?, *d.get(at + 3)?]))
}

/// The table directory of a TrueType file: tag → bytes.
fn tables(font: &[u8]) -> Option<BTreeMap<[u8; 4], &[u8]>> {
    let n = usize::from(be16(font, 4)?);
    let mut out = BTreeMap::new();
    for i in 0..n {
        let rec = 12 + i * 16;
        let tag: [u8; 4] = font.get(rec..rec + 4)?.try_into().ok()?;
        let off = usize::try_from(be32(font, rec + 8)?).ok()?;
        let len = usize::try_from(be32(font, rec + 12)?).ok()?;
        out.insert(tag, font.get(off..off.checked_add(len)?)?);
    }
    Some(out)
}

struct FaceMetrics {
    upem: u16,
    bbox: [i16; 4],
    ascent: i16,
    descent: i16,
    cap_height: i16,
    postscript_name: String,
}

impl FaceMetrics {
    fn read(font: &[u8]) -> Result<FaceMetrics, EmbedError> {
        let t = tables(font).ok_or(EmbedError::BadFont("table directory"))?;
        let head = t.get(b"head").ok_or(EmbedError::BadFont("head"))?;
        let hhea = t.get(b"hhea").ok_or(EmbedError::BadFont("hhea"))?;
        let upem = be16(head, 18).ok_or(EmbedError::BadFont("head"))?;
        let bbox = [36, 38, 40, 42].map(|o| be_i16(head, o).unwrap_or(0));
        let ascent = be_i16(hhea, 4).ok_or(EmbedError::BadFont("hhea"))?;
        let descent = be_i16(hhea, 6).ok_or(EmbedError::BadFont("hhea"))?;
        // OS/2 version 2+ has sCapHeight at 88.
        let cap_height = t.get(b"OS/2").filter(|o| be16(o, 0).unwrap_or(0) >= 2).and_then(|o| be_i16(o, 88)).unwrap_or(ascent);
        let postscript_name = t.get(b"name").and_then(|n| postscript_name(n)).unwrap_or_else(|| "Font".into());
        Ok(FaceMetrics { upem, bbox, ascent, descent, cap_height, postscript_name })
    }
}

/// Name id 6 (PostScript name), restricted to the characters a PDF name and BaseFont allow.
fn postscript_name(name: &[u8]) -> Option<String> {
    let count = usize::from(be16(name, 2)?);
    let storage = usize::from(be16(name, 4)?);
    for i in 0..count {
        let rec = 6 + i * 12;
        let (platform, name_id) = (be16(name, rec)?, be16(name, rec + 6)?);
        if name_id != 6 {
            continue;
        }
        let len = usize::from(be16(name, rec + 8)?);
        let off = storage + usize::from(be16(name, rec + 10)?);
        let raw = name.get(off..off + len)?;
        let s: String = if platform == 3 || platform == 0 {
            let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        } else {
            raw.iter().map(|b| char::from(*b)).collect()
        };
        let clean: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(60).collect();
        if !clean.is_empty() {
            return Some(clean);
        }
    }
    None
}

/// The source face's glyph data: `glyf` slices by glyph id, horizontal metrics.
struct Glyphs<'a> {
    glyf: &'a [u8],
    loca: Vec<usize>,
    hmtx: &'a [u8],
    num_h_metrics: usize,
}

impl<'a> Glyphs<'a> {
    fn read(t: &BTreeMap<[u8; 4], &'a [u8]>) -> Result<Glyphs<'a>, EmbedError> {
        let head = t.get(b"head").ok_or(EmbedError::BadFont("head"))?;
        let maxp = t.get(b"maxp").ok_or(EmbedError::BadFont("maxp"))?;
        let hhea = t.get(b"hhea").ok_or(EmbedError::BadFont("hhea"))?;
        let glyf = t.get(b"glyf").copied().ok_or(EmbedError::BadFont("glyf"))?;
        let loca_raw = t.get(b"loca").ok_or(EmbedError::BadFont("loca"))?;
        let hmtx = t.get(b"hmtx").copied().ok_or(EmbedError::BadFont("hmtx"))?;
        let num_glyphs = usize::from(be16(maxp, 4).ok_or(EmbedError::BadFont("maxp"))?);
        let long = be16(head, 50).ok_or(EmbedError::BadFont("head"))? == 1;
        let mut loca = Vec::with_capacity(num_glyphs + 1);
        for i in 0..=num_glyphs {
            let v =
                if long { be32(loca_raw, i * 4).and_then(|v| usize::try_from(v).ok()) } else { be16(loca_raw, i * 2).map(|v| usize::from(v) * 2) };
            loca.push(v.ok_or(EmbedError::BadFont("loca"))?);
        }
        let num_h_metrics = usize::from(be16(hhea, 34).ok_or(EmbedError::BadFont("hhea"))?).max(1);
        Ok(Glyphs { glyf, loca, hmtx, num_h_metrics })
    }

    fn count(&self) -> usize {
        self.loca.len().saturating_sub(1)
    }

    /// The glyph's bytes (empty for glyphs without outlines).
    fn data(&self, gid: u16) -> Result<&'a [u8], EmbedError> {
        let i = usize::from(gid);
        let (Some(&a), Some(&b)) = (self.loca.get(i), self.loca.get(i + 1)) else { return Err(EmbedError::BadFont("glyf")) };
        if b <= a {
            return Ok(&[]);
        }
        self.glyf.get(a..b).ok_or(EmbedError::BadFont("glyf"))
    }

    fn advance(&self, gid: u16) -> u16 {
        let i = usize::from(gid).min(self.num_h_metrics - 1);
        be16(self.hmtx, i * 4).unwrap_or(0)
    }

    fn lsb(&self, gid: u16) -> i16 {
        let i = usize::from(gid);
        if i < self.num_h_metrics {
            be_i16(self.hmtx, i * 4 + 2).unwrap_or(0)
        } else {
            be_i16(self.hmtx, self.num_h_metrics * 4 + (i - self.num_h_metrics) * 2).unwrap_or(0)
        }
    }

    /// Bounding box of a glyph, `None` when it has no outline.
    fn bbox(&self, gid: u16) -> Result<Option<[i16; 4]>, EmbedError> {
        let d = self.data(gid)?;
        if d.len() < 10 {
            return Ok(None);
        }
        Ok(Some([2, 4, 6, 8].map(|o| be_i16(d, o).unwrap_or(0))))
    }
}

/// Composite-glyph component flags (OpenType `glyf`).
const ARG_1_AND_2_ARE_WORDS: u16 = 0x0001;
const ARGS_ARE_XY_VALUES: u16 = 0x0002;
const WE_HAVE_A_SCALE: u16 = 0x0008;
const MORE_COMPONENTS: u16 = 0x0020;
const WE_HAVE_AN_X_AND_Y_SCALE: u16 = 0x0040;
const WE_HAVE_A_TWO_BY_TWO: u16 = 0x0080;

/// Byte offsets of the glyph-index fields of a composite glyph's components, and the ids.
fn components(data: &[u8]) -> Result<Vec<(usize, u16)>, EmbedError> {
    let mut out = Vec::new();
    if data.len() < 10 || be_i16(data, 0).unwrap_or(0) >= 0 {
        return Ok(out);
    }
    let mut at = 10;
    loop {
        let flags = be16(data, at).ok_or(EmbedError::BadFont("glyf"))?;
        let gid = be16(data, at + 2).ok_or(EmbedError::BadFont("glyf"))?;
        out.push((at + 2, gid));
        at += 4 + if flags & ARG_1_AND_2_ARE_WORDS != 0 { 4 } else { 2 };
        at += if flags & WE_HAVE_A_SCALE != 0 {
            2
        } else if flags & WE_HAVE_AN_X_AND_Y_SCALE != 0 {
            4
        } else if flags & WE_HAVE_A_TWO_BY_TWO != 0 {
            8
        } else {
            0
        };
        if flags & MORE_COMPONENTS == 0 || out.len() > 1024 {
            break;
        }
        if at >= data.len() {
            return Err(EmbedError::BadFont("glyf"));
        }
    }
    Ok(out)
}

/// The source glyphs the clusters need, with every composite's components (transitively).
fn closure(g: &Glyphs<'_>, clusters: &[ClusterKey]) -> Result<BTreeSet<u16>, EmbedError> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<(u16, usize)> = clusters.iter().flat_map(|k| k.glyphs.iter().map(|p| (p.gid, 0))).collect();
    while let Some((gid, depth)) = stack.pop() {
        if usize::from(gid) >= g.count() {
            return Err(EmbedError::BadFont("glyf"));
        }
        if depth > MAX_COMPONENT_DEPTH || !seen.insert(gid) {
            continue;
        }
        for (_, c) in components(g.data(gid)?)? {
            stack.push((c, depth + 1));
        }
    }
    seen.remove(&0);
    Ok(seen)
}

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, c| {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        sum.wrapping_add(u32::from_be_bytes(w))
    })
}

/// The subset TrueType file: glyph 0 `.notdef`, glyphs 1..=n one composite per cluster, then
/// the source glyphs they use.
fn build_font(font: &[u8], clusters: &[ClusterKey]) -> Result<Vec<u8>, EmbedError> {
    let t = tables(font).ok_or(EmbedError::BadFont("table directory"))?;
    let g = Glyphs::read(&t)?;
    let sources = closure(&g, clusters)?;
    let first_source = clusters.len() + 1;
    let total = first_source + sources.len();
    if total > usize::from(u16::MAX) {
        return Err(EmbedError::TooManyGlyphs);
    }
    // Source glyph id → id in the subset. Bounded by the check above.
    let new_id: HashMap<u16, u16> = sources.iter().enumerate().map(|(i, gid)| (*gid, (first_source + i) as u16)).collect();

    let mut glyf = Vec::new();
    let mut loca: Vec<u32> = Vec::with_capacity(total + 1);
    let mut hmtx = Vec::with_capacity(total * 4);
    let mut max_components = 0usize;
    let mut advance_max = 0u16;
    let push_glyph = |glyf: &mut Vec<u8>, loca: &mut Vec<u32>, data: &[u8]| {
        loca.push(u32::try_from(glyf.len()).unwrap_or(u32::MAX));
        glyf.extend_from_slice(data);
        pad4(glyf);
    };

    // 0: .notdef, as in the face.
    push_glyph(&mut glyf, &mut loca, g.data(0)?);
    hmtx.extend_from_slice(&g.advance(0).to_be_bytes());
    hmtx.extend_from_slice(&g.lsb(0).to_be_bytes());
    advance_max = advance_max.max(g.advance(0));

    // 1..=n: one composite per cluster.
    for k in clusters {
        let mut parts = Vec::new();
        let mut bbox: Option<[i32; 4]> = None;
        for p in &k.glyphs {
            let Some(b) = g.bbox(p.gid)? else { continue };
            let Some(&id) = new_id.get(&p.gid) else { return Err(EmbedError::BadFont("glyf")) };
            let (dx, dy) =
                (i16::try_from(p.dx).map_err(|_| EmbedError::ClusterTooComplex)?, i16::try_from(p.dy).map_err(|_| EmbedError::ClusterTooComplex)?);
            let shifted =
                [i32::from(b[0]) + i32::from(dx), i32::from(b[1]) + i32::from(dy), i32::from(b[2]) + i32::from(dx), i32::from(b[3]) + i32::from(dy)];
            bbox = Some(match bbox {
                None => shifted,
                Some(a) => [a[0].min(shifted[0]), a[1].min(shifted[1]), a[2].max(shifted[2]), a[3].max(shifted[3])],
            });
            parts.push((id, dx, dy));
        }
        let advance = u16::try_from(k.advance.max(0)).unwrap_or(u16::MAX);
        advance_max = advance_max.max(advance);
        let mut data = Vec::new();
        let mut lsb = 0i16;
        if let Some(b) = bbox {
            let clamp = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            lsb = clamp(b[0]);
            data.extend_from_slice(&(-1i16).to_be_bytes());
            for v in b {
                data.extend_from_slice(&clamp(v).to_be_bytes());
            }
            let n = parts.len();
            max_components = max_components.max(n);
            for (i, (id, dx, dy)) in parts.into_iter().enumerate() {
                let mut flags = ARG_1_AND_2_ARE_WORDS | ARGS_ARE_XY_VALUES;
                if i + 1 < n {
                    flags |= MORE_COMPONENTS;
                }
                data.extend_from_slice(&flags.to_be_bytes());
                data.extend_from_slice(&id.to_be_bytes());
                data.extend_from_slice(&dx.to_be_bytes());
                data.extend_from_slice(&dy.to_be_bytes());
            }
        }
        push_glyph(&mut glyf, &mut loca, &data);
        hmtx.extend_from_slice(&advance.to_be_bytes());
        hmtx.extend_from_slice(&lsb.to_be_bytes());
    }

    // The source glyphs, composites renumbered.
    for gid in &sources {
        let mut data = g.data(*gid)?.to_vec();
        let comps = components(&data)?;
        max_components = max_components.max(comps.len());
        for (at, c) in comps {
            let id = new_id.get(&c).copied().ok_or(EmbedError::BadFont("glyf"))?;
            if let Some(slot) = data.get_mut(at..at + 2) {
                slot.copy_from_slice(&id.to_be_bytes());
            }
        }
        push_glyph(&mut glyf, &mut loca, &data);
        hmtx.extend_from_slice(&g.advance(*gid).to_be_bytes());
        hmtx.extend_from_slice(&g.lsb(*gid).to_be_bytes());
        advance_max = advance_max.max(g.advance(*gid));
    }
    loca.push(u32::try_from(glyf.len()).unwrap_or(u32::MAX));
    let loca_bytes: Vec<u8> = loca.iter().flat_map(|v| v.to_be_bytes()).collect();
    let num = u16::try_from(total).map_err(|_| EmbedError::TooManyGlyphs)?;

    // head: long loca, checksum adjustment filled in below.
    let mut head = t.get(b"head").ok_or(EmbedError::BadFont("head"))?.to_vec();
    if head.len() < 54 {
        return Err(EmbedError::BadFont("head"));
    }
    head[8..12].copy_from_slice(&[0, 0, 0, 0]);
    head[50..52].copy_from_slice(&1u16.to_be_bytes());
    // hhea: every glyph has a full metric.
    let mut hhea = t.get(b"hhea").ok_or(EmbedError::BadFont("hhea"))?.to_vec();
    if hhea.len() < 36 {
        return Err(EmbedError::BadFont("hhea"));
    }
    hhea[10..12].copy_from_slice(&advance_max.to_be_bytes());
    hhea[34..36].copy_from_slice(&num.to_be_bytes());
    // maxp: glyph count and composite limits (cluster composites add one nesting level).
    let mut maxp = t.get(b"maxp").ok_or(EmbedError::BadFont("maxp"))?.to_vec();
    if maxp.len() < 6 {
        return Err(EmbedError::BadFont("maxp"));
    }
    maxp[4..6].copy_from_slice(&num.to_be_bytes());
    if maxp.len() >= 32 {
        let src_points = be16(&maxp, 6).unwrap_or(0);
        let src_contours = be16(&maxp, 8).unwrap_or(0);
        let comp_points = be16(&maxp, 10).unwrap_or(0);
        let comp_contours = be16(&maxp, 12).unwrap_or(0);
        // A cluster composite draws at most MAX_CLUSTER_GLYPHS glyphs of at most the face's
        // largest size; record a bound that covers it.
        let bound = |a: u16, b: u16| a.max(b).saturating_mul(u16::try_from(max_components.max(1)).unwrap_or(u16::MAX));
        maxp[10..12].copy_from_slice(&bound(comp_points, src_points).to_be_bytes());
        maxp[12..14].copy_from_slice(&bound(comp_contours, src_contours).to_be_bytes());
        let elements = be16(&maxp, 28).unwrap_or(0).max(u16::try_from(max_components).unwrap_or(u16::MAX));
        maxp[28..30].copy_from_slice(&elements.to_be_bytes());
        let depth = be16(&maxp, 30).unwrap_or(0).saturating_add(1);
        maxp[30..32].copy_from_slice(&depth.to_be_bytes());
    }
    // post format 3: no glyph names.
    let mut post = t.get(b"post").map(|p| p.get(..32).unwrap_or(p).to_vec()).unwrap_or_else(|| vec![0; 32]);
    post.resize(32, 0);
    post[0..4].copy_from_slice(&0x0003_0000u32.to_be_bytes());
    // cmap: one empty Windows Unicode subtable (codes reach glyphs through CIDs, not a cmap).
    let cmap: Vec<u8> = [
        0u16, 1, // version, one table
        3, 1, 0, 12, // platform 3, encoding 1, offset 12 (written as two u16: 0, 12)
        4, 24, 0, // format 4, length 24, language
        2, 2, 0, 0, // segCountX2 (one segment), searchRange, entrySelector, rangeShift
        0xFFFF, 0,      // endCode, reservedPad
        0xFFFF, // startCode
        1, 0, // idDelta, idRangeOffset
    ]
    .iter()
    .flat_map(|v| v.to_be_bytes())
    .collect();

    let mut out_tables: Vec<([u8; 4], Vec<u8>)> = vec![
        (*b"cmap", cmap),
        (*b"glyf", glyf),
        (*b"head", head),
        (*b"hhea", hhea),
        (*b"hmtx", hmtx),
        (*b"loca", loca_bytes),
        (*b"maxp", maxp),
        (*b"post", post),
    ];
    for tag in [*b"OS/2", *b"cvt ", *b"fpgm", *b"gasp", *b"name", *b"prep"] {
        if let Some(d) = t.get(&tag) {
            out_tables.push((tag, d.to_vec()));
        }
    }
    out_tables.sort_by_key(|(tag, _)| *tag);
    Ok(assemble(out_tables))
}

/// A TrueType file from its tables (sorted by tag), with checksums and `checkSumAdjustment`.
fn assemble(tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    let n = u16::try_from(tables.len()).unwrap_or(u16::MAX);
    let mut entry_selector = 0u16;
    while (1u32 << (entry_selector + 1)) <= u32::from(n) {
        entry_selector += 1;
    }
    let search_range = (1u16 << entry_selector).saturating_mul(16);
    let mut out = Vec::new();
    out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&n.saturating_mul(16).saturating_sub(search_range).to_be_bytes());
    let mut offset = 12 + tables.len() * 16;
    let mut body = Vec::new();
    let mut head_at = None;
    for (tag, data) in &tables {
        out.extend_from_slice(tag);
        out.extend_from_slice(&checksum(data).to_be_bytes());
        out.extend_from_slice(&u32::try_from(offset).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
        if tag == b"head" {
            head_at = Some(offset);
        }
        body.extend_from_slice(data);
        pad4(&mut body);
        offset = 12 + tables.len() * 16 + body.len();
    }
    out.extend_from_slice(&body);
    if let Some(at) = head_at {
        let adjust = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
        if let Some(slot) = out.get_mut(at + 8..at + 12) {
            slot.copy_from_slice(&adjust.to_be_bytes());
        }
    }
    out
}

// ---- Content -------------------------------------------------------------------------------

/// The hex string of glyph codes for a content stream (`<0001 0002 …>`).
pub fn hex_codes(codes: &[u16]) -> String {
    let mut s = String::with_capacity(2 + codes.len() * 4);
    s.push('<');
    for c in codes {
        s.push_str(&format!("{c:04X}"));
    }
    s.push('>');
    s
}

/// A `/Span << /ActualText … >> BDC` operator for `text` (UTF-16BE with BOM, hex).
pub fn actual_text_bdc(text: &str) -> String {
    format!("/Span <</ActualText <FEFF{}>>> BDC", utf16_hex(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shaping::tests::face_or_skip;

    #[test]
    fn cmap_maps_codes_to_text_and_skips_empty() {
        let c = String::from_utf8(to_unicode_cmap(["ب", "لا", "", " "])).unwrap();
        assert!(c.contains("<0001> <0628>"));
        assert!(c.contains("<0002> <06440627>"));
        assert!(!c.contains("<0003>"));
        assert!(c.contains("<0004> <0020>"));
        assert!(c.contains("3 beginbfchar"));
        // 100 entries per block at most.
        let many: Vec<String> = (0..250).map(|i| char::from_u32(0x0627 + (i % 20)).unwrap().to_string()).collect();
        let c = String::from_utf8(to_unicode_cmap(many.iter().map(String::as_str))).unwrap();
        assert_eq!(c.matches("beginbfchar").count(), 3);
        assert!(c.contains("50 beginbfchar"));
    }

    #[test]
    fn same_cluster_same_code() {
        let Some(face) = face_or_skip() else { return };
        let mut s = FontSubset::new(face);
        let a = face.shape("بب", true).unwrap();
        let codes: Vec<u16> = a.iter().map(|c| s.code(c).unwrap()).collect();
        assert_eq!(codes.len(), 2);
        assert_ne!(codes[0], codes[1], "final and initial forms are different codes");
        let again: Vec<u16> = face.shape("بب", true).unwrap().iter().map(|c| s.code(c).unwrap()).collect();
        assert_eq!(codes, again);
        assert_eq!(s.len(), 2);
        assert_eq!(s.text(codes[0]), Some("ب"));
        assert!(s.width(codes[0]) > 0.0);
        assert_eq!(s.text(0), None);
        assert_eq!(s.tag().len(), 6);
        assert!(s.tag().chars().all(|c| c.is_ascii_uppercase()));
    }

    #[test]
    fn subset_is_a_valid_truetype_font_with_one_glyph_per_code() {
        let Some(face) = face_or_skip() else { return };
        let mut s = FontSubset::new(face);
        let mut codes = Vec::new();
        for c in face.shape("بِسْمِ اللهِ الرَّحْمٰنِ لا 123 ABC", true).unwrap() {
            codes.push(s.code(&c).unwrap());
        }
        let program = s.font_program().unwrap();
        // skrifa reads it back, and glyph `code` has the cluster's advance.
        let font = skrifa::FontRef::new(&program).unwrap();
        use skrifa::MetadataProvider;
        let metrics = font.glyph_metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default());
        for code in &codes {
            let w = metrics.advance_width(skrifa::GlyphId::new(u32::from(*code))).unwrap();
            assert!((f64::from(w) - s.width(*code) * f64::from(face.units_per_em()) / 1000.0).abs() < 0.5);
        }
        // Every cluster glyph has an outline except spaces, and outlines can be drawn.
        let outlines = font.outline_glyphs();
        for code in &codes {
            let text = s.text(*code).unwrap();
            let g = outlines.get(skrifa::GlyphId::new(u32::from(*code)));
            if text.trim().is_empty() {
                continue;
            }
            let g = g.unwrap_or_else(|| panic!("no outline for {text:?}"));
            let mut pen = Count(0);
            g.draw(skrifa::instance::Size::unscaled(), &mut pen).unwrap();
            assert!(pen.0 > 0, "{text:?} draws nothing");
        }
        // Checksums: the whole file sums to 0xB1B0AFBA.
        assert_eq!(checksum(&program), 0xB1B0_AFBA);
        // No variation or shaping tables.
        let t = tables(&program).unwrap();
        for tag in [b"fvar", b"gvar", b"GSUB", b"GPOS", b"GDEF", b"HVAR"] {
            assert!(!t.contains_key(tag), "{}", String::from_utf8_lossy(tag));
        }
        // Much smaller than the face.
        assert!(program.len() < face.bytes().len() / 4, "{} vs {}", program.len(), face.bytes().len());
    }

    struct Count(usize);
    impl skrifa::outline::OutlinePen for Count {
        fn move_to(&mut self, _: f32, _: f32) {
            self.0 += 1;
        }
        fn line_to(&mut self, _: f32, _: f32) {
            self.0 += 1;
        }
        fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {
            self.0 += 1;
        }
        fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {
            self.0 += 1;
        }
        fn close(&mut self) {}
    }

    #[test]
    fn writes_type0_identity_h_font_objects() {
        let Some(face) = face_or_skip() else { return };
        let mut s = FontSubset::new(face);
        for c in face.shape("سلام", true).unwrap() {
            s.code(&c).unwrap();
        }
        let mut doc = Document::new_empty();
        let font = s.write(&mut doc).unwrap();
        let f = doc.get(font);
        let d = f.as_dict().unwrap();
        assert_eq!(d.name(b"Subtype"), Some(&b"Type0"[..]));
        assert_eq!(d.name(b"Encoding"), Some(&b"Identity-H"[..]));
        assert!(d.name(b"BaseFont").unwrap().ends_with(b"+NotoSansArabic-Regular"));
        let desc = d.get(b"DescendantFonts").and_then(|a| a.as_array()).and_then(|a| a.first()).and_then(Object::as_ref).unwrap();
        let cid = doc.get(desc);
        let cid = cid.as_dict().unwrap();
        assert_eq!(cid.name(b"Subtype"), Some(&b"CIDFontType2"[..]));
        assert_eq!(cid.name(b"CIDToGIDMap"), Some(&b"Identity"[..]));
        let fd = doc.get(cid.reference(b"FontDescriptor").unwrap());
        assert!(fd.as_dict().unwrap().reference(b"FontFile2").is_some());
        let tu = doc.get(d.reference(b"ToUnicode").unwrap());
        let Object::Stream(tu) = &*tu else { panic!("ToUnicode is a stream") };
        let cmap = String::from_utf8(tu.decoded().unwrap()).unwrap();
        assert!(cmap.contains("<0645>") && cmap.contains("<0633>"), "{cmap}");
    }

    #[test]
    fn content_helpers() {
        assert_eq!(hex_codes(&[1, 0x1234]), "<00011234>");
        assert_eq!(actual_text_bdc("لا"), "/Span <</ActualText <FEFF06440627>>> BDC");
    }

    #[test]
    fn hostile_font_data_is_an_error_not_a_panic() {
        for bytes in [&b""[..], b"\x00\x01\x00\x00\x00\x05", &[0xFFu8; 200]] {
            assert!(build_font(bytes, &[]).is_err());
            assert!(FaceMetrics::read(bytes).is_err());
        }
        assert!(components(&[0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x20]).is_err());
    }
}
