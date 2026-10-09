//! Tables drawn with rules: the horizontal and vertical lines a page draws make a grid, and
//! the grid's boxes are the cells. A side of a box with no rule along it joins the box to its
//! neighbour, which is how merged cells (spanning columns or rows) are drawn. Text goes to the
//! cell its centre falls in.

use crate::{Block, Cell, Table, VMerge};

/// How far (points) two rules may be apart and still be one grid line; how far a rule may stop
/// short of the line it meets.
const TOL: f64 = 2.0;
/// A cell side counts as ruled when rules cover this much of it.
const COVER: f64 = 0.6;
/// Rules considered per page, and the largest grid a table may have.
const MAX_RULES: usize = 4000;
const MAX_ROWS: usize = 500;

/// A cell's grid box: first row, first column, last row, last column.
type Span = (usize, usize, usize, usize);

#[derive(Clone, Copy, Debug)]
struct Seg {
    /// The line's position (y of a horizontal rule, x of a vertical one) and its extent along it.
    at: f64,
    from: f64,
    to: f64,
}

/// Collinear segments that touch or overlap, merged: sorted by position, then extent.
fn merge(mut segs: Vec<Seg>) -> Vec<Seg> {
    segs.sort_by(|a, b| a.at.total_cmp(&b.at).then(a.from.total_cmp(&b.from)));
    let mut out: Vec<Seg> = Vec::new();
    for s in segs {
        match out.iter_mut().rev().take_while(|o| (o.at - s.at).abs() <= TOL).find(|o| s.from <= o.to + TOL && s.to >= o.from - TOL) {
            Some(o) => {
                o.from = o.from.min(s.from);
                o.to = o.to.max(s.to);
            }
            None => out.push(s),
        }
    }
    out
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while let Some(&p) = parent.get(i) {
        if p == i {
            break;
        }
        let gp = parent.get(p).copied().unwrap_or(p);
        if let Some(slot) = parent.get_mut(i) {
            *slot = gp;
        }
        i = p;
    }
    i
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb
        && let Some(slot) = parent.get_mut(ra)
    {
        *slot = rb;
    }
}

/// Distinct positions, sorted, clustered within `TOL`.
fn lines(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() <= TOL);
    v
}

/// How much of `[a, b]` the segments at `at` cover (0 to 1).
fn covered(segs: &[&Seg], at: f64, a: f64, b: f64) -> f64 {
    let len = (b - a).max(f64::EPSILON);
    let mut spans: Vec<(f64, f64)> =
        segs.iter().filter(|s| (s.at - at).abs() <= TOL).map(|s| (s.from.max(a), s.to.min(b))).filter(|(x, y)| y > x).collect();
    spans.sort_by(|x, y| x.0.total_cmp(&y.0));
    let (mut total, mut end) = (0.0, a);
    for (x, y) in spans {
        let x = x.max(end);
        if y > x {
            total += y - x;
            end = y;
        }
    }
    total / len
}

/// The tables the rules `rules` draw, filled with the text `fragments` (one line each). Returns
/// each table and the fragments it took.
pub(crate) fn ruled_tables(rules: &[[f64; 4]], fragments: &[Block]) -> Vec<(Table, Vec<usize>)> {
    if rules.len() > MAX_RULES {
        return Vec::new();
    }
    let h = merge(rules.iter().filter(|r| (r[1] - r[3]).abs() <= f64::EPSILON).map(|r| Seg { at: r[1], from: r[0], to: r[2] }).collect());
    let v =
        merge(rules.iter().filter(|r| (r[0] - r[2]).abs() <= f64::EPSILON && r[3] > r[1]).map(|r| Seg { at: r[0], from: r[1], to: r[3] }).collect());
    // Connected components of crossing rules: indices 0..h.len() are horizontal, then vertical.
    let mut parent: Vec<usize> = (0..h.len() + v.len()).collect();
    for (i, a) in h.iter().enumerate() {
        for (j, b) in v.iter().enumerate() {
            let crosses = b.at >= a.from - TOL && b.at <= a.to + TOL && a.at >= b.from - TOL && a.at <= b.to + TOL;
            if crosses {
                union(&mut parent, i, h.len() + j);
            }
        }
    }
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for i in 0..parent.len() {
        let r = find(&mut parent, i);
        match groups.iter_mut().find(|g| g.0 == r) {
            Some(g) => g.1.push(i),
            None => groups.push((r, vec![i])),
        }
    }
    let mut out = Vec::new();
    for (_, members) in groups {
        let hs: Vec<&Seg> = members.iter().filter_map(|&i| h.get(i)).collect();
        let vs: Vec<&Seg> = members.iter().filter_map(|&i| i.checked_sub(h.len()).and_then(|j| v.get(j))).collect();
        if hs.len() < 2 || vs.len() < 2 {
            continue;
        }
        if let Some(t) = grid_table(&hs, &vs, fragments) {
            out.push(t);
        }
    }
    out
}

/// One table from one group of crossing rules.
fn grid_table(hs: &[&Seg], vs: &[&Seg], fragments: &[Block]) -> Option<(Table, Vec<usize>)> {
    let xs = lines(vs.iter().map(|s| s.at).collect());
    let mut ys = lines(hs.iter().map(|s| s.at).collect());
    ys.reverse(); // top to bottom
    let (ncols, nrows) = (xs.len().checked_sub(1)?, ys.len().checked_sub(1)?);
    if ncols == 0 || nrows == 0 || ncols > crate::MAX_COLS || nrows > MAX_ROWS || ncols * nrows < 2 {
        return None;
    }
    let x = |j: usize| xs.get(j).copied().unwrap_or(0.0);
    let y = |i: usize| ys.get(i).copied().unwrap_or(0.0);
    // Grid boxes joined across sides no rule draws.
    let idx = |i: usize, j: usize| i * ncols + j;
    let mut parent: Vec<usize> = (0..nrows * ncols).collect();
    for i in 0..nrows {
        for j in 0..ncols {
            if j + 1 < ncols && covered(vs, x(j + 1), y(i + 1), y(i)) < COVER {
                union(&mut parent, idx(i, j), idx(i, j + 1));
            }
            if i + 1 < nrows && covered(hs, y(i + 1), x(j), x(j + 1)) < COVER {
                union(&mut parent, idx(i, j), idx(i + 1, j));
            }
        }
    }
    // Each region's box (rows r0..=r1, columns c0..=c1); a region that isn't a rectangle is
    // split back into its boxes.
    let mut region: Vec<Span> = vec![(0, 0, 0, 0); nrows * ncols];
    let mut roots: std::collections::HashMap<usize, (Span, usize)> = std::collections::HashMap::new();
    for i in 0..nrows {
        for j in 0..ncols {
            let r = find(&mut parent, idx(i, j));
            let e = roots.entry(r).or_insert(((i, j, i, j), 0));
            e.0 = (e.0.0.min(i), e.0.1.min(j), e.0.2.max(i), e.0.3.max(j));
            e.1 += 1;
        }
    }
    for i in 0..nrows {
        for j in 0..ncols {
            let r = find(&mut parent, idx(i, j));
            let b = roots.get(&r).map_or((i, j, i, j), |&(bx, n)| {
                let (r0, c0, r1, c1) = bx;
                if (r1 - r0 + 1) * (c1 - c0 + 1) == n { bx } else { (i, j, i, j) }
            });
            if let Some(slot) = region.get_mut(idx(i, j)) {
                *slot = b;
            }
        }
    }
    // Fragments by the box their centre falls in.
    let (left, right, top, bottom) = (x(0), x(ncols), y(0), y(nrows));
    let mut text_of: Vec<Vec<usize>> = vec![Vec::new(); nrows * ncols];
    let mut taken = Vec::new();
    for (k, f) in fragments.iter().enumerate() {
        let (cx, cy) = ((f.rect[0] + f.rect[2]) / 2.0, (f.rect[1] + f.rect[3]) / 2.0);
        if !(cx > left && cx < right && cy < top && cy > bottom) {
            continue;
        }
        let Some(j) = (0..ncols).find(|&j| cx >= x(j) && cx <= x(j + 1)) else { continue };
        let Some(i) = (0..nrows).find(|&i| cy <= y(i) && cy >= y(i + 1)) else { continue };
        let (r0, c0, _, _) = region.get(idx(i, j)).copied().unwrap_or((i, j, i, j));
        if let Some(list) = text_of.get_mut(idx(r0, c0)) {
            list.push(k);
            taken.push(k);
        }
    }
    let mut rows = Vec::with_capacity(nrows);
    for i in 0..nrows {
        let mut row = Vec::new();
        for j in 0..ncols {
            let (r0, c0, r1, c1) = region.get(idx(i, j)).copied().unwrap_or((i, j, i, j));
            if c0 != j {
                continue; // inside a cell that started further left
            }
            let span = c1 - c0 + 1;
            if r0 != i {
                row.push(Cell { span, vmerge: VMerge::Continue, ..Cell::default() });
                continue;
            }
            let mut cell = cell_from(text_of.get(idx(i, j)).map(Vec::as_slice).unwrap_or_default(), fragments);
            cell.span = span;
            cell.vmerge = if r1 > r0 { VMerge::Restart } else { VMerge::None };
            row.push(cell);
        }
        rows.push(row);
    }
    let cols: Vec<f64> = xs.iter().take(ncols).copied().collect();
    Some((Table { rect: [left, bottom, right, top], cols, rows }, taken))
}

/// A cell holding the fragments `ids`: lines top to bottom, each line's fragments in reading
/// order (right to left in a right-to-left cell), lines separated by `\n`.
fn cell_from(ids: &[usize], fragments: &[Block]) -> Cell {
    let mut fs: Vec<&Block> = ids.iter().filter_map(|&k| fragments.get(k)).collect();
    if fs.is_empty() {
        return Cell::default();
    }
    let rtl_chars: usize = fs.iter().filter(|f| f.rtl).map(|f| f.text.chars().count()).sum();
    let all_chars: usize = fs.iter().map(|f| f.text.chars().count()).sum();
    let rtl = rtl_chars * 2 > all_chars;
    // Lines: fragments whose vertical extents overlap by half the smaller height.
    fs.sort_by(|a, b| b.rect[3].total_cmp(&a.rect[3]));
    let mut lines: Vec<Vec<&Block>> = Vec::new();
    for f in fs {
        let same = lines.last().and_then(|l| l.first()).is_some_and(|g| {
            let overlap = g.rect[3].min(f.rect[3]) - g.rect[1].max(f.rect[1]);
            overlap > 0.5 * (g.rect[3] - g.rect[1]).min(f.rect[3] - f.rect[1])
        });
        match lines.last_mut() {
            Some(l) if same => l.push(f),
            _ => lines.push(vec![f]),
        }
    }
    let mut text = String::new();
    for l in &mut lines {
        l.sort_by(|a, b| if rtl { b.rect[0].total_cmp(&a.rect[0]) } else { a.rect[0].total_cmp(&b.rect[0]) });
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&l.iter().map(|f| f.text.trim()).collect::<Vec<_>>().join(" "));
    }
    let first = lines.first().and_then(|l| l.first());
    Cell {
        text,
        size: first.map_or(11.0, |f| f.size),
        bold: first.is_some_and(|f| f.bold),
        italic: first.is_some_and(|f| f.italic),
        span: 1,
        rtl,
        vmerge: VMerge::None,
        font: first.and_then(|f| f.font.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(text: &str, x: f64, y: f64) -> Block {
        Block { text: text.into(), rect: [x - 5.0, y - 4.0, x + 5.0, y + 4.0], size: 8.0, ..Block::default() }
    }

    /// A 2×2 grid at x 0/100/200, y 200/100/0, with `rules` added to the outer box.
    fn boxed(inner: &[[f64; 4]]) -> Vec<[f64; 4]> {
        let mut r = vec![[0.0, 0.0, 200.0, 0.0], [0.0, 200.0, 200.0, 200.0], [0.0, 0.0, 0.0, 200.0], [200.0, 0.0, 200.0, 200.0]];
        r.extend_from_slice(inner);
        r
    }

    #[test]
    fn a_full_grid_has_every_cell() {
        let rules = boxed(&[[0.0, 100.0, 200.0, 100.0], [100.0, 0.0, 100.0, 200.0]]);
        let frags = [frag("a", 50.0, 150.0), frag("b", 150.0, 150.0), frag("c", 50.0, 50.0), frag("d", 150.0, 50.0), frag("out", 300.0, 50.0)];
        let t = ruled_tables(&rules, &frags);
        assert_eq!(t.len(), 1);
        let (table, taken) = &t[0];
        assert_eq!(taken.len(), 4, "the fragment outside stays out");
        let texts: Vec<Vec<&str>> = table.rows.iter().map(|r| r.iter().map(|c| c.text.as_str()).collect()).collect();
        assert_eq!(texts, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn missing_sides_merge_cells() {
        // The vertical divider only runs through the bottom row: the top row is one cell.
        let rules = boxed(&[[0.0, 100.0, 200.0, 100.0], [100.0, 0.0, 100.0, 100.0]]);
        let t = ruled_tables(&rules, &[frag("head", 150.0, 150.0)]);
        let rows = &t[0].0.rows;
        assert_eq!(rows[0].len(), 1);
        assert_eq!(rows[0][0].span, 2);
        assert_eq!(rows[0][0].text, "head");
        assert_eq!(rows[1].len(), 2);
        // The horizontal divider only runs across the left column: the right column merges down.
        let rules = boxed(&[[0.0, 100.0, 100.0, 100.0], [100.0, 0.0, 100.0, 200.0]]);
        let rows = ruled_tables(&rules, &[]).remove(0).0.rows;
        assert_eq!(rows[0][1].vmerge, VMerge::Restart);
        assert_eq!(rows[1][1].vmerge, VMerge::Continue);
    }

    #[test]
    fn hostile_rules_give_no_table_and_no_panic() {
        assert!(ruled_tables(&[], &[]).is_empty());
        let nan = [[f64::NAN, 0.0, f64::NAN, 0.0], [0.0, f64::INFINITY, 0.0, f64::NEG_INFINITY], [5.0, 5.0, 5.0, 5.0]];
        assert!(ruled_tables(&nan, &[frag("x", 1.0, 1.0)]).is_empty());
        // Too many rules: skipped, not analysed.
        let many: Vec<[f64; 4]> = (0..MAX_RULES + 1).map(|i| [0.0, i as f64, 10.0, i as f64]).collect();
        assert!(ruled_tables(&many, &[]).is_empty());
        // A dense grid larger than a table may be.
        let mut dense = Vec::new();
        for i in 0..=crate::MAX_COLS + 2 {
            let p = i as f64 * 5.0;
            dense.push([0.0, p, 400.0, p]);
            dense.push([p, 0.0, p, 400.0]);
        }
        let _ = ruled_tables(&dense, &[]);
    }
}
