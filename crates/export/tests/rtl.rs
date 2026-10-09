//! Right-to-left export: Arabic paragraphs and tables in HTML, Word and RTF. The files are left
//! in the test's temporary directory (`rtl.html`, `rtl.docx`, `rtl.rtf`) for checks with
//! independent readers (LibreOffice, pandoc, a browser).

use std::io::Read;

use pdfcraft_export::{Block, Page, docx, html, rtf};

fn block(text: &str, x: f64, y: f64, w: f64, size: f64, rtl: bool) -> Block {
    Block { text: text.into(), rect: [x, y, x + w, y + size], size, bold: false, italic: false, rtl, font: None }
}

/// A page: an Arabic heading and paragraph, a mixed line, an English paragraph, and a
/// three-column Arabic table (on the page, its first column "الاسم" is on the right).
fn page() -> Page {
    let mut blocks = vec![
        block("تقرير المبيعات", 300.0, 740.0, 240.0, 20.0, true),
        block("هذه فقرة عربية تشرح محتوى التقرير بالتفصيل لكي تكون بحجم نص المتن في الصفحة.", 72.0, 700.0, 468.0, 11.0, true),
        block("رقم الفاتورة 123 ABC", 300.0, 680.0, 240.0, 11.0, true),
        block("The word سلام means peace, and this English paragraph is body text too.", 72.0, 660.0, 468.0, 11.0, false),
    ];
    // Columns right to left: الاسم (x 420), الكمية (x 300), السعر (x 180).
    for (row, y) in [(["الاسم", "الكمية", "السعر"], 600.0), (["قلم", "٣", "١٠"], 585.0), (["دفتر", "٥", "٢٠"], 570.0)] {
        for (text, x) in row.iter().zip([420.0, 300.0, 180.0]) {
            blocks.push(block(text, x, y, 60.0, 11.0, true));
        }
    }
    Page { width: 612.0, height: 792.0, blocks, images: Vec::new(), ..Default::default() }
}

fn unzip(bytes: &[u8], name: &str) -> String {
    // The writer's own stored/deflated parts, read with a minimal local-header walk.
    let mut at = 0usize;
    while let Some(h) = bytes.get(at..at + 30) {
        if h[..4] != *b"PK\x03\x04" {
            break;
        }
        let method = u16::from_le_bytes([h[8], h[9]]);
        let size = u32::from_le_bytes([h[18], h[19], h[20], h[21]]) as usize;
        let nlen = u16::from_le_bytes([h[26], h[27]]) as usize;
        let xlen = u16::from_le_bytes([h[28], h[29]]) as usize;
        let fname = String::from_utf8_lossy(&bytes[at + 30..at + 30 + nlen]).into_owned();
        let data = &bytes[at + 30 + nlen + xlen..at + 30 + nlen + xlen + size];
        if fname == name {
            let mut out = String::new();
            if method == 8 {
                flate2::read::DeflateDecoder::new(data).read_to_string(&mut out).unwrap();
            } else {
                out = String::from_utf8(data.to_vec()).unwrap();
            }
            return out;
        }
        at += 30 + nlen + xlen + size;
    }
    panic!("{name} not in the archive");
}

#[test]
fn arabic_exports_right_to_left_in_every_format() {
    let pages = [page()];
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));

    let h = html(&pages, "تقرير");
    std::fs::write(dir.join("rtl.html"), &h).unwrap();
    assert!(h.contains("<h1 dir=\"rtl\">تقرير المبيعات</h1>") || h.contains("<h2 dir=\"rtl\">تقرير المبيعات</h2>"), "{h}");
    assert!(h.contains("<p dir=\"rtl\">رقم الفاتورة 123 ABC</p>"), "text in logical order: {h}");
    assert!(h.contains("<p>The word سلام means peace"), "an English paragraph stays left to right: {h}");
    // The table reads right to left, its first logical cell being the rightmost on the page.
    assert!(h.contains("<table dir=\"rtl\" style=\"margin-left:auto\">\n<tr><td>الاسم</td><td>الكمية</td><td>السعر</td></tr>"), "{h}");

    let d = docx(&pages, "تقرير");
    std::fs::write(dir.join("rtl.docx"), &d).unwrap();
    let xml = unzip(&d, "word/document.xml");
    assert!(xml.contains("<w:p><w:pPr><w:bidi/></w:pPr>"), "{xml}");
    // The mixed line: an Arabic run (with its trailing space) marked right to left, then the
    // number and the Latin as a left-to-right run, in logical order.
    assert!(
        xml.contains("<w:szCs w:val=\"22\"/><w:rtl/><w:lang w:bidi=\"ar-SA\"/></w:rPr><w:t xml:space=\"preserve\">رقم الفاتورة </w:t></w:r><w:r><w:rPr><w:sz w:val=\"22\"/></w:rPr><w:t xml:space=\"preserve\">123 ABC</w:t>"),
        "{xml}"
    );
    assert!(xml.contains("<w:tblPr><w:bidiVisual/>"), "{xml}");
    let first_cell = xml.find(">الاسم<").unwrap();
    assert!(first_cell < xml.find(">الكمية<").unwrap() && xml.find(">الكمية<").unwrap() < xml.find(">السعر<").unwrap());
    // Latin text is formatted as before: no complex-script properties.
    assert!(xml.contains("<w:r><w:rPr><w:sz w:val=\"22\"/></w:rPr><w:t xml:space=\"preserve\">The word </w:t>"), "{xml}");

    let r = rtf(&pages);
    std::fs::write(dir.join("rtl.rtf"), &r).unwrap();
    assert!(r.contains("{\\pard\\rtlpar\\qr\\fs22 {\\rtlch\\afs22 "), "{r}");
    assert!(r.contains("{\\ltrch 123 ABC}"), "{r}");
    // Right-to-left rows, cells in logical order from the right.
    assert!(r.contains("\\trowd\\rtlrow\\trgaph108"), "{r}");
    let cell = |t: &str| r.find(&t.chars().map(|c| format!("\\u{}?", c as u32 as u16 as i16)).collect::<String>()).unwrap();
    assert!(cell("الاسم") < cell("الكمية") && cell("الكمية") < cell("السعر"));
}

#[test]
fn latin_exports_are_unchanged() {
    let p = Page {
        width: 612.0,
        height: 792.0,
        blocks: vec![block("Hello world, this is body text.", 72.0, 700.0, 400.0, 11.0, false)],
        images: Vec::new(),
        ..Default::default()
    };
    let h = html(std::slice::from_ref(&p), "t");
    assert!(h.contains("<p>Hello world, this is body text.</p>") && !h.contains("dir="));
    let xml = unzip(&docx(std::slice::from_ref(&p), "t"), "word/document.xml");
    assert!(!xml.contains("bidi") && !xml.contains("w:rtl") && !xml.contains("szCs"));
    let r = rtf(&[p]);
    assert!(r.contains("{\\pard\\fs22 Hello world, this is body text.\\par}") && !r.contains("rtl"));
}
