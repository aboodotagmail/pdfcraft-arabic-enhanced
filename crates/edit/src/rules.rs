//! The straight horizontal and vertical lines a page draws (table borders and rules), for
//! Export to Word, HTML and RTF: a table's cells are the boxes its rules make.
//!
//! Lines come from stroked paths (`m`/`l`, `re`) and from filled rectangles thin enough to be
//! a line (word processors often draw borders as 0.5 pt wide filled boxes), through `q`/`Q`/`cm`
//! and form XObjects. Curves, diagonals and large filled areas (cell shading) are not rules.

use pdfcraft_content::{Matrix, parse};
use pdfcraft_cos::{Dict, Document, ObjRef};

use crate::EditError;

/// A rule from (x0, y0) to (x1, y1) in user space (y up): horizontal (`y0 == y1`, `x0 < x1`)
/// or vertical (`x0 == x1`, `y0 < y1`).
pub type Rule = [f64; 4];

/// Filled rectangles at most this thick (points) are lines.
const MAX_LINE_THICKNESS: f64 = 3.0;
/// Shorter segments are not rules (dots, underline ticks).
const MIN_RULE_LENGTH: f64 = 4.0;
/// How far (points) a segment may lean and still be horizontal or vertical.
const SLANT: f64 = 0.5;
/// Rules kept per page; ops read per page, all streams and forms together.
const MAX_RULES: usize = 20_000;
const MAX_OPS: usize = 2_000_000;

/// The horizontal and vertical rules page `page` (0-based) draws.
pub fn page_rules(doc: &Document, page: usize) -> Result<Vec<Rule>, EditError> {
    let p = crate::text::page_dict(doc, page)?;
    let res = p.dict.get(b"Resources").map(|r| doc.resolve(r)).and_then(|r| r.as_dict().cloned()).unwrap_or_default();
    let mut w = Walk { doc, out: Vec::new(), ops: 0, forms: 0, path: Vec::new() };
    // The CTM carries from one content stream to the next (they are one stream in pieces).
    let mut st = State { ctm: Matrix::IDENTITY, stack: Vec::new() };
    for (_, data) in crate::text::content_streams(doc, &p.dict) {
        walk(&mut w, &data, &res, &mut st);
    }
    Ok(w.out)
}

struct Walk<'a> {
    doc: &'a Document,
    out: Vec<Rule>,
    ops: usize,
    forms: usize,
    path: Vec<ObjRef>,
}

struct State {
    ctm: Matrix,
    stack: Vec<Matrix>,
}

/// One path being built: its segments (user space) and the rectangles among them.
#[derive(Default)]
struct Path {
    segments: Vec<[(f64, f64); 2]>,
    rects: Vec<[(f64, f64); 4]>,
    start: Option<(f64, f64)>,
    at: Option<(f64, f64)>,
}

fn walk(w: &mut Walk, data: &[u8], resources: &Dict, st: &mut State) {
    let mut path = Path::default();
    for op in parse(data).ops {
        w.ops = w.ops.saturating_add(1);
        if w.ops > MAX_OPS || w.out.len() >= MAX_RULES {
            return;
        }
        let ctm = st.ctm;
        match op.op.as_slice() {
            b"q" => st.stack.push(st.ctm),
            b"Q" => st.ctm = st.stack.pop().unwrap_or(st.ctm),
            b"cm" => {
                if let Some(m) = op.nums::<6>().filter(|m| m.iter().all(|v| v.is_finite())) {
                    st.ctm = Matrix(m).then(&st.ctm);
                }
            }
            b"m" => {
                if let Some([x, y]) = op.nums::<2>() {
                    let p = ctm.apply(x, y);
                    path.start = Some(p);
                    path.at = Some(p);
                }
            }
            b"l" => {
                if let (Some([x, y]), Some(from)) = (op.nums::<2>(), path.at) {
                    let to = ctm.apply(x, y);
                    path.segments.push([from, to]);
                    path.at = Some(to);
                }
            }
            b"c" | b"v" | b"y" => {
                // Curves are not rules; the path goes on from the curve's end.
                if let Some([x, y]) = op.nums::<2>() {
                    path.at = Some(ctm.apply(x, y));
                }
            }
            b"h" => {
                if let (Some(from), Some(to)) = (path.at, path.start) {
                    path.segments.push([from, to]);
                    path.at = Some(to);
                }
            }
            b"re" => {
                if let Some([x, y, wd, ht]) = op.nums::<4>() {
                    let corners = [ctm.apply(x, y), ctm.apply(x + wd, y), ctm.apply(x + wd, y + ht), ctm.apply(x, y + ht)];
                    path.rects.push(corners);
                    path.start = Some(corners[0]);
                    path.at = Some(corners[0]);
                }
            }
            b"S" | b"s" => {
                stroke(&mut w.out, &path);
                path = Path::default();
            }
            b"f" | b"F" | b"f*" => {
                fill(&mut w.out, &path);
                path = Path::default();
            }
            b"B" | b"B*" | b"b" | b"b*" => {
                stroke(&mut w.out, &path);
                fill(&mut w.out, &path);
                path = Path::default();
            }
            b"n" => path = Path::default(),
            b"Do" => {
                let Some(name) = op.name(0) else { continue };
                if w.forms >= crate::text::MAX_FORM_CALLS {
                    continue;
                }
                if let Some(form) = crate::text::form_call(w.doc, resources, name, st.ctm, &w.path) {
                    w.forms = w.forms.saturating_add(1);
                    if let Some(r) = form.obj {
                        w.path.push(r);
                    }
                    let mut inner = State { ctm: form.ctm, stack: Vec::new() };
                    walk(w, &form.data, &form.resources, &mut inner);
                    if form.obj.is_some() {
                        w.path.pop();
                    }
                }
            }
            _ => {}
        }
    }
}

/// A segment as a rule, if it is horizontal or vertical and long enough.
fn rule(a: (f64, f64), b: (f64, f64)) -> Option<Rule> {
    let (dx, dy) = ((b.0 - a.0).abs(), (b.1 - a.1).abs());
    if ![a.0, a.1, b.0, b.1].iter().all(|v| v.is_finite()) {
        return None;
    }
    if dy <= SLANT && dx >= MIN_RULE_LENGTH {
        let y = (a.1 + b.1) / 2.0;
        Some([a.0.min(b.0), y, a.0.max(b.0), y])
    } else if dx <= SLANT && dy >= MIN_RULE_LENGTH {
        let x = (a.0 + b.0) / 2.0;
        Some([x, a.1.min(b.1), x, a.1.max(b.1)])
    } else {
        None
    }
}

fn push(out: &mut Vec<Rule>, r: Option<Rule>) {
    if let Some(r) = r
        && out.len() < MAX_RULES
    {
        out.push(r);
    }
}

/// A stroked path: each straight segment, and each side of each rectangle.
fn stroke(out: &mut Vec<Rule>, path: &Path) {
    for [a, b] in &path.segments {
        push(out, rule(*a, *b));
    }
    for c in &path.rects {
        // Four corners: `k` and `(k + 1) % 4` are always below 4.
        for k in 0..4 {
            push(out, rule(c[k], c[(k + 1) % 4]));
        }
    }
}

/// A filled path: rectangles thin in one direction are a line along the other (a border drawn
/// as a box). Wider fills are shading, not rules.
fn fill(out: &mut Vec<Rule>, path: &Path) {
    for c in &path.rects {
        let xs = [c[0].0, c[1].0, c[2].0, c[3].0];
        let ys = [c[0].1, c[1].1, c[2].1, c[3].1];
        let (x0, x1) = (xs.iter().copied().fold(f64::MAX, f64::min), xs.iter().copied().fold(f64::MIN, f64::max));
        let (y0, y1) = (ys.iter().copied().fold(f64::MAX, f64::min), ys.iter().copied().fold(f64::MIN, f64::max));
        let (w, h) = (x1 - x0, y1 - y0);
        if h <= MAX_LINE_THICKNESS && w >= MIN_RULE_LENGTH {
            push(out, rule((x0, (y0 + y1) / 2.0), (x1, (y0 + y1) / 2.0)));
        } else if w <= MAX_LINE_THICKNESS && h >= MIN_RULE_LENGTH {
            push(out, rule(((x0 + x1) / 2.0, y0), ((x0 + x1) / 2.0, y1)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_become_rules_only_when_straight_and_long() {
        assert_eq!(rule((10.0, 5.0), (100.0, 5.2)), Some([10.0, 5.1, 100.0, 5.1]));
        assert_eq!(rule((7.0, 90.0), (7.0, 10.0)), Some([7.0, 10.0, 7.0, 90.0]));
        assert_eq!(rule((0.0, 0.0), (50.0, 50.0)), None, "diagonal");
        assert_eq!(rule((0.0, 0.0), (2.0, 0.0)), None, "too short");
        assert_eq!(rule((0.0, f64::NAN), (20.0, 0.0)), None);
    }
}
