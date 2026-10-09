//! A whole PDF with Arabic in an embedded subset font: written with `cos`, read back, and its
//! font program, ToUnicode map and content checked. The file is left in the test's temporary
//! directory (`arabic-embedded.pdf`) for external checks (qpdf, mutool, pdftotext, pdffonts).

use std::sync::Arc;

use pdfcraft_cos::{Dict, Document, Object, Stream};
use pdfcraft_fonts::embed::FontSubset;
use pdfcraft_fonts::layout::{BaseDirection, LineAlign, layout};
use pdfcraft_fonts::paint::{PaintOptions, paint_lines};
use pdfcraft_fonts::shaping::ShapingFace;

const SAMPLE: &str = "بِسْمِ اللهِ الرَّحْمٰنِ الرَّحِيمِ — رقم 123 ABC\nمرحبا بالعالم (تجربة) لا إله إلا الله ١٢٣\nThe word سلام means peace.";

/// A one-page document drawing `text` at 14 pt in a 400 pt wide box.
fn document(face: &'static ShapingFace, text: &str, align: LineAlign) -> Document {
    let mut doc = Document::new_empty();
    let upem = i64::from(face.units_per_em());
    let lines = layout(face, text, 400 * upem / 14, BaseDirection::Auto, align).unwrap();
    let mut subset = FontSubset::new(face);
    let ops = paint_lines(
        &mut subset,
        &lines,
        &PaintOptions { font: "PCAr1", size: 14.0, left: 100.0, baseline: 700.0, leading: 22.0, fake_bold: false, slant: 0.0, actual_text: false },
    )
    .unwrap();
    let font = subset.write(&mut doc).unwrap();
    let content = doc.add(Stream::flate(Dict::new(), ops.as_bytes()));
    let mut fonts = Dict::new();
    fonts.set(b"PCAr1".to_vec(), Object::Ref(font));
    let mut res = Dict::new();
    res.set(b"Font".to_vec(), Object::Dict(fonts));
    let root = doc.root().unwrap();
    let pages = doc.get(root).as_dict().unwrap().reference(b"Pages").unwrap();
    let mut page = Dict::new();
    page.set(b"Type".to_vec(), Object::name("Page"));
    page.set(b"Parent".to_vec(), Object::Ref(pages));
    page.set(b"MediaBox".to_vec(), Object::Array([0, 0, 612, 792].map(Object::Int).to_vec()));
    page.set(b"Resources".to_vec(), Object::Dict(res));
    page.set(b"Contents".to_vec(), Object::Ref(content));
    let page = doc.add(page);
    doc.update_dict(pages, |d| {
        d.set(b"Kids".to_vec(), Object::Array(vec![Object::Ref(page)]));
        d.set(b"Count".to_vec(), Object::Int(1));
    })
    .unwrap();
    doc
}

#[test]
fn arabic_pdf_round_trips_through_cos() {
    let Some(face) = ShapingFace::arabic() else {
        eprintln!("skipping: built without the craft-fonts Arabic face (set CRAFT_FONTS_DIR)");
        return;
    };
    let doc = document(face, SAMPLE, LineAlign::Start);
    let bytes = pdfcraft_cos::write_full(&doc, &Default::default()).unwrap();
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::write(dir.join("arabic-embedded.pdf"), &bytes).unwrap();
    let justified = document(face, &"كلمة عربية طويلة ".repeat(30), LineAlign::Justify);
    std::fs::write(dir.join("arabic-justified.pdf"), pdfcraft_cos::write_full(&justified, &Default::default()).unwrap()).unwrap();

    // Read back: one Type0 font whose descendant embeds a TrueType program skrifa can read.
    let back = Document::open(Arc::new(bytes)).unwrap();
    let mut type0 = 0;
    for num in back.object_numbers() {
        let o = back.try_get(num).unwrap();
        let Some(d) = o.as_dict() else { continue };
        if d.name(b"Subtype") != Some(b"Type0") {
            continue;
        }
        type0 += 1;
        let tu = back.get(d.reference(b"ToUnicode").unwrap());
        let Object::Stream(tu) = &*tu else { panic!() };
        let cmap = String::from_utf8(tu.decoded().unwrap()).unwrap();
        // Every letter of the sample is somewhere in the map (as part of a cluster).
        for c in "بسماللهالرحمنرقممرحبابالعالمتجربةإلاسلام".chars() {
            let hex = format!("{:04X}", u32::from(c));
            assert!(cmap.contains(&hex), "{c} missing from ToUnicode");
        }
        let cid = d.get(b"DescendantFonts").and_then(Object::as_array).and_then(|a| a.first()).and_then(Object::as_ref).unwrap();
        let cid = back.get(cid);
        let fd = back.get(cid.as_dict().unwrap().reference(b"FontDescriptor").unwrap());
        let ff = back.get(fd.as_dict().unwrap().reference(b"FontFile2").unwrap());
        let Object::Stream(ff) = &*ff else { panic!() };
        let program = ff.decoded().unwrap();
        assert_eq!(ff.dict.int(b"Length1"), Some(program.len() as i64));
        let font = skrifa::FontRef::new(&program).unwrap();
        use skrifa::MetadataProvider;
        assert!(font.outline_glyphs().get(skrifa::GlyphId::new(1)).is_some());
    }
    assert_eq!(type0, 1);
}

#[test]
fn nothing_is_written_when_text_cannot_be_shown() {
    let Some(face) = ShapingFace::arabic() else { return };
    // Layout refuses before any object exists.
    assert!(layout(face, "سلام 日本", 10_000, BaseDirection::Auto, LineAlign::Start).is_err());
}
