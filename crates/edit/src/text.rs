//! Edit a PDF ▸ Edit text: the text lines already on a page, and replacing a line's text in place.
//!
//! A *line* is a run of text-showing operators inside one `BT … ET` on the same baseline, read
//! from one content stream. Replacing it rewrites only that stream (as a new object, since content
//! streams may be shared): the first operator of the line shows the new text, the others are
//! dropped, and every positioning operator stays, so the lines after it keep their places
//! (`Td`, `T*` and friends move from the line matrix, which showing text doesn't change).
//!
//! The new text uses the line's own font when every character has a code and a glyph in it;
//! otherwise it is set in Helvetica (WinAnsi), and the result says the font was substituted.
//! Reflowing a paragraph to a new width is not done here (see `edit.text-reflow`).

use std::collections::HashMap;
use std::rc::Rc;

use pdfcraft_content::{Matrix, Op, parse, serialize_ops};
use pdfcraft_cos::{Dict, Document, Object, PdfString, Stream};
use pdfcraft_fonts::pdf::Metrics;
use pdfcraft_fonts::{CraftFont, GlyphError, japanese_glyph_from};

use crate::EditError;

/// One line of existing text.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLine {
    /// The line's text as shown.
    pub text: String,
    /// Its box in user space.
    pub rect: [f64; 4],
    /// The font's resource name and `/BaseFont` (or Type 3 descriptor's `/FontName`), and its size in text space.
    pub font: String,
    pub base_font: String,
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    /// The current fill colour as RGB.
    pub color: [f64; 3],
    /// Whether new text in this font can only be shown by substituting another font (no
    /// Unicode mapping for the line, so nothing could be reused).
    pub decodable: bool,
    /// Whether the line reads right to left (its paragraph direction, per UAX #9; see
    /// `pdfcraft_fonts::layout::read_line`).
    pub rtl: bool,
    /// Whether the reading (`text` and `rtl`) follows from the drawing alone; `false` when UAX #9
    /// allows another text or direction to be drawn the same way and the line's position on the
    /// page (or the right-to-left default) chose.
    pub direction_certain: bool,
    /// Each code's text and where it starts (user space), in content order.
    pub(crate) units: Vec<(String, f64)>,
    /// Codes the line shows, and how many have no Unicode meaning (dropped from `text`).
    pub(crate) codes: usize,
    pub(crate) unmapped: usize,
    stream: usize,
    ops: Vec<usize>,
    /// Where the line starts: text matrix (text space), the state there, and its `BT`.
    origin: Origin,
}

/// Text and graphics state captured for rewriting.
#[derive(Clone, Debug, PartialEq)]
struct TextState {
    font: Option<(String, f64)>,
    char_spacing: f64,
    word_spacing: f64,
    scale: f64,
    leading: f64,
    rise: f64,
    fill: Vec<Op>,
}

impl TextState {
    fn of(ts: &Ts) -> Self {
        TextState {
            font: ts.font.as_ref().map(|(n, _)| (String::from_utf8_lossy(n).into_owned(), ts.size)),
            char_spacing: ts.char_spacing,
            word_spacing: ts.word_spacing,
            scale: ts.scale,
            leading: ts.leading,
            rise: ts.rise,
            fill: ts.fill.clone(),
        }
    }

    /// Operators that set this state (inside a text object).
    fn ops(&self) -> Vec<Op> {
        let n = pdfcraft_content::num;
        let mut v = Vec::new();
        if let Some((f, size)) = &self.font {
            v.push(Op::new("Tf", vec![Object::name(f), n(*size)]));
        }
        v.push(Op::new("Tc", vec![n(self.char_spacing)]));
        v.push(Op::new("Tw", vec![n(self.word_spacing)]));
        v.push(Op::new("Tz", vec![n(self.scale * 100.0)]));
        v.push(Op::new("TL", vec![n(self.leading)]));
        v.push(Op::new("Ts", vec![n(self.rise)]));
        v.extend(self.fill.iter().cloned());
        v
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Origin {
    tm: [f64; 6],
    /// The text line matrix there (where the next line's Td/T* starts from).
    tlm: [f64; 6],
    /// Text-space units → user space (the scale of the text rendering matrix's y axis).
    k: f64,
    /// The CTM there (current space → page space), for moving the paragraph in page space.
    ctm: [f64; 6],
    baseline: f64,
    x: f64,
    state: TextState,
    bt_op: usize,
    bt_state: TextState,
}

/// A paragraph: consecutive lines in one stream with the same font and size, aligned on the left
/// and regularly spaced.
#[derive(Clone, Debug, PartialEq)]
pub struct TextBlock {
    /// The lines' text joined with spaces (lines that ended in a hyphen are joined directly).
    pub text: String,
    pub rect: [f64; 4],
    pub base_font: String,
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    /// The text fill colour as RGB, used by the visual editor and preserved when no colour
    /// override is requested.
    pub color: [f64; 3],
    /// Whether the paragraph reads right to left (its first line's direction, per UAX #9).
    pub rtl: bool,
    /// Indexes into [`text_lines`].
    pub lines: Vec<usize>,
}

impl TextLine {
    /// The baseline's height in user space.
    pub fn origin_baseline(&self) -> f64 {
        self.origin.baseline
    }
}

/// What replacing a line did.
#[derive(Clone, Debug, PartialEq)]
pub struct LineEdit {
    /// `Some(font)` when the line's font couldn't show the new text and Helvetica was used.
    pub substituted: Option<String>,
}

#[derive(Clone)]
struct Ts {
    ctm: Matrix,
    /// The operators that set the current fill colour (`g`, `rg`, `k`, or `cs` + `sc`/`scn`).
    fill: Vec<Op>,
    font: Option<(Vec<u8>, Rc<Metrics>)>,
    size: f64,
    char_spacing: f64,
    word_spacing: f64,
    scale: f64,
    leading: f64,
    rise: f64,
}

fn content_streams(doc: &Document, page: &Dict) -> Vec<(Object, Vec<u8>)> {
    let list: Vec<Object> = match page.get(b"Contents") {
        None => Vec::new(),
        Some(c) => match &*doc.resolve(c) {
            Object::Array(a) => a.clone(),
            _ => vec![c.clone()],
        },
    };
    list.into_iter()
        .map(|o| {
            let data = match &*doc.resolve(&o) {
                Object::Stream(s) => s.decoded().unwrap_or_default(),
                _ => Vec::new(),
            };
            (o, data)
        })
        .collect()
}

/// Rewrite one of a page's content streams: `edit(i)` gives the operators to insert before
/// operator `i` of `ops` (parsed from `data`) and whether to keep it. Everything else is copied
/// byte for byte. A page's streams are one stream in pieces, split between any two tokens
/// (ISO 32000-2 §7.8.2), so a piece can end with operands whose operator starts the next one,
/// or start by closing a dictionary the previous one opened; those tokens belong to no operator
/// parsed here and must stay where they are.
fn splice(data: &[u8], ops: &[Op], mut edit: impl FnMut(usize) -> (Vec<Op>, bool)) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut at = 0;
    for (i, op) in ops.iter().enumerate() {
        let (insert, keep) = edit(i);
        if insert.is_empty() && keep {
            continue;
        }
        let start = op.span.start.clamp(at, data.len());
        out.extend_from_slice(data.get(at..start).unwrap_or_default());
        if !insert.is_empty() {
            if out.last().is_some_and(|b| !b.is_ascii_whitespace()) {
                out.push(b'\n');
            }
            out.extend_from_slice(&serialize_ops(&insert));
        }
        at = if keep { start } else { op.span.end.clamp(start, data.len()) };
    }
    out.extend_from_slice(data.get(at..).unwrap_or_default());
    out
}

fn page_dict(doc: &Document, page: usize) -> Result<pdfcraft_model::Page, EditError> {
    pdfcraft_model::pages(doc).into_iter().nth(page).ok_or(EditError::NoSuchPage(page))
}

fn fill_color(fill: &[Op]) -> [f64; 3] {
    let Some(op) = fill.iter().rev().find(|op| matches!(op.op.as_slice(), b"g" | b"rg" | b"k")) else {
        return [0.0, 0.0, 0.0];
    };
    match op.op.as_slice() {
        b"g" => {
            let v = op.operands.first().and_then(Object::as_f64).unwrap_or(0.0).clamp(0.0, 1.0);
            [v, v, v]
        }
        b"rg" => [
            op.operands.first().and_then(Object::as_f64).unwrap_or(0.0).clamp(0.0, 1.0),
            op.operands.get(1).and_then(Object::as_f64).unwrap_or(0.0).clamp(0.0, 1.0),
            op.operands.get(2).and_then(Object::as_f64).unwrap_or(0.0).clamp(0.0, 1.0),
        ],
        b"k" => {
            let c = op.operands.first().and_then(Object::as_f64).unwrap_or(0.0);
            let m = op.operands.get(1).and_then(Object::as_f64).unwrap_or(0.0);
            let y = op.operands.get(2).and_then(Object::as_f64).unwrap_or(0.0);
            let k = op.operands.get(3).and_then(Object::as_f64).unwrap_or(0.0);
            [(1.0 - c) * (1.0 - k), (1.0 - m) * (1.0 - k), (1.0 - y) * (1.0 - k)]
        }
        _ => [0.0, 0.0, 0.0],
    }
}

/// The text-showing operators of one stream with their text, font, box and baseline.
struct Shown {
    op: usize,
    /// The text matrix where it starts (text space), and the state there and at its `BT`.
    tm: Matrix,
    tlm: Matrix,
    state: Ts,
    bt_op: usize,
    bt_state: Ts,
    text: String,
    /// Each code's text with where it starts along the baseline (user space), in content order.
    pub(crate) units: Vec<(String, f64)>,
    rect: [f64; 4],
    baseline: f64,
    start_x: f64,
    end_x: f64,
    bt: usize,
    font: Vec<u8>,
    base_font: String,
    size: f64,
    bold: bool,
    italic: bool,
    decodable: bool,
    /// Codes shown, and how many of them have no Unicode meaning in the font.
    codes: usize,
    unmapped: usize,
}

/// The graphics state carried from one of a page's content streams to the next: the streams
/// are one stream in pieces (§7.8.2), so a `cm` (AutoCAD scales the whole page in the first
/// stream), an unbalanced `q`, the font and the colour still apply in the streams after it.
struct Carry {
    ts: Ts,
    stack: Vec<Ts>,
}

impl Carry {
    fn new() -> Self {
        let ts = Ts {
            ctm: Matrix::IDENTITY,
            fill: Vec::new(),
            font: None,
            size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            scale: 1.0,
            leading: 0.0,
            rise: 0.0,
        };
        Carry { ts, stack: Vec::new() }
    }
}

/// A `Do` met while interpreting: its operator index, XObject name and the state there.
type Invocation = (usize, Vec<u8>, Ts);

fn interpret(
    doc: &Document,
    ops: &[Op],
    fonts_res: &Dict,
    cache: &mut HashMap<Vec<u8>, Rc<Metrics>>,
    carry: &mut Carry,
    mut invoked: Option<&mut Vec<Invocation>>,
) -> Vec<Shown> {
    let mut out = Vec::new();
    let mut ts = carry.ts.clone();
    let mut at_bt = (0usize, ts.clone());
    let mut stack: Vec<Ts> = std::mem::take(&mut carry.stack);
    let (mut tm, mut tlm) = (Matrix::IDENTITY, Matrix::IDENTITY);
    let mut bt = 0usize;
    for (i, op) in ops.iter().enumerate() {
        match op.op.as_slice() {
            b"q" => stack.push(ts.clone()),
            b"Q" => {
                if let Some(s) = stack.pop() {
                    ts = s;
                }
            }
            b"cm" => {
                if let Some(m) = op.nums::<6>() {
                    ts.ctm = Matrix(m).then(&ts.ctm);
                }
            }
            b"BT" => {
                tm = Matrix::IDENTITY;
                tlm = Matrix::IDENTITY;
                bt += 1;
                at_bt = (i, ts.clone());
            }
            b"g" | b"rg" | b"k" | b"cs" => ts.fill = vec![op.clone()],
            b"sc" | b"scn" => {
                ts.fill.retain(|o| o.is("cs"));
                ts.fill.push(op.clone());
            }
            b"Tf" => {
                ts.size = op.num(1).unwrap_or(ts.size);
                if let Some(name) = op.name(0) {
                    let m = cache.entry(name.to_vec()).or_insert_with(|| {
                        Rc::new(
                            fonts_res
                                .get(name)
                                .and_then(|f| doc.resolve(f).as_dict().cloned())
                                .map(|d| Metrics::from_dict(doc, &d))
                                .unwrap_or_else(Metrics::fallback),
                        )
                    });
                    ts.font = Some((name.to_vec(), m.clone()));
                }
            }
            b"Tc" => ts.char_spacing = op.num(0).unwrap_or(0.0),
            b"Tw" => ts.word_spacing = op.num(0).unwrap_or(0.0),
            b"Tz" => ts.scale = op.num(0).unwrap_or(100.0) / 100.0,
            b"TL" => ts.leading = op.num(0).unwrap_or(0.0),
            b"Ts" => ts.rise = op.num(0).unwrap_or(0.0),
            b"Do" => {
                if let (Some(list), Some(name)) = (invoked.as_deref_mut(), op.name(0)) {
                    list.push((i, name.to_vec(), ts.clone()));
                }
            }
            b"Td" | b"TD" => {
                if let Some([x, y]) = op.nums::<2>() {
                    if op.is("TD") {
                        ts.leading = -y;
                    }
                    tlm = Matrix([1.0, 0.0, 0.0, 1.0, x, y]).then(&tlm);
                    tm = tlm;
                }
            }
            b"Tm" => {
                if let Some(m) = op.nums::<6>() {
                    tlm = Matrix(m);
                    tm = tlm;
                }
            }
            b"T*" => {
                tlm = Matrix([1.0, 0.0, 0.0, 1.0, 0.0, -ts.leading]).then(&tlm);
                tm = tlm;
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                if matches!(op.op.as_slice(), b"'" | b"\"") {
                    if op.is("\"") {
                        ts.word_spacing = op.num(0).unwrap_or(ts.word_spacing);
                        ts.char_spacing = op.num(1).unwrap_or(ts.char_spacing);
                    }
                    tlm = Matrix([1.0, 0.0, 0.0, 1.0, 0.0, -ts.leading]).then(&tlm);
                    tm = tlm;
                }
                let Some((name, m)) = ts.font.clone() else { continue };
                // Pieces: strings, and TJ number adjustments.
                let pieces: Vec<Object> = match op.op.as_slice() {
                    b"TJ" => op.operands.first().and_then(Object::as_array).cloned().unwrap_or_default(),
                    _ => op.operands.last().cloned().into_iter().collect(),
                };
                let trm0 = tm.then(&ts.ctm);
                let start = trm0.apply(0.0, ts.rise);
                let mut text = String::new();
                let mut units: Vec<(String, f64)> = Vec::new();
                let mut decodable = true;
                let (mut n_codes, mut n_unmapped) = (0usize, 0usize);
                let mut x_text = 0.0;
                for p in &pieces {
                    match p {
                        Object::String(s) => {
                            let codes = m.codes(&s.bytes);
                            let just: Vec<u32> = codes.iter().map(|c| c.0).collect();
                            // Codes still to read as part of a glyph sequence already read.
                            let mut covered = 0usize;
                            for (k, &(code, len)) in codes.iter().enumerate() {
                                n_codes = n_codes.saturating_add(1);
                                if covered > 0 {
                                    covered -= 1;
                                } else if let Some((t, n)) = m.sequence_text(just.get(k..).unwrap_or_default()) {
                                    // A letter the font draws as several glyphs (a base and its dots).
                                    text.push_str(t);
                                    units.push((t.to_string(), trm0.apply(x_text, ts.rise).0));
                                    covered = n.saturating_sub(1);
                                } else {
                                    match m.text_of(code) {
                                        Some(t) => {
                                            text.push_str(t);
                                            units.push((t.to_string(), trm0.apply(x_text, ts.rise).0));
                                        }
                                        None => {
                                            decodable = false;
                                            n_unmapped = n_unmapped.saturating_add(1);
                                        }
                                    }
                                }
                                let w = m.width(code) * ts.size + ts.char_spacing + if m.is_space(code, len) { ts.word_spacing } else { 0.0 };
                                x_text += w * ts.scale;
                            }
                        }
                        other => {
                            if let Some(n) = other.as_f64() {
                                let dx = -n / 1000.0 * ts.size * ts.scale;
                                // A large gap inside TJ reads as a space.
                                if dx > ts.size * 0.2 && !text.ends_with(' ') {
                                    text.push(' ');
                                    units.push((" ".to_string(), trm0.apply(x_text, ts.rise).0));
                                }
                                x_text += dx;
                            }
                        }
                    }
                }
                tm = Matrix([1.0, 0.0, 0.0, 1.0, x_text, 0.0]).then(&tm);
                let end = tm.then(&ts.ctm).apply(0.0, ts.rise);
                // The box: from descent to ascent along the run.
                let corners = [
                    trm0.apply(0.0, ts.rise + m.descent * ts.size),
                    trm0.apply(x_text, ts.rise + m.descent * ts.size),
                    trm0.apply(0.0, ts.rise + m.ascent * ts.size),
                    trm0.apply(x_text, ts.rise + m.ascent * ts.size),
                ];
                let rect = corners
                    .iter()
                    .fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |b, p| [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)]);
                let size_user = (trm0.0[2].powi(2) + trm0.0[3].powi(2)).sqrt() * ts.size;
                out.push(Shown {
                    op: i,
                    tm: Matrix([tm.0[0], tm.0[1], tm.0[2], tm.0[3], tm.0[4] - x_text * tm.0[0], tm.0[5] - x_text * tm.0[1]]),
                    state: ts.clone(),
                    tlm,
                    bt_op: at_bt.0,
                    bt_state: at_bt.1.clone(),
                    text,
                    units,
                    rect,
                    baseline: start.1,
                    start_x: start.0,
                    end_x: end.0,
                    bt,
                    font: name,
                    base_font: m.base_font.clone(),
                    size: if size_user > 0.0 { size_user } else { ts.size },
                    bold: m.bold,
                    italic: m.italic,
                    decodable,
                    codes: n_codes,
                    unmapped: n_unmapped,
                });
            }
            _ => {}
        }
    }
    carry.ts = ts;
    carry.stack = stack;
    out
}

/// The lines of text on a page (0-based), in content order.
pub fn text_lines(doc: &Document, page: usize) -> Result<Vec<TextLine>, EditError> {
    let p = page_dict(doc, page)?;
    let res = p.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let fonts_res = res.get(b"Font").map(|f| doc.resolve(f)).and_then(|f| f.as_dict().cloned()).unwrap_or_default();
    let mut cache = HashMap::new();
    let mut lines: Vec<TextLine> = Vec::new();
    let mut carry = Carry::new();
    for (si, (_, data)) in content_streams(doc, &p.dict).into_iter().enumerate() {
        let ops = parse(&data).ops;
        let mut last = None;
        for s in interpret(doc, &ops, &fonts_res, &mut cache, &mut carry, None) {
            add_shown(&mut lines, &mut last, s, si);
        }
    }
    // Lines of only spaces aren't editable text.
    lines.retain(|l| !l.text.trim().is_empty());
    let crop = p.crop(doc);
    for l in &mut lines {
        logical_order(l, crop);
    }
    Ok(lines)
}

/// Right-to-left text is drawn in visual order: put a line's text in reading (logical) order,
/// from its codes' positions along the baseline, per UAX #9 (`read_line`). Where both
/// directions show the line the same way, its place on the page (`crop`) decides: nearer the
/// right edge reads right to left. Lines without right-to-left letters are left as they are.
fn logical_order(l: &mut TextLine, crop: [f64; 4]) {
    if !l.units.iter().any(|(t, _)| t.chars().any(pdfcraft_fonts::shaping::is_rtl_char)) {
        return;
    }
    let mut units = l.units.clone();
    // Drawing order, left to right (stable: codes drawn at one place keep their order).
    units.sort_by(|a, b| a.1.total_cmp(&b.1));
    let (left_gap, right_gap) = (l.rect[0] - crop[0], crop[2] - l.rect[2]);
    let hint = if (left_gap - right_gap).abs() > l.size { Some(right_gap < left_gap) } else { None };
    let texts: Vec<&str> = units.iter().map(|(t, _)| t.as_str()).collect();
    let reading = pdfcraft_fonts::layout::read_line(&texts, None, hint);
    l.rtl = reading.rtl;
    l.direction_certain = !reading.ambiguous;
    l.text = reading.order.iter().filter_map(|i| texts.get(*i).copied()).collect();
}

/// The paragraph direction an edited line keeps: the one it was read in. The line's text, laid
/// out again in that direction, is drawn as it was, so what the user leaves alone stays where
/// it was even when the direction came from the line's position (`direction_certain` false).
fn known_direction(line: &TextLine) -> pdfcraft_fonts::layout::BaseDirection {
    use pdfcraft_fonts::layout::BaseDirection;
    if line.rtl { BaseDirection::Rtl } else { BaseDirection::Ltr }
}

/// Whether `text` must be shaped and laid out right to left by PdfCraft rather than encoded in
/// an existing font: it has right-to-left letters (Arabic, Hebrew). Reusing the line's font code
/// by code would give each letter one fixed form in typed order, i.e. disjoined and mirrored.
fn needs_shaping(text: &str) -> bool {
    text.chars().any(pdfcraft_fonts::shaping::is_rtl_char)
}

/// Operators that show `text` shaped (joined, right to left where it is) in an embedded subset
/// of the Arabic face, in place of a line's first text operator, at the line's text-space font
/// size `size` (`k`: text space → user space). Character and word spacing and horizontal
/// scaling are set to neutral around it (spacing letters apart would break their joins) and put
/// back after; a right-to-left line keeps its right edge. Returns the operators, the font's
/// resource name and a description of the font used.
fn shaped_line_ops(
    doc: &mut Document,
    fonts_res: &mut Dict,
    text: &str,
    size: f64,
    k: f64,
    target: &TextLine,
) -> Result<(Vec<Op>, String, String), EditError> {
    let err = |e: pdfcraft_fonts::paint::TextError| EditError::Invalid(format!("\"{text}\" can't be shown: {e}"));
    let mut u = pdfcraft_fonts::paint::UnicodeLines::new().map_err(err)?;
    let user_size = size * k;
    let (line, width) = u.line_in(text, user_size, known_direction(target)).map_err(err)?;
    let mut codes: Vec<u16> = Vec::new();
    for c in line.runs.iter().flat_map(|r| r.clusters.iter()) {
        codes.push(u.code(c).map_err(err)?);
    }
    let bytes: Vec<u8> = codes.iter().flat_map(|c| c.to_be_bytes()).collect();
    let font = u.write(doc).map_err(err)?;
    let mut name = String::from("PCUni");
    let mut suffix = 0u32;
    while fonts_res.contains(name.as_bytes()) {
        suffix = suffix.saturating_add(1);
        name = format!("PCUni{suffix}");
    }
    fonts_res.set(name.clone().into_bytes(), Object::Ref(font));
    let n = pdfcraft_content::num;
    let st = &target.origin.state;
    // Right edge kept for right-to-left lines: start further right (or left) by the difference.
    let shift = if line.rtl { ((target.rect[2] - target.rect[0]) - width) / k } else { 0.0 };
    let mut out = Vec::new();
    if shift != 0.0 && shift.is_finite() {
        out.push(Op::new("Td", vec![n(shift), n(0.0)]));
    }
    out.push(Op::new("Tc", vec![n(0.0)]));
    out.push(Op::new("Tw", vec![n(0.0)]));
    out.push(Op::new("Tz", vec![n(100.0)]));
    out.push(Op::new("Tf", vec![Object::name(&name), n(size)]));
    out.push(Op::new("Tj", vec![Object::String(PdfString { bytes, hex: true })]));
    out.push(Op::new("Tf", vec![Object::name(&target.font), n(size)]));
    out.push(Op::new("Tc", vec![n(st.char_spacing)]));
    out.push(Op::new("Tw", vec![n(st.word_spacing)]));
    out.push(Op::new("Tz", vec![n(st.scale * 100.0)]));
    if shift != 0.0 && shift.is_finite() {
        out.push(Op::new("Td", vec![n(-shift), n(0.0)]));
    }
    let family = pdfcraft_fonts::shaping::ShapingFace::arabic().map_or_else(|| "Arabic".to_string(), |f| f.name().to_string());
    Ok((out, name, family))
}

/// The line `s` continues, or a new one: `last` is the previous run of the same stream
/// (`bt`, baseline, end x, size).
fn add_shown(lines: &mut Vec<TextLine>, last: &mut Option<(usize, f64, f64, f64)>, s: Shown, stream: usize) {
    let joins = last.is_some_and(|(bt, base, end, size)| {
        bt == s.bt
            && lines.last().is_some_and(|l| l.font.as_bytes() == s.font.as_slice())
            && lines
                .last()
                .is_some_and(|l| (s.size - l.size).abs() < 0.01 && s.bold == l.bold && s.italic == l.italic && fill_color(&s.state.fill) == l.color)
            && (s.baseline - base).abs() < size * 0.3
            && s.start_x > end - size
            && s.start_x - end < size * 3.0
    });
    if joins && let Some(l) = lines.last_mut() {
        let gap = s.start_x - last.map_or(s.start_x, |x| x.2);
        if gap > l.size * 0.2 && !l.text.ends_with(' ') && !s.text.starts_with(' ') {
            l.text.push(' ');
            l.units.push((" ".to_string(), last.map_or(s.start_x, |x| x.2)));
        }
        l.text.push_str(&s.text);
        l.units.extend(s.units.iter().cloned());
        l.rect = [l.rect[0].min(s.rect[0]), l.rect[1].min(s.rect[1]), l.rect[2].max(s.rect[2]), l.rect[3].max(s.rect[3])];
        l.ops.push(s.op);
        l.decodable &= s.decodable;
        l.codes = l.codes.saturating_add(s.codes);
        l.unmapped = l.unmapped.saturating_add(s.unmapped);
        l.color = fill_color(&s.state.fill);
    } else {
        lines.push(TextLine {
            text: s.text.clone(),
            rect: s.rect,
            font: String::from_utf8_lossy(&s.font).into_owned(),
            base_font: s.base_font.clone(),
            size: s.size,
            bold: s.bold,
            italic: s.italic,
            color: fill_color(&s.state.fill),
            decodable: s.decodable,
            rtl: false,
            direction_certain: true,
            units: s.units.clone(),
            codes: s.codes,
            unmapped: s.unmapped,
            stream,
            ops: vec![s.op],
            origin: Origin {
                tm: s.tm.0,
                tlm: s.tlm.0,
                k: (s.tm.then(&s.state.ctm).0[2].powi(2) + s.tm.then(&s.state.ctm).0[3].powi(2)).sqrt(),
                ctm: s.state.ctm.0,
                baseline: s.baseline,
                x: s.start_x,
                state: TextState::of(&s.state),
                bt_op: s.bt_op,
                bt_state: TextState::of(&s.bt_state),
            },
        });
    }
    *last = Some((s.bt, s.baseline, s.end_x, s.size.max(1.0)));
}

/// How deep form XObjects nest before reading stops (they may also refer to themselves).
pub(crate) const MAX_FORM_DEPTH: usize = 12;

/// How many form XObjects one page may draw, all nesting levels together, before reading
/// stops: a form drawing another many times, a few levels deep, would otherwise multiply out.
pub const MAX_FORM_CALLS: usize = 4096;

/// A form XObject a page draws: its object, content, resources and its matrix composed with the
/// CTM at the `Do`.
pub(crate) struct FormCall {
    pub(crate) obj: Option<pdfcraft_cos::ObjRef>,
    pub(crate) data: Vec<u8>,
    pub(crate) resources: Dict,
    pub(crate) ctm: Matrix,
}

/// The form XObject `name` in `resources`, drawn with `ctm`. `None` for images, missing names
/// and forms already being read (`path`) or nested too deep.
pub(crate) fn form_call(doc: &Document, resources: &Dict, name: &[u8], ctm: Matrix, path: &[pdfcraft_cos::ObjRef]) -> Option<FormCall> {
    if path.len() >= MAX_FORM_DEPTH {
        return None;
    }
    let xobjects = resources.get(b"XObject").map(|x| doc.resolve(x)).and_then(|x| x.as_dict().cloned())?;
    let o = xobjects.get(name)?;
    let obj = o.as_ref();
    if obj.is_some_and(|r| path.contains(&r)) {
        return None;
    }
    let Object::Stream(s) = &*doc.resolve(o) else { return None };
    if s.dict.name(b"Subtype") != Some(b"Form") {
        return None;
    }
    let m = s.dict.get(b"Matrix").map(|m| doc.resolve(m)).and_then(|m| {
        let a = m.as_array()?;
        let v: Vec<f64> = a.iter().filter_map(|x| doc.resolve(x).as_f64()).collect();
        <[f64; 6]>::try_from(v).ok().filter(|v| v.iter().all(|x| x.is_finite()))
    });
    // A form without its own resources uses the ones it was drawn with (PDF 1.1 files).
    let own = s.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned());
    Some(FormCall {
        obj,
        data: s.decoded().unwrap_or_default(),
        resources: own.unwrap_or_else(|| resources.clone()),
        ctm: m.map_or(ctm, |m| Matrix(m).then(&ctm)),
    })
}

/// The lines a page shows as it reads, including text drawn by form XObjects (which
/// [`text_lines`] leaves out because it can only rewrite the page's own streams). Lines from a
/// form carry a stream number past the page's streams, one per form drawn.
fn reading_lines(doc: &Document, page: usize) -> Result<Vec<TextLine>, EditError> {
    reading_lines_with(doc, page, false)
}

/// [`reading_lines`]; `keep_empty` also keeps lines with no decoded text (every code unmapped),
/// unordered, for [`audit_page`].
fn reading_lines_with(doc: &Document, page: usize, keep_empty: bool) -> Result<Vec<TextLine>, EditError> {
    struct Walk<'a> {
        doc: &'a Document,
        lines: Vec<TextLine>,
        next_stream: usize,
        path: Vec<pdfcraft_cos::ObjRef>,
        calls: usize,
    }
    fn walk(w: &mut Walk, ops: &[Op], resources: &Dict, carry: &mut Carry, stream: usize) {
        let fonts_res = resources.get(b"Font").map(|f| w.doc.resolve(f)).and_then(|f| f.as_dict().cloned()).unwrap_or_default();
        let mut cache = HashMap::new();
        let mut invoked = Vec::new();
        let shown = interpret(w.doc, ops, &fonts_res, &mut cache, carry, Some(&mut invoked));
        let mut last = None;
        let mut forms = invoked.into_iter().peekable();
        for s in shown {
            while let Some((_, name, ts)) = forms.next_if(|f| f.0 < s.op) {
                enter(w, resources, &name, ts);
                // Text after a form starts a new line.
                last = None;
            }
            add_shown(&mut w.lines, &mut last, s, stream);
        }
        for (_, name, ts) in forms {
            enter(w, resources, &name, ts);
        }
    }
    fn enter(w: &mut Walk, resources: &Dict, name: &[u8], ts: Ts) {
        if w.calls >= MAX_FORM_CALLS {
            return;
        }
        w.calls = w.calls.saturating_add(1);
        let Some(form) = form_call(w.doc, resources, name, ts.ctm, &w.path) else { return };
        let ops = parse(&form.data).ops;
        // The form starts with the graphics state at its `Do`, its own matrix applied.
        let mut carry = Carry { ts: Ts { ctm: form.ctm, ..ts }, stack: Vec::new() };
        let stream = w.next_stream;
        w.next_stream = w.next_stream.saturating_add(1);
        if let Some(r) = form.obj {
            w.path.push(r);
        }
        walk(w, &ops, &form.resources, &mut carry, stream);
        if form.obj.is_some() {
            w.path.pop();
        }
    }
    let p = page_dict(doc, page)?;
    let res = p.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let streams = content_streams(doc, &p.dict);
    let mut w = Walk { doc, lines: Vec::new(), next_stream: streams.len(), path: Vec::new(), calls: 0 };
    let mut carry = Carry::new();
    for (si, (_, data)) in streams.into_iter().enumerate() {
        walk(&mut w, &parse(&data).ops, &res, &mut carry, si);
    }
    let mut lines = w.lines;
    if keep_empty {
        return Ok(lines);
    }
    lines.retain(|l| !l.text.trim().is_empty());
    let crop = p.crop(doc);
    for l in &mut lines {
        logical_order(l, crop);
    }
    Ok(lines)
}

/// Text → the bytes that show it in the chosen font.
type Encoder = Box<dyn Fn(&str) -> Option<Vec<u8>>>;

fn is_win_ansi_char(c: char) -> bool {
    matches!(c, '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' | '€' | '‚' | '„' | '…' | '‘' | '’' | '“' | '”' | '•' | '–' | '—' | '™' | '\t')
}

fn needs_type3(text: &str) -> bool {
    text.chars().any(|c| !is_win_ansi_char(c))
}

fn source_family(base_font: &str) -> crate::added::Family {
    let name = base_font.to_ascii_lowercase();
    if ["courier", "mono", "consolas", "menlo", "monaco", "lucida console"].iter().any(|s| name.contains(s)) {
        crate::added::Family::Courier
    } else if !name.contains("sans")
        && ["times", "serif", "roman", "mincho", "cambria", "georgia", "palatino", "garamond"].iter().any(|s| name.contains(s))
    {
        crate::added::Family::Times
    } else {
        crate::added::Family::Helvetica
    }
}

const SUBSTITUTE_NAME: &str = "PCEdHelv";
const SUBSTITUTE: &[u8] = SUBSTITUTE_NAME.as_bytes();
const MAX_TYPE3_GLYPHS: usize = 240;

#[derive(Clone)]
struct Type3Fallback {
    name: String,
    /// The family its glyphs come from ("Shippori Mincho").
    family: &'static str,
    codes: Vec<(char, u8, f64)>,
}

fn pdf_num(v: f64) -> String {
    if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.4}").trim_end_matches('0').trim_end_matches('.').to_string() }
}

fn type3_path(face: &CraftFont, ch: char) -> Result<(Vec<u8>, f64), EditError> {
    let glyph = japanese_glyph_from(face, ch).map_err(|e| match e {
        GlyphError::NoFont => no_japanese_font(),
        GlyphError::Missing => EditError::Invalid(format!("Japanese fallback font has no glyph for U+{:04X}", ch as u32)),
        GlyphError::TooComplex => EditError::Invalid(format!("Japanese fallback glyph U+{:04X} is too complex", ch as u32)),
    })?;
    let scale = 1000.0;
    let mut out = format!("{} 0 0 0 0 1000 1000 d1\n", pdf_num(glyph.width * scale)).into_bytes();
    for contour in glyph.contours {
        let Some(first) = contour.first() else { continue };
        out.extend_from_slice(format!("{} {} m\n", pdf_num(first[0] * scale), pdf_num(first[1] * scale)).as_bytes());
        for p in contour.iter().skip(1) {
            out.extend_from_slice(format!("{} {} l\n", pdf_num(p[0] * scale), pdf_num(p[1] * scale)).as_bytes());
        }
        out.extend_from_slice(b"h\n");
    }
    out.extend_from_slice(b"f\n");
    Ok((out, glyph.width))
}

fn unicode_hex(ch: char) -> String {
    let mut units = [0u16; 2];
    let encoded = ch.encode_utf16(&mut units);
    encoded.iter().map(|u| format!("{u:04X}")).collect()
}

/// This build has no Japanese face to draw replacement text with.
fn no_japanese_font() -> EditError {
    EditError::Invalid(
        "this text needs PdfCraft's Japanese fallback font, which this build doesn't include \
         (official releases do; to build it in, set CRAFT_FONTS_DIR to a craft-fonts checkout)"
            .into(),
    )
}

fn type3_font(doc: &mut Document, fonts_res: &mut Dict, text: &str, family: crate::added::Family, bold: bool) -> Result<Type3Fallback, EditError> {
    let face = pdfcraft_fonts::document_japanese_font_for_style(family == crate::added::Family::Times, bold).ok_or_else(no_japanese_font)?;
    let family = face.family;
    let mut chars = Vec::new();
    for ch in text.chars() {
        if !chars.contains(&ch) {
            if chars.len() >= MAX_TYPE3_GLYPHS {
                return Err(EditError::Invalid("Japanese replacement has too many unique characters".into()));
            }
            chars.push(ch);
        }
    }
    if chars.is_empty() {
        return Err(EditError::Invalid("replacement text is empty".into()));
    }
    let mut codes = Vec::with_capacity(chars.len());
    let mut charprocs = Dict::new();
    let mut widths = Vec::with_capacity(chars.len());
    let mut differences = vec![Object::Int(1)];
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CMapType 2 def\n1 begincodespacerange\n<01> <FF>\nendcodespacerange\n",
    );
    cmap.push_str(&format!("{} beginbfchar\n", chars.len()));
    for (i, ch) in chars.into_iter().enumerate() {
        let code = u8::try_from(i + 1).map_err(|_| EditError::Invalid("Japanese replacement has too many unique characters".into()))?;
        let glyph_name = format!("g{code:02X}");
        let (path, width) = type3_path(face, ch)?;
        let mut pd = Dict::new();
        pd.set(b"Length".to_vec(), path.len() as i64);
        let proc_ref = doc.add(Object::Stream(Stream::from_raw(pd, path)));
        charprocs.set(glyph_name.as_bytes().to_vec(), Object::Ref(proc_ref));
        differences.push(Object::name(&glyph_name));
        widths.push(Object::Real((width * 1000.0).round()));
        cmap.push_str(&format!("<{code:02X}> <{}>\n", unicode_hex(ch)));
        codes.push((ch, code, width));
    }
    cmap.push_str("endbfchar\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    let mut cmap_dict = Dict::new();
    cmap_dict.set(b"Length".to_vec(), cmap.len() as i64);
    let cmap_ref = doc.add(Object::Stream(Stream::from_raw(cmap_dict, cmap.into_bytes())));
    let mut encoding = Dict::new();
    encoding.set(b"Type".to_vec(), Object::name("Encoding"));
    encoding.set(b"Differences".to_vec(), Object::Array(differences));
    let mut font = Dict::new();
    font.set(b"Type".to_vec(), Object::name("Font"));
    font.set(b"Subtype".to_vec(), Object::name("Type3"));
    font.set(b"Name".to_vec(), Object::name("PCJapanese"));
    let mut descriptor = Dict::new();
    descriptor.set(b"Type".to_vec(), Object::name("FontDescriptor"));
    descriptor.set(b"FontName".to_vec(), Object::name(&format!("{}-{}", face.family, face.style).replace(' ', "")));
    descriptor.set(b"FontFamily".to_vec(), Object::String(PdfString::literal(face.family.as_bytes().to_vec())));
    descriptor.set(b"Flags".to_vec(), Object::Int(if face.family.contains("Mincho") { 6 } else { 4 }));
    descriptor.set(b"ItalicAngle".to_vec(), Object::Int(0));
    // PDF 1.7 tables 5.9 and 5.19: a Type 3 descriptor is indirect; Ascent/Descent may be omitted.
    font.set(b"FontDescriptor".to_vec(), Object::Ref(doc.add(Object::Dict(descriptor))));
    font.set(b"FontBBox".to_vec(), Object::Array(vec![Object::Int(0), Object::Int(-300), Object::Int(1000), Object::Int(1000)]));
    font.set(
        b"FontMatrix".to_vec(),
        Object::Array(vec![Object::Real(0.001), Object::Int(0), Object::Int(0), Object::Real(0.001), Object::Int(0), Object::Int(0)]),
    );
    font.set(b"FirstChar".to_vec(), Object::Int(1));
    font.set(b"LastChar".to_vec(), Object::Int(codes.len() as i64));
    font.set(b"Widths".to_vec(), Object::Array(widths));
    font.set(b"Encoding".to_vec(), Object::Dict(encoding));
    font.set(b"CharProcs".to_vec(), Object::Dict(charprocs));
    font.set(b"ToUnicode".to_vec(), Object::Ref(cmap_ref));
    let mut name = String::from("PCJp");
    let mut suffix = 0usize;
    while fonts_res.contains(name.as_bytes()) {
        suffix = suffix.saturating_add(1);
        name = format!("PCJp{suffix}");
    }
    fonts_res.set(name.as_bytes().to_vec(), Object::Dict(font));
    Ok(Type3Fallback { name, family, codes })
}

fn type3_encode(fallback: &Type3Fallback, text: &str) -> Option<Vec<u8>> {
    text.chars().map(|ch| fallback.codes.iter().find(|(c, _, _)| *c == ch).map(|(_, code, _)| *code)).collect()
}

/// Replace the text of line `line` (an index into [`text_lines`]) on `page` with `text`.
pub fn replace_line(doc: &mut Document, page: usize, line: usize, text: &str) -> Result<LineEdit, EditError> {
    crate::atomic(doc, |doc| replace_line_inner(doc, page, line, text))
}

fn replace_line_inner(doc: &mut Document, page: usize, line: usize, text: &str) -> Result<LineEdit, EditError> {
    let text = text.replace(['\n', '\r'], " ");
    let lines = text_lines(doc, page)?;
    let target = lines.get(line).cloned().ok_or_else(|| EditError::Invalid(format!("page {} has no line {}", page + 1, line + 1)))?;
    let p = page_dict(doc, page)?;
    let mut res = p.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let mut fonts_res = res.get(b"Font").map(|f| doc.resolve(f)).and_then(|f| f.as_dict().cloned()).unwrap_or_default();
    let streams = content_streams(doc, &p.dict);
    let (stream_obj, data) = streams.get(target.stream).cloned().ok_or_else(|| EditError::Invalid("the page's content changed".into()))?;
    let ops = parse(&data).ops;
    let first = target.ops[0];
    // The line's own font, when it can show every character.
    let font = fonts_res.get(target.font.as_bytes()).and_then(|f| doc.resolve(f).as_dict().cloned()).map(|d| Metrics::from_dict(doc, &d));
    // Right-to-left text is never re-encoded code by code in the line's font (see `needs_shaping`).
    let reused = if needs_shaping(&text) { None } else { font.as_ref().and_then(|m| m.encode(&text)) };
    let mut substituted = None;
    let mut replacement: Vec<Op> = Vec::new();
    // ' and " also move to the next line; keep that.
    match ops[first].op.as_slice() {
        b"'" => replacement.push(Op::new("T*", vec![])),
        b"\"" => {
            replacement.push(Op::new("Tw", vec![ops[first].operands.first().cloned().unwrap_or(Object::Int(0))]));
            replacement.push(Op::new("Tc", vec![ops[first].operands.get(1).cloned().unwrap_or(Object::Int(0))]));
            replacement.push(Op::new("T*", vec![]));
        }
        _ => {}
    }
    match reused {
        Some(bytes) => replacement.push(Op::new("Tj", vec![Object::String(PdfString::literal(bytes))])),
        None => {
            if needs_shaping(&text) {
                let size = font_size_before(&ops, first).unwrap_or(target.size);
                let k = target.origin.k.max(1e-6);
                let (ops_new, name, family) = shaped_line_ops(doc, &mut fonts_res, &text, size, k, &target)?;
                replacement.extend(ops_new);
                let _ = name;
                substituted = Some(family);
            } else if needs_type3(&text) {
                let fallback = type3_font(doc, &mut fonts_res, &text, source_family(&target.base_font), target.bold)?;
                let bytes = type3_encode(&fallback, &text)
                    .ok_or_else(|| EditError::Invalid(format!("\"{text}\" can't be shown by the Japanese fallback")))?;
                let size = font_size_before(&ops, first).unwrap_or(target.size);
                replacement.push(Op::new("Tf", vec![Object::name(&fallback.name), pdfcraft_content::num(size)]));
                replacement.push(Op::new("Tj", vec![Object::String(PdfString::literal(bytes))]));
                replacement.push(Op::new("Tf", vec![Object::name(&target.font), pdfcraft_content::num(size)]));
                substituted = Some(format!("{} Type3", fallback.family));
            } else {
                let win = pdfcraft_fonts::win_ansi(&text);
                // WinAnsi turns what it can't show into '?'; refuse rather than print the wrong thing.
                let back: String = win.iter().map(|b| char::from_u32(u32::from(*b)).unwrap_or('?')).collect();
                if text.chars().zip(back.chars()).any(|(a, b)| b == '?' && a != '?') {
                    return Err(EditError::Invalid(format!("\"{text}\" has characters neither {} nor Helvetica can show", target.base_font)));
                }
                // The size in text space: the current Tf's size.
                let size = font_size_before(&ops, first).unwrap_or(target.size);
                let family = source_family(&target.base_font);
                let base = family.base_font(target.bold, target.italic);
                let substitute_name = format!("PCEd{}", base.replace('-', ""));
                replacement.push(Op::new("Tf", vec![Object::name(&substitute_name), pdfcraft_content::num(size)]));
                replacement.push(Op::new("Tj", vec![Object::String(PdfString::literal(win))]));
                replacement.push(Op::new("Tf", vec![Object::name(&target.font), pdfcraft_content::num(size)]));
                substituted = Some(base.to_string());
                let mut f = Dict::new();
                f.set(b"Type".to_vec(), Object::name("Font"));
                f.set(b"Subtype".to_vec(), Object::name("Type1"));
                f.set(b"BaseFont".to_vec(), Object::name(base));
                f.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
                fonts_res.set(substitute_name.into_bytes(), Object::Dict(f));
            }
        }
    }
    // Rebuild: the line's first operator becomes the replacement, its others go. Copies of the
    // line drawn in the same area go too, so the replacement is all that shows.
    let drops = coincident_ops(&lines, std::slice::from_ref(&target.rect));
    let new_data = splice(&data, &ops, |i| {
        if i == first { (std::mem::take(&mut replacement), false) } else { (Vec::new(), !drops.get(&target.stream).is_some_and(|d| d.contains(&i))) }
    });
    let mut dict = match &*doc.resolve(&stream_obj) {
        Object::Stream(s) => s.dict.clone(),
        _ => Dict::new(),
    };
    dict.remove(b"Length");
    let new = doc.add(Object::Stream(Stream::flate(dict, &new_data)));
    let contents: Vec<Object> = streams.iter().enumerate().map(|(i, (o, _))| if i == target.stream { Object::Ref(new) } else { o.clone() }).collect();
    let page_ref = p.obj;
    if substituted.is_some() {
        res.set(b"Font".to_vec(), Object::Dict(fonts_res));
    }
    doc.update_dict(page_ref, |d| {
        d.set(b"Contents".to_vec(), if contents.len() == 1 { contents[0].clone() } else { Object::Array(contents) });
        if substituted.is_some() {
            d.set(b"Resources".to_vec(), Object::Dict(res));
        }
    })?;
    Ok(LineEdit { substituted })
}

/// The size of the last `Tf` before operator `at`.
fn font_size_before(ops: &[Op], at: usize) -> Option<f64> {
    ops[..at].iter().rev().find(|o| o.is("Tf")).and_then(|o| o.num(1))
}

/// The operators of every line drawn in the same area as one of `rects`, grouped by content
/// stream. Documents sometimes draw a line more than once (fake bold, an invisible text layer),
/// and a surviving copy would show the old text under the replaced line. Copies overlap a
/// member's rect by more than half of the smaller rect; neighbouring lines share no area.
fn coincident_ops(lines: &[TextLine], rects: &[[f64; 4]]) -> std::collections::HashMap<usize, std::collections::HashSet<usize>> {
    let area = |r: [f64; 4]| ((r[2] - r[0]) * (r[3] - r[1])).max(0.0);
    let mut drop = std::collections::HashMap::new();
    for l in lines {
        let covered = rects.iter().any(|m| {
            let ix = (m[2].min(l.rect[2]) - m[0].max(l.rect[0])).max(0.0);
            let iy = (m[3].min(l.rect[3]) - m[1].max(l.rect[1])).max(0.0);
            let small = area(*m).min(area(l.rect));
            small > 0.0 && ix * iy / small > 0.5
        });
        if covered {
            drop.entry(l.stream).or_insert_with(std::collections::HashSet::new).extend(l.ops.iter().copied());
        }
    }
    drop
}

/// The paragraphs on a page (0-based).
pub fn text_blocks(doc: &Document, page: usize) -> Result<Vec<TextBlock>, EditError> {
    let lines = text_lines(doc, page)?;
    Ok(group_blocks(&lines))
}

/// How the text of one font on a page decodes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FontAudit {
    /// `/BaseFont`, as the file names it.
    pub base_font: String,
    pub codes: usize,
    /// Codes with no Unicode meaning (no ToUnicode entry and no encoding): dropped from text.
    pub unmapped: usize,
}

/// The kinds of characters a page's text decodes to. Counts only: no text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CharCounts {
    pub arabic_letters: usize,
    pub hebrew_letters: usize,
    pub latin_letters: usize,
    pub digits: usize,
    /// Combining marks (Arabic harakat and the like).
    pub marks: usize,
    /// Arabic presentation forms (U+FB50–FDFF, U+FE70–FEFF): contextual glyph codes, not letters.
    pub presentation_forms: usize,
    /// Private-use code points (U+E000–F8FF and the supplementary planes 15–16).
    pub private_use: usize,
    /// U+FFFD replacement characters.
    pub replacement: usize,
    /// Control characters other than tab and newline.
    pub controls: usize,
    pub other: usize,
}

impl CharCounts {
    pub fn add(&mut self, text: &str) {
        for c in text.chars() {
            let n = u32::from(c);
            let slot = match n {
                0xFFFD => &mut self.replacement,
                0xE000..=0xF8FF | 0xF_0000..=0x10_FFFF => &mut self.private_use,
                0xFB50..=0xFDFF | 0xFE70..=0xFEFF => &mut self.presentation_forms,
                _ if unicode_bidi::bidi_class(c) == unicode_bidi::BidiClass::NSM => &mut self.marks,
                0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF if c.is_alphabetic() => &mut self.arabic_letters,
                0x0590..=0x05FF if c.is_alphabetic() => &mut self.hebrew_letters,
                _ if c.is_numeric() => &mut self.digits,
                _ if c.is_alphabetic() && n < 0x0250 => &mut self.latin_letters,
                _ if c.is_control() && c != '\t' && c != '\n' => &mut self.controls,
                _ => &mut self.other,
            };
            *slot = slot.saturating_add(1);
        }
    }
}

/// What a page's text decodes to, by font and by kind of character, as export reads it
/// (`reading_blocks`). It holds counts and font names, never the text itself, so it can be
/// shared to diagnose a confidential document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageAudit {
    pub codes: usize,
    pub unmapped: usize,
    pub chars: CharCounts,
    pub fonts: Vec<FontAudit>,
}

/// [`PageAudit`] for page `page` (0-based).
pub fn audit_page(doc: &Document, page: usize) -> Result<PageAudit, EditError> {
    let mut out = PageAudit::default();
    for l in reading_lines_with(doc, page, true)? {
        out.codes = out.codes.saturating_add(l.codes);
        out.unmapped = out.unmapped.saturating_add(l.unmapped);
        out.chars.add(&l.text);
        match out.fonts.iter_mut().find(|f| f.base_font == l.base_font) {
            Some(f) => {
                f.codes = f.codes.saturating_add(l.codes);
                f.unmapped = f.unmapped.saturating_add(l.unmapped);
            }
            None => out.fonts.push(FontAudit { base_font: l.base_font.clone(), codes: l.codes, unmapped: l.unmapped }),
        }
    }
    Ok(out)
}

/// The paragraphs on a page as a reader sees them, including text drawn by form XObjects
/// (Export to Word, HTML and RTF). Read-only: indexes into this list are not
/// [`text_blocks`] indexes, so they can't be passed to [`replace_block`] or [`rewrite_block`].
pub fn reading_blocks(doc: &Document, page: usize) -> Result<Vec<TextBlock>, EditError> {
    let lines = reading_lines(doc, page)?;
    Ok(group_blocks(&lines))
}

/// Lines that share a left edge, a centre or a right edge (left, centred or right-aligned text).
fn aligned(a: &TextLine, b: &TextLine) -> bool {
    let tol = b.size.max(1.0);
    let centre = |r: [f64; 4]| (r[0] + r[2]) / 2.0;
    (a.origin.x - b.origin.x).abs() < tol || (centre(a.rect) - centre(b.rect)).abs() < tol || (a.rect[2] - b.rect[2]).abs() < tol
}

fn group_blocks(lines: &[TextLine]) -> Vec<TextBlock> {
    let mut blocks: Vec<TextBlock> = Vec::new();
    let mut gap: Option<f64> = None;
    for (i, l) in lines.iter().enumerate() {
        let joins = i > 0
            && blocks.last().is_some_and(|b| {
                let Some(&p) = b.lines.last() else { return false };
                let prev = &lines[p];
                let g = prev.origin.baseline - l.origin.baseline;
                prev.stream == l.stream
                    && prev.font == l.font
                    && prev.base_font == l.base_font
                    && prev.bold == l.bold
                    && prev.italic == l.italic
                    && prev.color == l.color
                    && (prev.size - l.size).abs() < 0.01
                    && aligned(prev, l)
                    && g > l.size * 0.8
                    && g < l.size * 2.5
                    && gap.is_none_or(|first| (g - first).abs() < first * 0.2)
            });
        if joins
            && let Some(b) = blocks.last_mut()
            && let Some(&p) = b.lines.last()
        {
            let prev = &lines[p];
            gap.get_or_insert(prev.origin.baseline - l.origin.baseline);
            if b.text.ends_with('-') {
                b.text.pop();
            } else {
                b.text.push(' ');
            }
            b.text.push_str(l.text.trim_matches(paragraph_separator));
            b.rect = [b.rect[0].min(l.rect[0]), b.rect[1].min(l.rect[1]), b.rect[2].max(l.rect[2]), b.rect[3].max(l.rect[3])];
            b.lines.push(i);
        } else {
            gap = None;
            blocks.push(TextBlock {
                text: l.text.trim_matches(paragraph_separator).to_string(),
                rect: l.rect,
                base_font: l.base_font.clone(),
                size: l.size,
                bold: l.bold,
                italic: l.italic,
                color: l.color,
                rtl: l.rtl,
                lines: vec![i],
            });
        }
    }
    blocks
}

// An ideographic space carries intentional Japanese spacing, including paragraph indentation.
// Keep other whitespace normalization unchanged.
fn paragraph_separator(c: char) -> bool {
    c.is_whitespace() && c != '\u{3000}'
}

/// Greedy word wrapping to `width` with `advance` giving a string's width.
fn wrap(text: &str, width: f64, advance: impl Fn(&str) -> f64) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(paragraph_separator).filter(|word| !word.is_empty()) {
        let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if !line.is_empty() && advance(&candidate) > width {
            out.push(std::mem::take(&mut line));
            line = word.to_string();
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// Replace paragraph `block` (an index into [`text_blocks`]) with `text`, rewrapped to the
/// paragraph's width with its line spacing. The paragraph keeps its first line's position, font
/// (or Helvetica when the font can't show the text), size and colour.
pub fn replace_block(doc: &mut Document, page: usize, block: usize, text: &str) -> Result<LineEdit, EditError> {
    rewrite_block(doc, page, block, Some(text), &BlockStyle::default())
}

/// Formatting for a rewritten paragraph (`None` keeps the paragraph's own).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockStyle {
    /// A standard font family, bold, italic.
    pub family: Option<(crate::added::Family, bool, bool)>,
    /// Override weight without choosing a different family (`None` keeps the source weight).
    pub bold: Option<bool>,
    /// Font size in points (user space).
    pub size: Option<f64>,
    /// Fill colour (RGB 0–1).
    pub color: Option<[f64; 3]>,
    pub align: Option<crate::added::Align>,
    /// Draw a line under each line of text.
    pub underline: Option<bool>,
    /// Line spacing as a multiple of the font size (1.2 is ordinary).
    pub line_spacing: Option<f64>,
    /// Character spacing (points) and horizontal scaling (percent).
    pub char_spacing: Option<f64>,
    pub scale: Option<f64>,
    /// Move the paragraph by `[dx, dy]` in user space (dragging its box).
    pub offset: Option<[f64; 2]>,
    /// Rewrap to this width in user space (dragging the box's edge).
    pub width: Option<f64>,
}

/// Rewrite paragraph `block` with new text (or its own) and formatting, rewrapped to its width.
pub fn rewrite_block(doc: &mut Document, page: usize, block: usize, text: Option<&str>, style: &BlockStyle) -> Result<LineEdit, EditError> {
    crate::atomic(doc, |doc| rewrite_block_inner(doc, page, block, text, style))
}

fn rewrite_block_inner(doc: &mut Document, page: usize, block: usize, text: Option<&str>, style: &BlockStyle) -> Result<LineEdit, EditError> {
    let lines = text_lines(doc, page)?;
    let blocks = group_blocks(&lines);
    let b = blocks.get(block).cloned().ok_or_else(|| EditError::Invalid(format!("page {} has no paragraph {}", page + 1, block + 1)))?;
    let members: Vec<&TextLine> = b.lines.iter().map(|i| &lines[*i]).collect();
    let first = members[0];
    let p = page_dict(doc, page)?;
    let mut res = p.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let mut fonts_res = res.get(b"Font").map(|f| doc.resolve(f)).and_then(|f| f.as_dict().cloned()).unwrap_or_default();
    let streams = content_streams(doc, &p.dict);
    let (stream_obj, data) = streams.get(first.stream).cloned().ok_or_else(|| EditError::Invalid("the page's content changed".into()))?;
    let ops = parse(&data).ops;
    let text = text.unwrap_or(&b.text).split(paragraph_separator).filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ");
    let o = &first.origin;
    let (font_name, old_size) = o.state.font.clone().ok_or_else(|| EditError::Invalid("the paragraph has no font".into()))?;
    let k = o.k.max(1e-6);
    // The size in text space: points ÷ the text-to-user scale.
    let size = style.size.map_or(old_size, |pt| (pt / k).max(0.1));
    // Spacing and scaling (text state), with the chosen values.
    let mut ts_state = o.state.clone();
    if let Some(c) = style.char_spacing {
        ts_state.char_spacing = c / k;
    }
    if let Some(sc) = style.scale {
        ts_state.scale = (sc / 100.0).clamp(0.1, 10.0);
    }
    let o_state = ts_state.clone();
    let metrics = fonts_res.get(font_name.as_bytes()).and_then(|f| doc.resolve(f).as_dict().cloned()).map(|d| Metrics::from_dict(doc, &d));
    // Right-to-left text is shaped and laid out by PdfCraft in an embedded font (`needs_shaping`).
    let shaped = needs_shaping(&text);
    let reuse = !shaped && style.family.is_none() && style.bold.is_none() && metrics.as_ref().is_some_and(|m| m.encode(&text).is_some());
    // The chosen style also determines the real Japanese fallback outlines and advances.
    let (family, bold, italic) = style.family.unwrap_or((source_family(&b.base_font), b.bold, b.italic));
    let bold = style.bold.unwrap_or(bold);
    let type3 = if !reuse && !shaped && needs_type3(&text) { Some(type3_font(doc, &mut fonts_res, &text, family, bold)?) } else { None };
    let std_width = move |s: &str, size: f64| -> f64 {
        match family {
            crate::added::Family::Courier => s.chars().count() as f64 * 0.6 * size,
            crate::added::Family::Times => pdfcraft_fonts::helvetica_width(s, size) * 0.92,
            crate::added::Family::Helvetica => pdfcraft_fonts::helvetica_width(s, size) * if bold { 1.05 } else { 1.0 },
        }
    };
    let [dx, dy] = style.offset.unwrap_or([0.0, 0.0]);
    if !(dx.is_finite() && dy.is_finite()) {
        return Err(EditError::Invalid("the paragraph can't be moved that far".into()));
    }
    // A paragraph keeps its width (or takes the one asked for); a single line grows to the right,
    // up to the page's margin.
    let width = match style.width {
        // Capped at the largest page PDF allows (14 400 pt).
        Some(w) if w.is_finite() => w.clamp(size * k, 14_400.0),
        Some(_) => return Err(EditError::Invalid("the paragraph's width must be a number".into())),
        None if members.len() == 1 => (b.rect[2] - b.rect[0]).max(size * k).max(p.crop(doc)[2] - 36.0 - (b.rect[0] + dx)),
        None => (b.rect[2] - b.rect[0]).max(size * k),
    };
    let advance = |s: &str| -> f64 {
        let t = match (&metrics, reuse) {
            (Some(m), true) => m
                .encode(s)
                .map(|bytes| {
                    m.codes(&bytes)
                        .iter()
                        .map(|(c, l)| m.width(*c) * size + o_state.char_spacing + if m.is_space(*c, *l) { o_state.word_spacing } else { 0.0 })
                        .sum::<f64>()
                })
                .unwrap_or(0.0),
            _ if let Some(fallback) = &type3 => {
                fallback.codes.iter().map(|(ch, _, width)| s.chars().filter(|c| c == ch).count() as f64 * width * size).sum::<f64>()
                    + s.chars().count() as f64 * o_state.char_spacing
            }
            _ => std_width(s, size) + s.chars().count() as f64 * o_state.char_spacing,
        };
        t * o_state.scale * k
    };
    // Shaped paragraphs: laid out (bidi, joining, wrapping by shaped widths) in user space; each
    // line's codes, its start offset and width in text space.
    let mut shaped_lines: Vec<(Vec<u8>, f64, f64)> = Vec::new();
    let mut shaped_font: Option<(String, String)> = None;
    if shaped {
        use pdfcraft_fonts::layout::LineAlign;
        let err = |e: pdfcraft_fonts::paint::TextError| EditError::Invalid(format!("\"{text}\" can't be shown: {e}"));
        let mut u = pdfcraft_fonts::paint::UnicodeLines::new().map_err(err)?;
        let align = match style.align {
            Some(crate::added::Align::Center) => LineAlign::Center,
            Some(crate::added::Align::Right) => LineAlign::Right,
            Some(crate::added::Align::Justify) => LineAlign::Justify,
            _ => LineAlign::Start,
        };
        let user_size = size * k;
        let to_user = user_size / f64::from(u.units_per_em().max(1));
        for line in u.paragraph(&text, user_size, width, known_direction(first), align).map_err(err)? {
            let mut bytes = Vec::new();
            for c in line.runs.iter().flat_map(|r| r.clusters.iter()) {
                bytes.extend(u.code(c).map_err(err)?.to_be_bytes());
            }
            shaped_lines.push((bytes, line.x as f64 * to_user / k, line.advance() as f64 * to_user / k));
        }
        let font = u.write(doc).map_err(err)?;
        let mut name = String::from("PCUni");
        let mut suffix = 0u32;
        while fonts_res.contains(name.as_bytes()) {
            suffix = suffix.saturating_add(1);
            name = format!("PCUni{suffix}");
        }
        fonts_res.set(name.clone().into_bytes(), Object::Ref(font));
        let family = pdfcraft_fonts::shaping::ShapingFace::arabic().map_or_else(|| "Arabic".to_string(), |f| f.name().to_string());
        shaped_font = Some((name, family));
    }
    let wrapped = if shaped { vec![String::new(); shaped_lines.len()] } else { wrap(&text, width + 0.5, advance) };
    let mut substituted = None;
    let new_font = !reuse;
    let (show_font, encode): (String, Encoder) = if let Some((name, family)) = shaped_font.clone() {
        substituted = Some(family);
        (name, Box::new(|_: &str| None))
    } else if let Some(m) = metrics.clone().filter(|_| reuse) {
        (font_name.clone(), Box::new(move |s: &str| m.encode(s)))
    } else if let Some(fallback) = type3.clone() {
        let name = fallback.name.clone();
        let encoder = fallback.clone();
        substituted = Some(format!("{} Type3", fallback.family));
        (name, Box::new(move |s: &str| type3_encode(&encoder, s)))
    } else {
        let win = pdfcraft_fonts::win_ansi(&text);
        let back: String = win.iter().map(|c| char::from_u32(u32::from(*c)).unwrap_or('?')).collect();
        let base = family.base_font(bold, italic);
        if text.chars().zip(back.chars()).any(|(a, c)| c == '?' && a != '?') {
            return Err(EditError::Invalid(format!("\"{text}\" has characters neither {} nor {base} can show", b.base_font)));
        }
        let mut f = Dict::new();
        f.set(b"Type".to_vec(), Object::name("Font"));
        f.set(b"Subtype".to_vec(), Object::name("Type1"));
        f.set(b"BaseFont".to_vec(), Object::name(base));
        f.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
        let name = if style.family.is_none() { String::from_utf8_lossy(SUBSTITUTE).into_owned() } else { format!("PCEd{}", base.replace('-', "")) };
        fonts_res.set(name.clone().into_bytes(), Object::Dict(f));
        if style.family.is_none() {
            substituted = Some(base.to_string());
        }
        (name, Box::new(|s: &str| Some(pdfcraft_fonts::win_ansi(s))))
    };
    // Line spacing in text space: the paragraph's own (scaled with the size), or 1.2 × the size.
    let lead = match style.line_spacing {
        Some(m) => size * m.clamp(0.5, 5.0),
        None if members.len() > 1 => (members[0].origin.baseline - members[1].origin.baseline) / k * size / old_size,
        None if o_state.leading > 0.0 => o_state.leading,
        None => size * 1.2,
    };
    let n = pdfcraft_content::num;
    // In its own graphics state, so a new colour (or anything else) stops at the paragraph.
    let mut block_ops = vec![Op::new("q", vec![])];
    // Moved: a translation inside that state. The move is in page space, so it is taken back
    // through the CTM's linear part into the space the paragraph is drawn in.
    if dx != 0.0 || dy != 0.0 {
        let c = o.ctm;
        let back = Matrix([c[0], c[1], c[2], c[3], 0.0, 0.0])
            .invert()
            .ok_or_else(|| EditError::Invalid("the paragraph is drawn in a space it can't be moved in".into()))?;
        let (ax, ay) = back.apply(dx, dy);
        block_ops.push(Op::new("cm", vec![n(1.0), n(0.0), n(0.0), n(1.0), n(ax), n(ay)]));
    }
    block_ops.push(Op::new("BT", vec![]));
    let mut state = o_state.clone();
    state.word_spacing = 0.0;
    if shaped {
        // Spacing letters apart would break their joins.
        state.char_spacing = 0.0;
        state.scale = 1.0;
    }
    state.font = Some((show_font, size));
    if let Some([r, g, bl]) = style.color {
        state.fill = vec![Op::new("rg", vec![n(r), n(g), n(bl)])];
    }
    block_ops.extend(state.ops());
    block_ops.push(Op::new("Tm", o.tm.iter().map(|v| n(*v)).collect()));
    // Alignment: each line's offset from the left edge, in text space.
    let offset = |line: &str| -> f64 {
        let free = (width - advance(line)).max(0.0) / k;
        match style.align {
            Some(crate::added::Align::Center) => free / 2.0,
            Some(crate::added::Align::Right) => free,
            _ => 0.0,
        }
    };
    // Justify: word spacing (single-byte code 32 only) so each line but the last fills the width.
    let single_byte = !reuse || metrics.as_ref().is_some_and(|m| !m.composite);
    let justify = style.align == Some(crate::added::Align::Justify) && single_byte;
    let mut x = 0.0;
    let mut tw_set = 0.0;
    let mut underlines: Vec<(f64, f64, f64)> = Vec::new(); // (x0, x1, y) in text space
    for (i, (bytes, sx, sw)) in shaped_lines.iter().enumerate() {
        if i > 0 || *sx != 0.0 {
            block_ops.push(Op::new("Td", vec![n(sx - x), n(if i > 0 { -lead } else { 0.0 })]));
        }
        x = *sx;
        block_ops.push(Op::new("Tj", vec![Object::String(PdfString { bytes: bytes.clone(), hex: true })]));
        underlines.push((*sx, sx + sw, -(i as f64) * lead - size * 0.12));
    }
    for (i, line) in wrapped.iter().enumerate().filter(|_| !shaped) {
        let dx = offset(line);
        if i > 0 || dx != 0.0 {
            block_ops.push(Op::new("Td", vec![n(dx - x), n(if i > 0 { -lead } else { 0.0 })]));
        }
        x = dx;
        let spaces = line.matches(' ').count();
        let tw =
            if justify && i + 1 < wrapped.len() && spaces > 0 { (width - advance(line)).max(0.0) / k / o_state.scale / spaces as f64 } else { 0.0 };
        if tw != tw_set {
            block_ops.push(Op::new("Tw", vec![n(tw)]));
            tw_set = tw;
        }
        let bytes = encode(line).ok_or_else(|| EditError::Invalid(format!("\"{line}\" can't be shown")))?;
        block_ops.push(Op::new("Tj", vec![Object::String(PdfString::literal(bytes))]));
        let w = (advance(line) + tw * spaces as f64 * o_state.scale * k) / k;
        underlines.push((dx, dx + w, -(i as f64) * lead - size * 0.12));
    }
    block_ops.push(Op::new("ET", vec![]));
    if style.underline == Some(true) {
        // In text space (the paragraph's text matrix), in its fill colour.
        let colour: Vec<Op> = state
            .fill
            .iter()
            .map(|f| match f.op.as_slice() {
                b"g" => Op::new("G", f.operands.clone()),
                b"rg" => Op::new("RG", f.operands.clone()),
                b"k" => Op::new("K", f.operands.clone()),
                b"cs" => Op::new("CS", f.operands.clone()),
                b"sc" => Op::new("SC", f.operands.clone()),
                _ => Op::new("SCN", f.operands.clone()),
            })
            .collect();
        block_ops.push(Op::new("q", vec![]));
        block_ops.push(Op::new("cm", o.tm.iter().map(|v| n(*v)).collect()));
        block_ops.extend(colour);
        block_ops.push(Op::new("w", vec![n(size * 0.06)]));
        for (x0, x1, y) in underlines {
            block_ops.push(Op::new("m", vec![n(x0), n(y)]));
            block_ops.push(Op::new("l", vec![n(x1), n(y)]));
        }
        block_ops.push(Op::new("S", vec![]));
        block_ops.push(Op::new("Q", vec![]));
    }
    block_ops.push(Op::new("Q", vec![]));
    // The paragraph's operators go, and so does anything drawn in the same area (a fake-bold
    // second copy, an invisible text layer): the new text is all that may show.
    let mut drop: std::collections::HashMap<usize, std::collections::HashSet<usize>> =
        members.iter().flat_map(|l| [(l.stream, l.ops.iter().copied().collect::<std::collections::HashSet<_>>())]).collect();
    let member_rects: Vec<[f64; 4]> = members.iter().map(|l| l.rect).collect();
    for (stream, ops) in coincident_ops(&lines, &member_rects) {
        drop.entry(stream).or_default().extend(ops);
    }
    // Text shown earlier in the same text object stays first: the paragraph then goes where it
    // was, between two halves of the text object, so it keeps its place in reading order.
    let start = first.ops.first().copied().unwrap_or(o.bt_op);
    let split = ops.get(o.bt_op..start).unwrap_or_default().iter().enumerate().any(|(i, op)| {
        matches!(op.op.as_slice(), b"Tj" | b"TJ" | b"'" | b"\"") && !drop.get(&first.stream).is_some_and(|d| d.contains(&(o.bt_op + i)))
    });
    // What the text after the paragraph expects: the state at its BT (or where it was split).
    let after = if split { &o.state } else { &o.bt_state };
    block_ops.extend(after.ops().into_iter().filter(|op| !op.is("Tf") || after.font.is_some()));
    if split {
        block_ops.insert(0, Op::new("ET", vec![]));
        block_ops.push(Op::new("BT", vec![]));
        block_ops.push(Op::new("Tm", o.tlm.iter().map(|v| n(*v)).collect()));
    }
    let at = if split { start } else { o.bt_op };
    let new_data = splice(&data, &ops, |i| {
        let insert = if i == at { std::mem::take(&mut block_ops) } else { Vec::new() };
        (insert, drop.get(&first.stream).is_none_or(|d| !d.contains(&i)))
    });
    let mut dict = match &*doc.resolve(&stream_obj) {
        Object::Stream(s) => s.dict.clone(),
        _ => Dict::new(),
    };
    dict.remove(b"Length");
    let new = doc.add(Object::Stream(Stream::flate(dict, &new_data)));
    let contents: Vec<Object> = streams.iter().enumerate().map(|(i, (o, _))| if i == first.stream { Object::Ref(new) } else { o.clone() }).collect();
    if new_font {
        res.set(b"Font".to_vec(), Object::Dict(fonts_res));
    }
    let page_ref = p.obj;
    doc.update_dict(page_ref, |d| {
        d.set(b"Contents".to_vec(), if contents.len() == 1 { contents[0].clone() } else { Object::Array(contents) });
        if new_font {
            d.set(b"Resources".to_vec(), Object::Dict(res));
        }
    })?;
    Ok(LineEdit { substituted })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_wrapping_keeps_ideographic_spaces() {
        let width = |s: &str| s.chars().count() as f64;
        assert_eq!(wrap("A　　B C", 4.0, width), ["A　　B", "C"]);
        assert_eq!(wrap("A\u{a0}B C", 3.0, width), ["A B", "C"]);
        assert_eq!(wrap("A\u{2028}B C", 3.0, width), ["A B", "C"]);
        assert_eq!(wrap(" \tA  B\r\nC ", 3.0, width), ["A B", "C"]);
    }
}
