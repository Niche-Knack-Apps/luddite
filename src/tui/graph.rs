//! Lane layout for the commit graph, in the spirit of `git log --graph` and
//! lazygit: one row per commit, newest first; each lane is a column that
//! "expects" a particular commit next.

use git2::Oid;

#[derive(Clone, Debug)]
pub struct GraphRow {
    /// Two cells per lane: (glyph, lane index) — lane index drives the colour.
    pub cells: Vec<(char, usize)>,
}

/// `rows` must be a topological order with children before parents.
pub fn layout(rows: &[(Oid, Vec<Oid>)]) -> Vec<GraphRow> {
    let mut active: Vec<Option<Oid>> = Vec::new();
    let mut out = Vec::with_capacity(rows.len());

    for (oid, parents) in rows {
        let expecting: Vec<usize> = active
            .iter()
            .enumerate()
            .filter(|(_, x)| **x == Some(*oid))
            .map(|(i, _)| i)
            .collect();

        let node_lane = match expecting.first() {
            Some(&l) => l,
            None => match active.iter().position(Option::is_none) {
                Some(i) => {
                    active[i] = Some(*oid);
                    i
                }
                None => {
                    active.push(Some(*oid));
                    active.len() - 1
                }
            },
        };
        let closing: Vec<usize> = expecting
            .iter()
            .copied()
            .filter(|&l| l != node_lane)
            .collect();
        let before = active.clone();
        for &l in &closing {
            active[l] = None;
        }

        let mut opening: Vec<usize> = Vec::new();
        let mut joining: Vec<usize> = Vec::new();
        let mut it = parents.iter();
        active[node_lane] = it.next().copied();
        for p in it {
            if let Some(l) = active.iter().position(|x| *x == Some(*p)) {
                joining.push(l);
            } else {
                let slot = (node_lane + 1..active.len()).find(|&i| active[i].is_none());
                let l = match slot {
                    Some(i) => {
                        active[i] = Some(*p);
                        i
                    }
                    None => {
                        active.push(Some(*p));
                        active.len() - 1
                    }
                };
                opening.push(l);
            }
        }

        let width = before.len().max(active.len());
        let mut cells: Vec<(char, usize)> = (0..width * 2).map(|c| (' ', c / 2)).collect();
        for lane in 0..width {
            let was = before.get(lane).copied().flatten().is_some();
            let is = active.get(lane).copied().flatten().is_some();
            let glyph = if lane == node_lane {
                '●'
            } else if opening.contains(&lane) {
                if lane > node_lane {
                    '╮'
                } else {
                    '╭'
                }
            } else if closing.contains(&lane) {
                if lane > node_lane {
                    '╯'
                } else {
                    '╰'
                }
            } else if joining.contains(&lane) {
                if lane > node_lane {
                    '┤'
                } else {
                    '├'
                }
            } else if was || is {
                '│'
            } else {
                ' '
            };
            cells[lane * 2].0 = glyph;
        }

        // Horizontal runs between the node and every lane it connects to.
        let targets: Vec<usize> = opening
            .iter()
            .chain(closing.iter())
            .chain(joining.iter())
            .copied()
            .collect();
        if let (Some(&lo), Some(&hi)) = (
            targets.iter().chain(std::iter::once(&node_lane)).min(),
            targets.iter().chain(std::iter::once(&node_lane)).max(),
        ) {
            for cell in cells.iter_mut().take(hi * 2).skip(lo * 2 + 1) {
                match cell.0 {
                    ' ' => cell.0 = '─',
                    '│' => cell.0 = '┼',
                    _ => {}
                }
            }
        }

        // Trim trailing blank lanes so the column stays tight.
        while cells.len() >= 2 && cells[cells.len() - 2].0 == ' ' && cells[cells.len() - 1].0 == ' '
        {
            cells.truncate(cells.len() - 2);
        }
        out.push(GraphRow { cells });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_str(&format!("{:040x}", n)).unwrap()
    }

    fn node_lane(row: &GraphRow) -> usize {
        row.cells.iter().position(|c| c.0 == '●').unwrap() / 2
    }

    fn text(rows: &[GraphRow]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                r.cells
                    .iter()
                    .map(|c| c.0)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn linear_history_is_one_lane() {
        let rows = vec![
            (oid(3), vec![oid(2)]),
            (oid(2), vec![oid(1)]),
            (oid(1), vec![]),
        ];
        let g = layout(&rows);
        assert_eq!(text(&g), vec!["●", "●", "●"]);
    }

    #[test]
    fn merge_opens_a_lane_and_closes_it_at_the_fork_point() {
        // 4 = merge(3, 2); 3 → 1; 2 → 1; 1 root
        let rows = vec![
            (oid(4), vec![oid(3), oid(2)]),
            (oid(3), vec![oid(1)]),
            (oid(2), vec![oid(1)]),
            (oid(1), vec![]),
        ];
        let g = layout(&rows);
        assert_eq!(text(&g), vec!["●─╮", "● │", "│ ●", "●─╯"]);
        assert_eq!(node_lane(&g[2]), 1);
    }

    #[test]
    fn side_branch_tip_gets_its_own_lane() {
        // two tips: 3 (main) and 2 (feature), both on 1
        let rows = vec![
            (oid(3), vec![oid(1)]),
            (oid(2), vec![oid(1)]),
            (oid(1), vec![]),
        ];
        let g = layout(&rows);
        assert_eq!(text(&g), vec!["●", "│ ●", "●─╯"]);
    }
}
