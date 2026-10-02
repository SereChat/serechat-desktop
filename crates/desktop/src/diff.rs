//! Line diffs for the agent's file changes: the lines a change removes and
//! adds, a few unchanged lines around each change, and the part of an edited
//! line that actually changed.
//!
//! Lines are matched by a longest-common-subsequence table after trimming
//! the common start and end, which is exact for the edits an agent makes.
//! A change too large for the table (see [`MAX_CELLS`]) shows as every line
//! between the common start and end removed, then added.

/// Unchanged lines kept around each change.
pub const CONTEXT: usize = 3;
/// Largest matching table filled, in cells (16 MB of `u32`).
const MAX_CELLS: usize = 4_000_000;
/// Longest line kept for display, in bytes.
const MAX_LINE: usize = 1000;

/// What a diff line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// In both texts.
    Same,
    /// Only in the old text.
    Removed,
    /// Only in the new text.
    Added,
    /// Unchanged lines left out; [`Line::skipped`] says how many.
    Skipped,
}

/// One row of a diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// What it is.
    pub kind: Kind,
    /// 1-based line number in the old text; 0 when it has none or numbers are unknown.
    pub old: usize,
    /// 1-based line number in the new text; 0 likewise.
    pub new: usize,
    /// The line, tabs expanded.
    pub text: String,
    /// The bytes of `text` that differ from the paired line on the other side
    /// of an edit.
    pub changed: Option<(usize, usize)>,
    /// Lines a [`Kind::Skipped`] row stands for.
    pub skipped: usize,
}

/// A whole diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// The rows, in order.
    pub lines: Vec<Line>,
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
}

/// Diffs `old` against `new`. `numbered`: the texts are whole files, so line
/// numbers mean something (not just snippets).
#[must_use]
pub fn diff(old: &str, new: &str, numbered: bool) -> Diff {
    let a: Vec<String> = old.lines().map(|l| l.replace('\t', "    ")).collect();
    let b: Vec<String> = new.lines().map(|l| l.replace('\t', "    ")).collect();
    let ops = matches(&a, &b);

    // Changes, with the unchanged lines near them.
    let near = |i: usize| ops[i.saturating_sub(CONTEXT)..(i + CONTEXT + 1).min(ops.len())].iter().any(|op| op.0 != Kind::Same);
    let mut out = Diff::default();
    let mut index = 0;
    while index < ops.len() {
        let (kind, ai, bi) = ops[index];
        if kind == Kind::Same && !near(index) {
            let start = index;
            while index < ops.len() && ops[index].0 == Kind::Same && !near(index) {
                index += 1;
            }
            out.lines.push(Line { kind: Kind::Skipped, old: 0, new: 0, text: String::new(), changed: None, skipped: index - start });
            continue;
        }
        let text = if kind == Kind::Added { &b[bi] } else { &a[ai] };
        let number = |n: usize, has: bool| if numbered && has { n + 1 } else { 0 };
        out.lines.push(Line {
            kind,
            old: number(ai, kind != Kind::Added),
            new: number(bi, kind != Kind::Removed),
            text: text.clone(),
            changed: None,
            skipped: 0,
        });
        match kind {
            Kind::Added => out.added += 1,
            Kind::Removed => out.removed += 1,
            Kind::Same | Kind::Skipped => {}
        }
        index += 1;
    }
    mark_changes(&mut out.lines);
    for line in &mut out.lines {
        if line.text.len() > MAX_LINE {
            let mut cut = MAX_LINE;
            while !line.text.is_char_boundary(cut) {
                cut -= 1;
            }
            line.text.truncate(cut);
            line.text.push('…');
            line.changed = line.changed.map(|(s, e)| (s.min(cut), e.min(cut))).filter(|(s, e)| s < e);
        }
    }
    out
}

/// The edit script turning `old` into `new`: `(kind, index in old, index in new)`,
/// removals before additions within each change.
fn matches(old: &[String], new: &[String]) -> Vec<(Kind, usize, usize)> {
    let prefix = old.iter().zip(new).take_while(|(x, y)| x == y).count();
    let suffix = old[prefix..].iter().rev().zip(new[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (mid_a, mid_b) = (&old[prefix..old.len() - suffix], &new[prefix..new.len() - suffix]);
    let mut ops: Vec<(Kind, usize, usize)> = (0..prefix).map(|i| (Kind::Same, i, i)).collect();
    let (rows, cols) = (mid_a.len(), mid_b.len());
    if rows.saturating_mul(cols) > MAX_CELLS || rows == 0 || cols == 0 {
        ops.extend((0..rows).map(|i| (Kind::Removed, prefix + i, prefix)));
        ops.extend((0..cols).map(|j| (Kind::Added, prefix + rows, prefix + j)));
    } else {
        // table[i][j]: common lines of mid_a[i..] and mid_b[j..].
        let width = cols + 1;
        let mut table = vec![0u32; (rows + 1) * width];
        for i in (0..rows).rev() {
            for j in (0..cols).rev() {
                table[i * width + j] =
                    if mid_a[i] == mid_b[j] { table[(i + 1) * width + j + 1] + 1 } else { table[(i + 1) * width + j].max(table[i * width + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < rows || j < cols {
            if i < rows && j < cols && mid_a[i] == mid_b[j] {
                ops.push((Kind::Same, prefix + i, prefix + j));
                (i, j) = (i + 1, j + 1);
            } else if j == cols || (i < rows && table[(i + 1) * width + j] >= table[i * width + j + 1]) {
                ops.push((Kind::Removed, prefix + i, prefix + j));
                i += 1;
            } else {
                ops.push((Kind::Added, prefix + i, prefix + j));
                j += 1;
            }
        }
    }
    ops.extend((0..suffix).map(|k| (Kind::Same, old.len() - suffix + k, new.len() - suffix + k)));
    ops
}

/// Pairs the removed and added lines of each change in order and marks the
/// part of each pair that differs, when the lines share a start or an end.
fn mark_changes(lines: &mut [Line]) {
    let mut i = 0;
    while i < lines.len() {
        let removed = lines[i..].iter().take_while(|l| l.kind == Kind::Removed).count();
        let added = lines[i + removed..].iter().take_while(|l| l.kind == Kind::Added).count();
        if removed == 0 || added == 0 {
            i += removed.max(1);
            continue;
        }
        for k in 0..removed.min(added) {
            let (old, new) = (&lines[i + k].text, &lines[i + removed + k].text);
            let prefix: usize = old.chars().zip(new.chars()).take_while(|(x, y)| x == y).map(|(c, _)| c.len_utf8()).sum();
            let room = old.len().min(new.len()) - prefix;
            let suffix: usize = old[prefix..].chars().rev().zip(new[prefix..].chars().rev()).take_while(|(x, y)| x == y).map(|(c, _)| c.len_utf8()).sum();
            let suffix = suffix.min(room);
            if prefix + suffix == 0 {
                continue;
            }
            let (old_len, new_len) = (old.len(), new.len());
            let range = |len: usize| Some((prefix, len - suffix)).filter(|(s, e)| s < e);
            lines[i + k].changed = range(old_len);
            lines[i + removed + k].changed = range(new_len);
        }
        i += removed + added;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(d: &Diff) -> String {
        d.lines
            .iter()
            .map(|l| match l.kind {
                Kind::Same => ' ',
                Kind::Removed => '-',
                Kind::Added => '+',
                Kind::Skipped => '~',
            })
            .collect()
    }

    #[test]
    fn changes_keep_their_context() {
        let old = (1..=20).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let new = old.replace("line 10\n", "line ten\n");
        let d = diff(&old, &new, true);
        assert_eq!(kinds(&d), "~   -+   ~");
        assert_eq!((d.lines[0].skipped, d.lines[9].skipped), (6, 7));
        let (removed, added) = (&d.lines[4], &d.lines[5]);
        assert_eq!((removed.old, removed.new, added.old, added.new), (10, 0, 0, 10));
        assert_eq!((removed.changed, added.changed), (Some((5, 7)), Some((5, 8))), "only the number changed");
        assert_eq!((d.added, d.removed), (1, 1));
    }

    #[test]
    fn new_files_and_snippets() {
        let d = diff("", "a\nb\n", true);
        assert_eq!(kinds(&d), "++");
        assert_eq!((d.lines[1].old, d.lines[1].new), (0, 2));
        let snippet = diff("x\ty", "x\ty\nz", false);
        assert_eq!(kinds(&snippet), " +");
        assert_eq!(snippet.lines[0].text, "x    y");
        assert!(snippet.lines.iter().all(|l| l.old == 0 && l.new == 0), "snippets have no line numbers");
        let same = diff("same\n", "same\n", true);
        assert_eq!((same.added, same.removed, same.lines.len()), (0, 0, 1), "one skipped row");
    }

    #[test]
    fn interleaved_edits_match_lines() {
        let d = diff("a\nb\nc\nd\n", "a\nB\nc\nd\ne\n", true);
        assert_eq!(kinds(&d), " -+  +");
        assert_eq!(d.lines[2].changed, None, "lines with nothing in common are not marked");
    }

    #[test]
    fn hostile_input() {
        // Multi-byte characters around the change never split.
        let d = diff("héllo wörld", "héllo wérld", false);
        assert_eq!(d.lines[0].changed, Some((8, 10)));
        assert_eq!(&d.lines[1].text[8..10], "é");
        // A long line is cut on a character boundary.
        let long = "é".repeat(800);
        let d = diff("", &long, false);
        assert!(d.lines[0].text.ends_with('…') && d.lines[0].text.len() <= MAX_LINE + 3);
        // Too big for the table: still correct, just coarser.
        let old = (0..3000).map(|n| format!("a{n}")).collect::<Vec<_>>().join("\n");
        let new = (0..3000).map(|n| format!("b{n}")).collect::<Vec<_>>().join("\n");
        let d = diff(&old, &new, true);
        assert_eq!((d.removed, d.added), (3000, 3000));
    }
}
