use ratatui::layout::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub columns: usize,
    pub rows: usize,
}

pub fn choose_grid(task_count: usize, terminal: Rect) -> Grid {
    if task_count <= 1 {
        return Grid {
            columns: 1,
            rows: 1,
        };
    }

    let terminal_aspect = f64::from(terminal.width.max(1)) / f64::from(terminal.height.max(1));
    // A terminal cell is usually around twice as tall as it is wide. Targeting
    // character dimensions directly would make panes excessively horizontal.
    let target_grid_aspect = (terminal_aspect / 2.0).max(0.25);

    (1..=task_count)
        .map(|columns| {
            let rows = task_count.div_ceil(columns);
            let empty = columns * rows - task_count;
            let aspect = columns as f64 / rows as f64;
            let shape_cost = (aspect / target_grid_aspect).ln().abs();
            let empty_cost = empty as f64 / task_count as f64;
            (shape_cost + empty_cost, empty, Grid { columns, rows })
        })
        .min_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        })
        .map(|candidate| candidate.2)
        .unwrap()
}

pub fn pane_rects(area: Rect, grid: Grid, task_count: usize) -> Vec<Rect> {
    let widths = segments(area.width, grid.columns);
    let heights = segments(area.height, grid.rows);
    let mut rects = Vec::with_capacity(task_count);
    let mut y = area.y;

    for height in heights {
        let mut x = area.x;
        for &width in &widths {
            if rects.len() == task_count {
                return rects;
            }
            rects.push(Rect::new(x, y, width, height));
            x = x.saturating_add(width);
        }
        y = y.saturating_add(height);
    }
    rects
}

fn segments(total: u16, count: usize) -> Vec<u16> {
    let count = count.max(1) as u16;
    let base = total / count;
    let remainder = total % count;
    (0..count)
        .map(|index| base + u16::from(index < remainder))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_common_task_counts_sensibly() {
        let terminal = Rect::new(0, 0, 120, 40);
        assert_eq!(
            choose_grid(1, terminal),
            Grid {
                columns: 1,
                rows: 1
            }
        );
        assert_eq!(
            choose_grid(2, terminal),
            Grid {
                columns: 2,
                rows: 1
            }
        );
        assert_eq!(
            choose_grid(4, terminal),
            Grid {
                columns: 2,
                rows: 2
            }
        );
        assert_eq!(
            choose_grid(5, terminal),
            Grid {
                columns: 3,
                rows: 2
            }
        );
    }

    #[test]
    fn pane_rects_fill_the_grid_without_overlap() {
        let area = Rect::new(0, 0, 11, 7);
        let rects = pane_rects(
            area,
            Grid {
                columns: 2,
                rows: 2,
            },
            4,
        );
        assert_eq!(rects[0], Rect::new(0, 0, 6, 4));
        assert_eq!(rects[1], Rect::new(6, 0, 5, 4));
        assert_eq!(rects[2], Rect::new(0, 4, 6, 3));
        assert_eq!(rects[3], Rect::new(6, 4, 5, 3));
    }
}
