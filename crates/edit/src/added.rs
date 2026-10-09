//! Edit a PDF ▸ Add content: text and images added as page content (execution plan M7.4).
//!
//! Each added item is its own content stream on the page, tagged `/PCMark /Added`, with its
//! parameters (text, font, size, colour, alignment, box; or image and box) kept in the stream
//! dictionary under `/PCAdded`. That keeps the item editable later (move, resize, retype,
//! reformat, delete) without content-stream surgery, while every viewer sees ordinary page
//! content. Boxes are in display space (origin at the bottom-left of the page as shown, after
//! `/Rotate`), so items stay upright on rotated pages.

use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString, Stream};
use pdfcraft_fonts::layout::{BaseDirection, LineAlign};
use pdfcraft_fonts::paint::{PaintOptions, layout_points, write_block};
use pdfcraft_fonts::shaping::ShapingFace;
use pdfcraft_fonts::{helvetica_width, literal, needs_unicode_font, win_ansi};

use crate::{EditError, check, contents, n, page_list, place_tagged};

const TAG: &str = "Added";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Family {
    #[default]
    Helvetica,
    Times,
    Courier,
}

impl Family {
    pub fn label(self) -> &'static str {
        match self {
            Family::Helvetica => "Helvetica",
            Family::Times => "Times",
            Family::Courier => "Courier",
        }
    }

    /// The standard-14 font name for this family and style.
    pub fn base_font(self, bold: bool, italic: bool) -> &'static str {
        match (self, bold, italic) {
            (Family::Helvetica, false, false) => "Helvetica",
            (Family::Helvetica, true, false) => "Helvetica-Bold",
            (Family::Helvetica, false, true) => "Helvetica-Oblique",
            (Family::Helvetica, true, true) => "Helvetica-BoldOblique",
            (Family::Times, false, false) => "Times-Roman",
            (Family::Times, true, false) => "Times-Bold",
            (Family::Times, false, true) => "Times-Italic",
            (Family::Times, true, true) => "Times-BoldItalic",
            (Family::Courier, false, false) => "Courier",
            (Family::Courier, true, false) => "Courier-Bold",
            (Family::Courier, false, true) => "Courier-Oblique",
            (Family::Courier, true, true) => "Courier-BoldOblique",
        }
    }

    fn from_base(base: &[u8]) -> (Family, bool, bool) {
        let s = String::from_utf8_lossy(base);
        let family = if s.starts_with("Times") {
            Family::Times
        } else if s.starts_with("Courier") {
            Family::Courier
        } else {
            Family::Helvetica
        };
        (family, s.contains("Bold"), s.contains("Italic") || s.contains("Oblique"))
    }

    /// Approximate advance width of `s` (standard-14 metrics are not bundled: Helvetica widths,
    /// scaled for Times; Courier is monospaced).
    pub fn width(self, s: &str, size: f64, bold: bool) -> f64 {
        let w = match self {
            Family::Courier => s.chars().count() as f64 * 0.6 * size,
            Family::Times => helvetica_width(s, size) * 0.9,
            Family::Helvetica => helvetica_width(s, size),
        };
        if bold { w * 1.05 } else { w }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
    /// Lines (but the last) stretched to the box's width.
    Justify,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AddedText {
    /// The text box in display space; its width wraps the text and its top is the first line's
    /// top. The height follows the text.
    pub rect: [f64; 4],
    pub text: String,
    pub family: Family,
    pub bold: bool,
    pub italic: bool,
    pub size: f64,
    pub color: [f64; 3],
    pub align: Align,
    /// Paragraph direction for text drawn with the embedded Unicode font (Arabic and other text
    /// WinAnsi can't show). `Auto` follows each paragraph's first strong character.
    pub direction: BaseDirection,
}

impl Default for AddedText {
    fn default() -> Self {
        AddedText {
            rect: [0.0; 4],
            text: String::new(),
            family: Family::Helvetica,
            bold: false,
            italic: false,
            size: 12.0,
            color: [0.0; 3],
            align: Align::Left,
            direction: BaseDirection::Auto,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AddedImage {
    /// Display space.
    pub rect: [f64; 4],
    /// The image XObject.
    pub image: ObjRef,
    /// Quarter turns counter-clockwise (0–3).
    pub rotation: u8,
    pub flip_h: bool,
    pub flip_v: bool,
    /// The fraction trimmed from each side of the image: left, bottom, right, top.
    pub crop: [f64; 4],
}

impl AddedImage {
    pub fn new(rect: [f64; 4], image: ObjRef) -> Self {
        AddedImage { rect, image, rotation: 0, flip_h: false, flip_v: false, crop: [0.0; 4] }
    }

    /// The matrix from image space (the unit square) to display space: crop, flip, rotate,
    /// then fill the box.
    fn matrix(&self) -> pdfcraft_content::Matrix {
        use pdfcraft_content::Matrix as M;
        let [l, b, r, t] = self.crop.map(|v| v.clamp(0.0, 0.45));
        let (cw, ch) = ((1.0 - l - r).max(0.05), (1.0 - b - t).max(0.05));
        let mut m = M([1.0 / cw, 0.0, 0.0, 1.0 / ch, -l / cw, -b / ch]);
        if self.flip_h {
            m = m.then(&M([-1.0, 0.0, 0.0, 1.0, 1.0, 0.0]));
        }
        if self.flip_v {
            m = m.then(&M([1.0, 0.0, 0.0, -1.0, 0.0, 1.0]));
        }
        for _ in 0..self.rotation % 4 {
            // A quarter turn counter-clockwise within the unit square.
            m = m.then(&M([0.0, 1.0, -1.0, 0.0, 1.0, 0.0]));
        }
        let [x0, y0, x1, y1] = norm(self.rect);
        m.then(&M([x1 - x0, 0.0, 0.0, y1 - y0, x0, y0]))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(AddedText),
    Image(AddedImage),
}

impl Content {
    pub fn rect(&self) -> [f64; 4] {
        match self {
            Content::Text(t) => t.rect,
            Content::Image(i) => i.rect,
        }
    }

    /// The same item moved/resized to `rect` (text keeps its computed height).
    pub fn with_rect(&self, rect: [f64; 4]) -> Content {
        match self {
            Content::Text(t) => Content::Text(AddedText { rect, ..t.clone() }),
            Content::Image(i) => Content::Image(AddedImage { rect, ..i.clone() }),
        }
    }
}

/// An added item on a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Added {
    pub page: usize,
    /// The item's content stream.
    pub obj: ObjRef,
    pub content: Content,
}

fn nums(doc: &Document, o: Option<&Object>) -> Vec<f64> {
    o.map(|o| doc.resolve(o)).and_then(|a| a.as_array().map(|a| a.iter().filter_map(Object::as_f64).collect())).unwrap_or_default()
}

fn norm(r: [f64; 4]) -> [f64; 4] {
    [r[0].min(r[2]), r[1].min(r[3]), r[0].max(r[2]), r[1].max(r[3])]
}

/// Whether a text item is drawn with the embedded Unicode font (shaped, bidi) rather than a
/// standard font: it has characters WinAnsi can't show.
pub fn uses_unicode_font(t: &AddedText) -> bool {
    needs_unicode_font(&t.text)
}

/// `Left` is the paragraph's start edge (right for right-to-left paragraphs).
fn line_align(a: Align) -> LineAlign {
    match a {
        Align::Left => LineAlign::Start,
        Align::Center => LineAlign::Center,
        Align::Right => LineAlign::Right,
        Align::Justify => LineAlign::Justify,
    }
}

/// The Unicode face, or why the text can't be shown.
fn unicode_face(t: &AddedText) -> Result<&'static ShapingFace, EditError> {
    ShapingFace::arabic().ok_or_else(|| {
        let c = t.text.chars().find(|c| needs_unicode_font(c.encode_utf8(&mut [0; 4]))).unwrap_or('?');
        EditError::Invalid(format!(
            "this build has no font for \"{c}\" (U+{:04X}): Arabic text needs the Noto Sans Arabic face from craft-fonts (build with CRAFT_FONTS_DIR)",
            u32::from(c)
        ))
    })
}

fn text_error(e: impl std::fmt::Display) -> EditError {
    EditError::Invalid(format!("the text can't be added: {e}"))
}

/// The laid-out lines of a Unicode-font item.
fn unicode_lines(t: &AddedText) -> Result<(&'static ShapingFace, Vec<pdfcraft_fonts::layout::LaidLine>), EditError> {
    let face = unicode_face(t)?;
    let width = (t.rect[2] - t.rect[0]).abs().max(t.size);
    let lines = layout_points(face, &t.text, t.size, width, t.direction, line_align(t.align)).map_err(text_error)?;
    Ok((face, lines))
}

/// The lines of a text item after wrapping to its box width (logical order).
pub fn lines(t: &AddedText) -> Vec<String> {
    if uses_unicode_font(t)
        && let Ok((_, laid)) = unicode_lines(t)
    {
        return laid.into_iter().map(|l| l.text).collect();
    }
    let width = (t.rect[2] - t.rect[0]).max(t.size);
    let mut out = Vec::new();
    for para in t.text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && t.family.width(&candidate, t.size, t.bold) > width {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
        }
        out.push(line);
    }
    out
}

/// The box a text item occupies (height from its lines).
pub fn text_rect(t: &AddedText) -> [f64; 4] {
    let r = norm(t.rect);
    let h = lines(t).len().max(1) as f64 * t.size * 1.2;
    [r[0], r[3] - h, r[2], r[3]]
}

fn font_name(base: &str) -> String {
    format!("PCF{}", base.replace('-', ""))
}

/// A font resource name no font on the page has (`taken`), so items never replace each other's
/// fonts (object numbers alone don't do: a full save renumbers objects but keeps names).
fn free_font_name(taken: &Dict, num: u32) -> String {
    let mut name = format!("PCAr{num}");
    let mut suffix = 0u32;
    while taken.contains(name.as_bytes()) {
        suffix = suffix.saturating_add(1);
        name = format!("PCAr{num}_{suffix}");
    }
    name
}

/// Content and resources for an item; `view` maps display space to user space. `taken` holds
/// the page's font names. Returns the names of fonts the item owns (embedded Unicode fonts).
fn draw(doc: &mut Document, c: &Content, view: [f64; 6], taken: &Dict) -> Result<(Vec<u8>, Dict, Vec<String>), EditError> {
    let mut res = Dict::new();
    let mut owned = Vec::new();
    let mut out = format!("q {} {} {} {} {} {} cm\n", n(view[0]), n(view[1]), n(view[2]), n(view[3]), n(view[4]), n(view[5])).into_bytes();
    match c {
        Content::Text(t) if uses_unicode_font(t) => {
            let (face, lines) = unicode_lines(t)?;
            let r = text_rect(t);
            // A provisional name; the font's object number is known only once it is written.
            let provisional = free_font_name(taken, doc.object_numbers().last().copied().unwrap_or(0).saturating_add(1));
            let opts = PaintOptions {
                font: &provisional,
                size: t.size,
                left: r[0],
                baseline: r[3] - 0.95 * t.size,
                leading: 1.2 * t.size,
                fake_bold: t.bold,
                slant: if t.italic { 0.2 } else { 0.0 },
                actual_text: false,
            };
            let block = write_block(doc, face, &lines, &opts).map_err(text_error)?;
            let [cr, cg, cb] = t.color.map(|v| v.clamp(0.0, 1.0));
            out.extend(format!("{} {} {} rg {} {} {} RG\n", n(cr), n(cg), n(cb), n(cr), n(cg), n(cb)).bytes());
            out.extend(block.ops.bytes());
            let mut fonts = Dict::new();
            fonts.set(provisional.clone().into_bytes(), Object::Ref(block.font));
            res.set(b"Font".to_vec(), Object::Dict(fonts));
            owned.push(provisional);
        }
        Content::Text(t) => {
            let base = t.family.base_font(t.bold, t.italic);
            let name = font_name(base);
            let mut font = Dict::new();
            font.set(b"Type".to_vec(), Object::name("Font"));
            font.set(b"Subtype".to_vec(), Object::name("Type1"));
            font.set(b"BaseFont".to_vec(), Object::name(base));
            if t.family != Family::Courier || !t.text.is_ascii() {
                font.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
            }
            let mut fonts = Dict::new();
            fonts.set(name.clone().into_bytes(), Object::Dict(font));
            res.set(b"Font".to_vec(), Object::Dict(fonts));
            let r = text_rect(t);
            let [cr, cg, cb] = t.color.map(|v| v.clamp(0.0, 1.0));
            out.extend(format!("BT /{name} {} Tf {} {} {} rg\n", n(t.size), n(cr), n(cg), n(cb)).bytes());
            for (i, line) in lines(t).iter().enumerate() {
                let w = t.family.width(line, t.size, t.bold);
                let x = match t.align {
                    Align::Left | Align::Justify => r[0],
                    Align::Center => r[0] + ((r[2] - r[0]) - w) / 2.0,
                    Align::Right => r[2] - w,
                };
                // Baseline: 0.8 em below the line top.
                let y = r[3] - (i as f64 * 1.2 + 0.95) * t.size;
                // Justified: word spacing makes every line but the last fill the box.
                let all = lines(t);
                let spaces = line.matches(' ').count();
                let tw =
                    if t.align == Align::Justify && i + 1 < all.len() && spaces > 0 { ((r[2] - r[0]) - w).max(0.0) / spaces as f64 } else { 0.0 };
                out.extend(format!("1 0 0 1 {} {} Tm {} Tw ", n(x), n(y), n(tw)).bytes());
                out.extend(literal(&win_ansi(line)));
                out.extend_from_slice(b" Tj\n");
            }
            out.extend_from_slice(b"ET\n");
        }
        Content::Image(i) => {
            if !matches!(&*doc.get(i.image), Object::Stream(s) if s.dict.name(b"Subtype") == Some(b"Image")) {
                return Err(EditError::Invalid("not an image".into()));
            }
            let r = norm(i.rect);
            let name = format!("PCImg{}", i.image.num);
            let mut xo = Dict::new();
            xo.set(name.clone().into_bytes(), Object::Ref(i.image));
            res.set(b"XObject".to_vec(), Object::Dict(xo));
            let [a, b, c, d, e, f] = i.matrix().0;
            // Clip to the box: the cropped-away parts stay hidden.
            out.extend(
                format!(
                    "{} {} {} {} re W n {} {} {} {} {} {} cm /{name} Do\n",
                    n(r[0]),
                    n(r[1]),
                    n(r[2] - r[0]),
                    n(r[3] - r[1]),
                    n(a),
                    n(b),
                    n(c),
                    n(d),
                    n(e),
                    n(f)
                )
                .bytes(),
            );
        }
    }
    out.extend_from_slice(b"Q\n");
    Ok((out, res, owned))
}

fn params(c: &Content, fonts: &[String]) -> Dict {
    let mut d = Dict::new();
    let arr = |v: &[f64]| Object::Array(v.iter().map(|x| Object::Real(*x)).collect());
    match c {
        Content::Text(t) => {
            d.set(b"Kind".to_vec(), Object::name("Text"));
            d.set(b"Text".to_vec(), PdfString::text(&t.text));
            d.set(b"Font".to_vec(), Object::name(t.family.base_font(t.bold, t.italic)));
            d.set(b"Size".to_vec(), Object::Real(t.size));
            d.set(b"Color".to_vec(), arr(&t.color));
            d.set(
                b"Align".to_vec(),
                Object::Int(match t.align {
                    Align::Left => 0,
                    Align::Center => 1,
                    Align::Right => 2,
                    Align::Justify => 3,
                }),
            );
            d.set(b"Rect".to_vec(), arr(&text_rect(t)));
            match t.direction {
                BaseDirection::Auto => {}
                BaseDirection::Ltr => d.set(b"Dir".to_vec(), Object::name("LTR")),
                BaseDirection::Rtl => d.set(b"Dir".to_vec(), Object::name("RTL")),
            }
            if !fonts.is_empty() {
                d.set(b"PCFonts".to_vec(), Object::Array(fonts.iter().map(|f| Object::name(f)).collect()));
            }
        }
        Content::Image(i) => {
            d.set(b"Kind".to_vec(), Object::name("Image"));
            d.set(b"Image".to_vec(), Object::Ref(i.image));
            d.set(b"Rect".to_vec(), arr(&norm(i.rect)));
            if i.rotation % 4 != 0 {
                d.set(b"Rotate".to_vec(), Object::Int(i64::from(i.rotation % 4) * 90));
            }
            if i.flip_h {
                d.set(b"FlipH".to_vec(), Object::Bool(true));
            }
            if i.flip_v {
                d.set(b"FlipV".to_vec(), Object::Bool(true));
            }
            if i.crop != [0.0; 4] {
                d.set(b"Crop".to_vec(), arr(&i.crop));
            }
        }
    }
    d
}

fn parse(doc: &Document, d: &Dict) -> Option<Content> {
    let r = nums(doc, d.get(b"Rect"));
    let rect: [f64; 4] = r.try_into().ok()?;
    match d.name(b"Kind")? {
        b"Text" => {
            let (family, bold, italic) = Family::from_base(d.name(b"Font").unwrap_or(b"Helvetica"));
            let c = nums(doc, d.get(b"Color"));
            Some(Content::Text(AddedText {
                rect,
                text: d.get(b"Text").and_then(|t| doc.resolve(t).as_string().map(|s| s.to_text())).unwrap_or_default(),
                family,
                bold,
                italic,
                size: d.get(b"Size").and_then(Object::as_f64).unwrap_or(12.0),
                color: if c.len() == 3 { [c[0], c[1], c[2]] } else { [0.0; 3] },
                align: match d.int(b"Align") {
                    Some(1) => Align::Center,
                    Some(2) => Align::Right,
                    Some(3) => Align::Justify,
                    _ => Align::Left,
                },
                direction: match d.name(b"Dir") {
                    Some(b"RTL") => BaseDirection::Rtl,
                    Some(b"LTR") => BaseDirection::Ltr,
                    _ => BaseDirection::Auto,
                },
            }))
        }
        b"Image" => {
            let crop = nums(doc, d.get(b"Crop"));
            Some(Content::Image(AddedImage {
                rect,
                image: d.get(b"Image")?.as_ref()?,
                rotation: (d.int(b"Rotate").unwrap_or(0).rem_euclid(360) / 90) as u8,
                flip_h: matches!(d.get(b"FlipH"), Some(Object::Bool(true))),
                flip_v: matches!(d.get(b"FlipV"), Some(Object::Bool(true))),
                crop: crop.try_into().unwrap_or([0.0; 4]),
            }))
        }
        _ => None,
    }
}

fn validate(c: &Content) -> Result<(), EditError> {
    let r = c.rect();
    if !r.iter().all(|v| v.is_finite()) {
        return Err(EditError::Invalid("invalid position".into()));
    }
    match c {
        Content::Text(t) => {
            if t.text.trim().is_empty() {
                return Err(EditError::Invalid("type some text first".into()));
            }
            if !(t.size.is_finite() && (1.0..=500.0).contains(&t.size)) {
                return Err(EditError::Invalid("the font size must be between 1 and 500 points".into()));
            }
            if (r[2] - r[0]).abs() < 1.0 {
                return Err(EditError::Invalid("the text box is too narrow".into()));
            }
        }
        Content::Image(_) => {
            if (r[2] - r[0]).abs() < 1.0 || (r[3] - r[1]).abs() < 1.0 {
                return Err(EditError::Invalid("the image is too small".into()));
            }
        }
    }
    Ok(())
}

/// The embedded fonts an item owns (their names in the page's `/Font` resources).
fn own_fonts(doc: &Document, item: ObjRef) -> Vec<Vec<u8>> {
    let obj = doc.get(item);
    let Some(d) = obj.as_dict() else { return Vec::new() };
    let Some(p) = d.get(b"PCAdded").map(|p| doc.resolve(p)) else { return Vec::new() };
    let Some(list) = p.as_dict().and_then(|p| p.get(b"PCFonts")).and_then(Object::as_array).cloned() else { return Vec::new() };
    list.iter().filter_map(|o| o.as_name().map(<[u8]>::to_vec)).filter(|n| n.starts_with(b"PCAr")).collect()
}

/// Remove `names` from a page resources dictionary's `/Font`.
fn drop_fonts(doc: &Document, res: &mut Dict, names: &[Vec<u8>]) {
    if names.is_empty() {
        return;
    }
    let Some(mut fonts) = res.get(b"Font").map(|f| doc.resolve(f)).and_then(|f| f.as_dict().cloned()) else { return };
    for name in names {
        fonts.remove(name);
    }
    res.set(b"Font".to_vec(), Object::Dict(fonts));
}

/// Write an item's stream (new, or replacing `obj`) and make its resources available to the page.
/// All or nothing: on error the document is exactly as it was.
fn write(doc: &mut Document, page: usize, c: &Content, obj: Option<ObjRef>) -> Result<ObjRef, EditError> {
    validate(c)?;
    let before = doc.clone();
    let result = write_inner(doc, page, c, obj);
    if result.is_err() {
        *doc = before;
    }
    result
}

fn write_inner(doc: &mut Document, page: usize, c: &Content, obj: Option<ObjRef>) -> Result<ObjRef, EditError> {
    let all = page_list(doc);
    check(&[page], all.len())?;
    let p = &all[page];
    // The page gets its own copy of its resources with the item's fonts and images added, and
    // without the embedded fonts of the version it replaces.
    let mut pres = p.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    if let Some(r) = obj {
        drop_fonts(doc, &mut pres, &own_fonts(doc, r));
    }
    let taken = pres.get(b"Font").map(|f| doc.resolve(f)).and_then(|f| f.as_dict().cloned()).unwrap_or_default();
    let view = p.view_matrix(doc);
    let page_obj = p.obj;
    let (content, res, fonts) = draw(doc, c, view, &taken)?;
    for (k, v) in res.iter() {
        let mut sub = pres.get(k).map(|s| doc.resolve(s)).and_then(|s| s.as_dict().cloned()).unwrap_or_default();
        for (n2, o) in v.as_dict().into_iter().flat_map(|d| d.iter()) {
            sub.set(n2.clone(), o.clone());
        }
        pres.set(k.clone(), Object::Dict(sub));
    }
    doc.update_dict(page_obj, |d| d.set(b"Resources".to_vec(), Object::Dict(pres)))?;
    let mut sd = Dict::new();
    sd.set(b"PCMark".to_vec(), Object::name(TAG));
    sd.set(b"PCAdded".to_vec(), Object::Dict(params(c, &fonts)));
    let stream = Stream::flate(sd, &content);
    match obj {
        Some(r) => {
            doc.set(r, Object::Stream(stream));
            Ok(r)
        }
        None => {
            // place_tagged appends a new stream; recover its reference.
            let p = page_list(doc).swap_remove(page);
            place_tagged(doc, &p, TAG, content.clone(), false)?;
            let p = page_list(doc).swap_remove(page);
            let r = contents(doc, &p)?.last().and_then(Object::as_ref).ok_or_else(|| EditError::Invalid("could not add the content".into()))?;
            doc.set(r, Object::Stream(stream));
            Ok(r)
        }
    }
}

/// Every added item, page by page, in drawing order.
pub fn list_added(doc: &Document) -> Vec<Added> {
    let mut out = Vec::new();
    for (pi, p) in page_list(doc).iter().enumerate() {
        let list: Vec<Object> = match p.dict.get(b"Contents") {
            Some(c) => match &*doc.resolve(c) {
                Object::Array(a) => a.clone(),
                _ => vec![c.clone()],
            },
            None => Vec::new(),
        };
        for o in list {
            let Some(r) = o.as_ref() else { continue };
            let obj = doc.get(r);
            let Some(d) = obj.as_dict() else { continue };
            if d.name(b"PCMark") != Some(TAG.as_bytes()) {
                continue;
            }
            if let Some(c) = d.get(b"PCAdded").and_then(|p| doc.resolve(p).as_dict().cloned()).and_then(|p| parse(doc, &p)) {
                out.push(Added { page: pi, obj: r, content: c });
            }
        }
    }
    out
}

/// Add an item to a page (0-based). Returns its index among the page's added items.
pub fn add_content(doc: &mut Document, page: usize, c: &Content) -> Result<usize, EditError> {
    write(doc, page, c, None)?;
    Ok(list_added(doc).iter().filter(|a| a.page == page).count() - 1)
}

fn find(doc: &Document, page: usize, index: usize) -> Result<Added, EditError> {
    list_added(doc)
        .into_iter()
        .filter(|a| a.page == page)
        .nth(index)
        .ok_or_else(|| EditError::Invalid(format!("page {} has no added item {}", page + 1, index + 1)))
}

/// Replace item `index` of `page` (moved, resized, retyped or reformatted).
pub fn update_content(doc: &mut Document, page: usize, index: usize, c: &Content) -> Result<(), EditError> {
    let a = find(doc, page, index)?;
    if std::mem::discriminant(&a.content) != std::mem::discriminant(c) {
        return Err(EditError::Invalid("an item can't change between text and image".into()));
    }
    write(doc, page, c, Some(a.obj))?;
    Ok(())
}

/// Remove item `index` of `page`.
pub fn delete_content(doc: &mut Document, page: usize, index: usize) -> Result<(), EditError> {
    let a = find(doc, page, index)?;
    let p = page_list(doc).swap_remove(page);
    let list: Vec<Object> = contents(doc, &p)?.into_iter().filter(|o| o.as_ref() != Some(a.obj)).collect();
    let fonts = own_fonts(doc, a.obj);
    // The item's embedded fonts leave the page's own resources with it (`write` gave the page
    // its own copy, so they are inline there).
    let res = match p.dict.get(b"Resources") {
        Some(Object::Dict(d)) if !fonts.is_empty() => {
            let mut d = d.clone();
            drop_fonts(doc, &mut d, &fonts);
            Some(d)
        }
        _ => None,
    };
    doc.update_dict(p.obj, |d| {
        d.set(b"Contents".to_vec(), Object::Array(list));
        if let Some(res) = res {
            d.set(b"Resources".to_vec(), Object::Dict(res));
        }
    })?;
    Ok(())
}
