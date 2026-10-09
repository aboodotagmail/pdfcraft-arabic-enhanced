//! The font family a PDF font's name stands for, so an exported Word file asks for the same
//! font ("Sakkal Majalla") and the reader's installed copy is used. Only the name is carried:
//! nothing of the font program leaves the PDF.

/// Longest family name kept (longer names are not font names a word processor knows).
const MAX_NAME: usize = 64;

/// Style words that end a PostScript name after a `-` or `,` ("Arial-BoldMT", "Arial,Bold").
const STYLES: &[&str] = &[
    "regular",
    "roman",
    "book",
    "normal",
    "bold",
    "italic",
    "oblique",
    "bolditalic",
    "boldoblique",
    "light",
    "medium",
    "semibold",
    "demibold",
    "black",
    "heavy",
    "thin",
    "extrabold",
    "extralight",
    "condensed",
    "it",
    "bd",
    "bi",
    "mt",
    "psmt",
];

/// The family `base_font` (a PDF `/BaseFont`) names, or `None` when it names none that can be
/// told: the subset tag (`ABCDEF+`), the style after `-` or `,` and PostScript's `MT`/`PS`
/// endings are dropped, joined words are split ("SakkalMajalla" → "Sakkal Majalla"), and the
/// standard 14 fonts become the faces Word has ("Helvetica" → "Arial").
pub fn font_family(base_font: &str) -> Option<String> {
    let mut name = base_font.trim();
    // A subset tag: six capitals and `+`.
    if let Some((tag, rest)) = name.split_once('+')
        && tag.len() == 6
        && tag.chars().all(|c| c.is_ascii_uppercase())
    {
        name = rest;
    }
    // The style after `,` always; after `-` when it is a style (a hyphen may be in a family).
    if let Some((family, _)) = name.split_once(',') {
        name = family;
    }
    if let Some((family, style)) = name.rsplit_once('-')
        && is_style(style)
    {
        name = family;
    }
    for end in ["PSMT", "MT", "PS"] {
        if let Some(stem) = name.strip_suffix(end)
            && stem.chars().last().is_some_and(|c| c.is_ascii_lowercase())
        {
            name = stem;
            break;
        }
    }
    if name.is_empty()
        || name.chars().count() > MAX_NAME
        || !name.chars().next().is_some_and(char::is_alphabetic)
        || name.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '&' | '"' | '\''))
    {
        return None;
    }
    let standard = match name {
        "Helvetica" => Some("Arial"),
        "Times" => Some("Times New Roman"),
        "Courier" => Some("Courier New"),
        "Symbol" => Some("Symbol"),
        "ZapfDingbats" => Some("Wingdings"),
        _ => None,
    };
    if let Some(s) = standard {
        return Some(s.to_string());
    }
    if name.contains(' ') {
        return Some(name.to_string());
    }
    // A generated name ("F1", "T3Font_0", "Font12") names nothing.
    if name.chars().any(|c| c == '_') || name.chars().count() < 3 {
        return None;
    }
    Some(split_words(name))
}

fn is_style(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    // "BoldMT", "BoldItalicMT", "SemiboldItalic": style words one after another.
    let mut rest = lower.as_str();
    while !rest.is_empty() {
        let Some(w) = STYLES.iter().filter(|w| rest.starts_with(**w)).max_by_key(|w| w.len()) else { return false };
        rest = rest.get(w.len()..).unwrap_or_default();
    }
    !lower.is_empty()
}

/// "SakkalMajalla" → "Sakkal Majalla", "MSGothic" → "MS Gothic": a space before a capital that
/// follows a small letter, or that starts a word after other capitals.
fn split_words(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|p| chars.get(p)).copied();
        let next = chars.get(i + 1).copied();
        let boundary = c.is_uppercase()
            && match prev {
                Some(p) if p.is_lowercase() => true,
                Some(p) if p.is_uppercase() => next.is_some_and(char::is_lowercase),
                _ => false,
            };
        if boundary {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_from_pdf_font_names() {
        let f = |s: &str| font_family(s);
        assert_eq!(f("ABCDEF+SakkalMajalla").as_deref(), Some("Sakkal Majalla"));
        assert_eq!(f("SakkalMajalla-Bold").as_deref(), Some("Sakkal Majalla"));
        assert_eq!(f("QWERTY+SakkalMajalla,Bold").as_deref(), Some("Sakkal Majalla"));
        assert_eq!(f("ArialMT").as_deref(), Some("Arial"));
        assert_eq!(f("Arial-BoldMT").as_deref(), Some("Arial"));
        assert_eq!(f("TimesNewRomanPSMT").as_deref(), Some("Times New Roman"));
        assert_eq!(f("TimesNewRomanPS-BoldItalicMT").as_deref(), Some("Times New Roman"));
        assert_eq!(f("Helvetica-Bold").as_deref(), Some("Arial"));
        assert_eq!(f("Times-Roman").as_deref(), Some("Times New Roman"));
        assert_eq!(f("MSGothic").as_deref(), Some("MS Gothic"));
        assert_eq!(f("NotoSansArabic-Regular").as_deref(), Some("Noto Sans Arabic"));
        assert_eq!(f("Traditional Arabic").as_deref(), Some("Traditional Arabic"));
        // A hyphen that isn't a style stays.
        assert_eq!(f("Al-Mohanad").as_deref(), Some("Al-Mohanad"));
        for none in ["", "F1", "T3Font_0", "ABCDEF+", "+", "12Font", "A<b>", &"X".repeat(100)] {
            assert_eq!(f(none), None, "{none:?}");
        }
    }
}
