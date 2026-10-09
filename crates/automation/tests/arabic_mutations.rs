//! Damaged Arabic PDFs never crash PdfCraft. `cargo xtask fuzz` mutates synthetic seeds without
//! embedded Type0/CIDFontType2 subsets, ToUnicode maps or right-to-left text, so it doesn't reach
//! the Arabic code paths. This test makes a PDF with Arabic in PdfCraft's embedded Noto Sans Arabic
//! subset, damages it in many deterministic ways, and drives each result through the tools: open,
//! render, extract (UAX #9 reading), find (Arabic folding), list lines, edit a line (shaped
//! rewrite), export to Word and save. Every call must return a result or an error, never panic.

use std::panic::{AssertUnwindSafe, catch_unwind};

use pdfcraft_automation::{Automation, Content};
use serde_json::{Value, json};

/// xorshift64: deterministic, so a failure names a reproducible iteration.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

fn json_of(r: Vec<Content>) -> Option<Value> {
    r.into_iter().find_map(|c| match c {
        Content::Json(v) => Some(v),
        Content::Png { .. } => None,
    })
}

/// One damaged copy of `seed`.
fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut d = seed.to_vec();
    for _ in 0..1 + rng.below(4) {
        let len = d.len().max(1);
        match rng.below(6) {
            // Flip bits of a byte.
            0 => {
                let i = rng.below(len);
                if let Some(b) = d.get_mut(i) {
                    *b ^= 1 << rng.below(8);
                }
            }
            // A number made huge, negative or zero (counts, lengths, widths, CIDs).
            1 => {
                let i = rng.below(len);
                let s: &[u8] = [b"99999999999".as_slice(), b"-1", b"0", b"4294967296"][rng.below(4)];
                let end = (i + s.len()).min(d.len());
                d.splice(i..end, s.iter().copied());
            }
            // Truncate.
            2 => d.truncate(rng.below(len)),
            // Duplicate a slice somewhere else.
            3 => {
                let a = rng.below(len);
                let b = (a + rng.below(200)).min(d.len());
                let chunk: Vec<u8> = d.get(a..b).map(<[u8]>::to_vec).unwrap_or_default();
                let at = rng.below(d.len().max(1));
                d.splice(at..at, chunk);
            }
            // Damage a ToUnicode or font keyword's surroundings.
            4 => {
                for key in [b"beginbfchar".as_slice(), b"/W ", b"/FontFile2", b"/CIDToGIDMap", b"/Length1"] {
                    if let Some(p) = d.windows(key.len()).position(|w| w == key) {
                        let i = p + key.len() + rng.below(8);
                        if let Some(b) = d.get_mut(i) {
                            *b = b"<>[]0 /"[rng.below(7)];
                        }
                    }
                }
            }
            // Random bytes.
            _ => {
                let i = rng.below(len);
                for k in 0..rng.below(16) {
                    if let Some(b) = d.get_mut(i + k) {
                        *b = (rng.next() & 0xFF) as u8;
                    }
                }
            }
        }
    }
    d
}

#[test]
fn damaged_arabic_pdfs_never_panic() {
    if pdfcraft_fonts::shaping::ShapingFace::arabic().is_none() {
        eprintln!("skipping: built without the craft-fonts Arabic face (set CRAFT_FONTS_DIR)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("pdfcraft-arabic-mutations-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut a = Automation::new().with_root(&dir).unwrap();

    // The seed: Arabic, mixed text and tashkeel in an embedded subset, saved in full.
    let doc = json_of(a.call("doc_create", &json!({ "from": "blank", "pages": 1 })).unwrap()).unwrap()["doc"].as_u64().unwrap();
    let text = "بِسْمِ اللهِ الرَّحْمٰنِ\nرقم 123 ABC (تجربة)\nThe word سلام means peace";
    a.call("page_add_text", &json!({ "doc": doc, "page": 1, "text": text, "at": [40, 60], "width": 300, "size": 14 })).unwrap();
    a.call("doc_save", &json!({ "doc": doc, "path": "seed.pdf", "full": true })).unwrap();
    let seed = std::fs::read(dir.join("seed.pdf")).unwrap();

    let iterations: usize = std::env::var("PDFCRAFT_ARABIC_MUTATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(150);
    let mut rng = Rng(0x5EED_A2AB_1C00_0001);
    let mut panics = Vec::new();
    let mut opened = 0usize;
    for i in 0..iterations {
        let bytes = mutate(&mut rng, &seed);
        std::fs::write(dir.join("m.pdf"), &bytes).unwrap();
        let mut step = |tool: &str, args: Value| -> Option<Value> {
            match catch_unwind(AssertUnwindSafe(|| a.call(tool, &args))) {
                Ok(Ok(r)) => json_of(r),
                Ok(Err(_)) => None,
                Err(_) => {
                    panics.push(format!("iteration {i}: {tool} {args} panicked"));
                    None
                }
            }
        };
        let Some(d) = step("doc_open", json!({ "path": "m.pdf" })).and_then(|v| v["doc"].as_u64()) else { continue };
        opened += 1;
        step("page_render", json!({ "doc": d, "page": 1, "dpi": 24 }));
        step("text_extract", json!({ "doc": d }));
        step("text_find", json!({ "doc": d, "query": "الرحمن" }));
        step("text_lines", json!({ "doc": d, "page": 1 }));
        step("text_edit", json!({ "doc": d, "page": 1, "line": 1, "text": "مرحبا بالعالم 42" }));
        step("doc_export_office", json!({ "doc": d, "path": "m.docx" }));
        step("doc_save", json!({ "doc": d, "path": "out.pdf" }));
        step("doc_close", json!({ "doc": d, "discard": true }));
    }
    eprintln!("arabic mutations: {iterations} mutants, {opened} opened, {} panics", panics.len());
    for p in panics.iter().take(10) {
        eprintln!("  {p}");
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(opened > iterations / 4, "most mutants should still open ({opened} of {iterations})");
    assert!(panics.is_empty(), "{} panic(s); the first: {}", panics.len(), panics[0]);
}
