//! A small line diff (longest common subsequence) for commit messages —
//! they are short, so the quadratic table is nothing.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffLine {
    Same(String),
    Del(String),
    Add(String),
}

pub fn diff_lines(before: &str, after: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let (n, m) = (a.len(), b.len());
    // lcs[i][j] = length of LCS of a[i..] and b[j..]
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(n.max(m));
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine::Same(a[i].to_string()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine::Del(a[i].to_string()));
            i += 1;
        } else {
            out.push(DiffLine::Add(b[j].to_string()));
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine::Del(a[i].to_string()));
        i += 1;
    }
    while j < m {
        out.push(DiffLine::Add(b[j].to_string()));
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_trailer_shows_as_deletions_only() {
        let before = "fix: thing\n\nBody.\n\nCo-Authored-By: Claude <noreply@anthropic.com>\n";
        let after = "fix: thing\n\nBody.\n";
        let d = diff_lines(before, after);
        assert_eq!(
            d,
            vec![
                DiffLine::Same("fix: thing".into()),
                DiffLine::Same("".into()),
                DiffLine::Same("Body.".into()),
                DiffLine::Del("".into()),
                DiffLine::Del("Co-Authored-By: Claude <noreply@anthropic.com>".into()),
            ]
        );
    }

    #[test]
    fn identical_is_all_same() {
        let d = diff_lines("a\nb\n", "a\nb\n");
        assert!(d.iter().all(|l| matches!(l, DiffLine::Same(_))));
        assert_eq!(d.len(), 2);
    }
}
