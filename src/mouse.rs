//! Pure hit-testing helpers mapping mouse coordinates to UI targets - list
//! rows, the list/preview divider, mosaic cells - kept free of App state so
//! they stay unit-testable; the event wiring lives in main.rs.

#![allow(dead_code)]

use ratatui::layout::{Position, Rect};

pub fn list_row_at(area: Rect, offset: usize, x: u16, y: u16, len: usize) -> Option<usize> {
    if area.width < 2 || area.height < 2 {
        return None;
    }
    let x_min = area.x + 1;
    let x_max = area.x + area.width - 2;
    let y_min = area.y + 1;
    let y_max = area.y + area.height - 2;
    if x < x_min || x > x_max || y < y_min || y > y_max {
        return None;
    }
    let idx = offset + (y - area.y - 1) as usize;
    if idx < len { Some(idx) } else { None }
}

pub fn on_divider(list_area: Rect, x: u16, y: u16) -> bool {
    let y_min = list_area.y;
    let y_max = list_area.y + list_area.height - 1;
    if y < y_min || y > y_max {
        return false;
    }
    x == list_area.x + list_area.width - 1 || x == list_area.x + list_area.width
}

pub fn drag_pct(body: Rect, x: u16) -> u16 {
    (((x.saturating_sub(body.x)) as u32 * 100) / body.width.max(1) as u32).clamp(20, 80) as u16
}

pub fn mosaic_cell_at(slots: &[Rect; 4], x: u16, y: u16) -> Option<usize> {
    let pos = Position { x, y };
    slots.iter().position(|slot| slot.contains(pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 20,
        }
    }

    #[test]
    fn list_row_at_first_row() {
        let a = area();
        assert_eq!(list_row_at(a, 0, a.x + 1, a.y + 1, 100), Some(0));
    }

    #[test]
    fn list_row_at_respects_offset() {
        let a = area();
        assert_eq!(list_row_at(a, 3, a.x + 1, a.y + 2, 100), Some(4));
    }

    #[test]
    fn list_row_at_top_border_is_none() {
        let a = area();
        assert_eq!(list_row_at(a, 0, a.x + 1, a.y, 100), None);
    }

    #[test]
    fn list_row_at_bottom_border_is_none() {
        let a = area();
        assert_eq!(list_row_at(a, 0, a.x + 1, a.y + a.height - 1, 100), None);
    }

    #[test]
    fn list_row_at_left_of_area_is_none() {
        let a = area();
        assert_eq!(list_row_at(a, 0, a.x, a.y + 1, 100), None);
    }

    #[test]
    fn list_row_at_right_of_area_is_none() {
        let a = area();
        assert_eq!(list_row_at(a, 0, a.x + a.width - 1, a.y + 1, 100), None);
    }

    #[test]
    fn list_row_at_far_outside_x_is_none() {
        let a = area();
        assert_eq!(list_row_at(a, 0, a.x + a.width + 10, a.y + 1, 100), None);
    }

    #[test]
    fn list_row_at_past_len_is_none() {
        let a = area();
        assert_eq!(list_row_at(a, 0, a.x + 1, a.y + 5, 2), None);
    }

    #[test]
    fn list_row_at_degenerate_width_is_none() {
        let mut a = area();
        a.width = 1;
        assert_eq!(list_row_at(a, 0, a.x, a.y + 1, 100), None);
    }

    #[test]
    fn list_row_at_degenerate_height_is_none() {
        let mut a = area();
        a.height = 1;
        assert_eq!(list_row_at(a, 0, a.x + 1, a.y, 100), None);
    }

    #[test]
    fn on_divider_hits_left_column() {
        let a = area();
        let right_border = a.x + a.width - 1;
        assert!(on_divider(a, right_border, a.y));
    }

    #[test]
    fn on_divider_hits_right_column() {
        let a = area();
        let right_border = a.x + a.width - 1;
        assert!(on_divider(a, right_border + 1, a.y));
    }

    #[test]
    fn on_divider_misses_past_columns() {
        let a = area();
        let right_border = a.x + a.width - 1;
        assert!(!on_divider(a, right_border + 2, a.y));
    }

    #[test]
    fn on_divider_misses_above_range() {
        let a = area();
        let right_border = a.x + a.width - 1;
        assert!(!on_divider(a, right_border, a.y.saturating_sub(1)));
    }

    #[test]
    fn on_divider_misses_below_range() {
        let a = area();
        let right_border = a.x + a.width - 1;
        assert!(!on_divider(a, right_border, a.y + a.height));
    }

    #[test]
    fn drag_pct_clamps_to_lower_bound() {
        let body = area();
        assert_eq!(drag_pct(body, body.x), 20);
    }

    #[test]
    fn drag_pct_clamps_to_upper_bound() {
        let body = area();
        assert_eq!(drag_pct(body, body.x + body.width), 80);
    }

    #[test]
    fn drag_pct_midpoint_is_unclamped() {
        let body = area();
        assert_eq!(drag_pct(body, body.x + body.width / 2), 50);
    }

    #[test]
    fn drag_pct_zero_width_body_does_not_panic() {
        let empty_body = Rect {
            x: 10,
            y: 5,
            width: 0,
            height: 20,
        };
        assert_eq!(drag_pct(empty_body, 15), 80);
    }

    fn quadrant_slots() -> [Rect; 4] {
        [
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            Rect {
                x: 10,
                y: 0,
                width: 10,
                height: 10,
            },
            Rect {
                x: 0,
                y: 10,
                width: 10,
                height: 10,
            },
            Rect {
                x: 10,
                y: 10,
                width: 10,
                height: 10,
            },
        ]
    }

    #[test]
    fn mosaic_cell_at_finds_cell() {
        let slots = quadrant_slots();
        assert_eq!(mosaic_cell_at(&slots, 5, 15), Some(2));
    }

    #[test]
    fn mosaic_cell_at_miss_is_none() {
        let slots = quadrant_slots();
        assert_eq!(mosaic_cell_at(&slots, 50, 50), None);
    }
}
