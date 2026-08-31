#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// Shrinks this rect by `amount` on every side, clamping width/
    /// height at 0 rather than going negative (an extreme gap setting
    /// on a tiny output shouldn't produce a rect with negative size —
    /// negative width/height is nonsensical for anything downstream
    /// that turns this into a real window resize).
    fn inset(&self, amount: i32) -> Rect {
        let w = (self.w - amount * 2).max(0);
        let h = (self.h - amount * 2).max(0);
        Rect { x: self.x + amount, y: self.y + amount, w, h }
    }
}

/// Which side of the master area the stack windows go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackSide {
    Right,
    Left,
    Bottom,
    Top,
}

impl Default for StackSide {
    fn default() -> Self {
        StackSide::Right
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilingConfig {
    /// Fraction (0.0–1.0) of the available width (or height, for
    /// `StackSide::Bottom`/`Top`) the master window takes. Clamped to
    /// `[0.1, 0.9]` in [`compute_layout`] — a ratio outside that range
    /// would produce a master or stack area too thin to be usable, and
    /// a config typo (e.g. `50` meant as a percentage, actually parsed
    /// as `50.0`) shouldn't be able to produce a window with zero
    /// usable width.
    pub master_ratio: f64,
    pub gaps_px: i32,
    pub stack_side: StackSide,
}

impl Default for TilingConfig {
    fn default() -> Self {
        Self { master_ratio: 0.55, gaps_px: 8, stack_side: StackSide::Right }
    }
}

/// Computes one [`Rect`] per window, in the same order windows were
/// given (the caller — `BlueState::apply_tiling_layout` — is
/// responsible for deciding *which* windows go in, and in what order;
/// this function only knows "N windows, this much space").
///
/// Returns an empty `Vec` for `window_count == 0`. Never panics —
/// every arithmetic path is checked/clamped so a pathological input
/// (a 1px-tall output, a master ratio of `f64::NAN`, `window_count` in
/// the millions) produces *some* rectangles rather than crashing the
/// compositor's render loop over a layout computation.
pub fn compute_layout(area: Rect, window_count: usize, config: &TilingConfig) -> Vec<Rect> {
    if window_count == 0 {
        return Vec::new();
    }

    let usable = area.inset(config.gaps_px);
    if usable.w <= 0 || usable.h <= 0 {
        // The output is too small (or gaps too large) for anything to
        // usefully render — hand back one degenerate rect per window
        // rather than an empty Vec, so callers that zip this against a
        // window list don't have to special-case "layout produced
        // fewer rects than windows".
        return vec![Rect::new(usable.x, usable.y, usable.w.max(0), usable.h.max(0)); window_count];
    }

    if window_count == 1 {
        return vec![usable];
    }

    let ratio = sanitize_ratio(config.master_ratio);
    let stack_count = window_count - 1;
    let gap = config.gaps_px.max(0);

    let (master, stack_area) = split_master_stack(usable, ratio, gap, config.stack_side);
    let mut rects = Vec::with_capacity(window_count);
    rects.push(master);
    rects.extend(stack_layout(stack_area, stack_count, gap, config.stack_side));
    rects
}

fn sanitize_ratio(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(0.1, 0.9)
    } else {
        TilingConfig::default().master_ratio
    }
}

/// Splits `area` into (master, stack) along the axis `stack_side`
/// implies, leaving one `gap`-wide seam between them.
fn split_master_stack(area: Rect, ratio: f64, gap: i32, side: StackSide) -> (Rect, Rect) {
    match side {
        StackSide::Right | StackSide::Left => {
            let master_w = ((area.w - gap) as f64 * ratio).round() as i32;
            let master_w = master_w.max(0).min(area.w);
            let stack_w = (area.w - master_w - gap).max(0);
            if side == StackSide::Right {
                let master = Rect::new(area.x, area.y, master_w, area.h);
                let stack = Rect::new(area.x + master_w + gap, area.y, stack_w, area.h);
                (master, stack)
            } else {
                let stack = Rect::new(area.x, area.y, stack_w, area.h);
                let master = Rect::new(area.x + stack_w + gap, area.y, master_w, area.h);
                (master, stack)
            }
        }
        StackSide::Bottom | StackSide::Top => {
            let master_h = ((area.h - gap) as f64 * ratio).round() as i32;
            let master_h = master_h.max(0).min(area.h);
            let stack_h = (area.h - master_h - gap).max(0);
            if side == StackSide::Bottom {
                let master = Rect::new(area.x, area.y, area.w, master_h);
                let stack = Rect::new(area.x, area.y + master_h + gap, area.w, stack_h);
                (master, stack)
            } else {
                let stack = Rect::new(area.x, area.y, area.w, stack_h);
                let master = Rect::new(area.x, area.y + stack_h + gap, area.w, master_h);
                (master, stack)
            }
        }
    }
}

/// Divides `area` evenly among `count` stacked windows, `gap` pixels
/// apart, along whichever axis is perpendicular to `side` (a
/// left/right stack divides vertically into rows; a top/bottom stack
/// divides horizontally into columns).
fn stack_layout(area: Rect, count: usize, gap: i32, side: StackSide) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let count_i32 = count as i32;

    match side {
        StackSide::Right | StackSide::Left => {
            let total_gaps = gap * (count_i32 - 1).max(0);
            let each_h = ((area.h - total_gaps) / count_i32).max(0);
            let mut rects = Vec::with_capacity(count);
            let mut y = area.y;
            for i in 0..count {
                // The last window absorbs any leftover pixels from
                // integer division, so the stack's total height always
                // exactly matches `area.h` instead of leaving a
                // rounding-error gap at the bottom.
                let h = if i == count - 1 { (area.y + area.h) - y } else { each_h };
                rects.push(Rect::new(area.x, y, area.w, h.max(0)));
                y += h + gap;
            }
            rects
        }
        StackSide::Bottom | StackSide::Top => {
            let total_gaps = gap * (count_i32 - 1).max(0);
            let each_w = ((area.w - total_gaps) / count_i32).max(0);
            let mut rects = Vec::with_capacity(count);
            let mut x = area.x;
            for i in 0..count {
                let w = if i == count - 1 { (area.x + area.w) - x } else { each_w };
                rects.push(Rect::new(x, area.y, w.max(0), area.h));
                x += w + gap;
            }
            rects
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };

    #[test]
    fn zero_windows_produces_nothing() {
        assert!(compute_layout(AREA, 0, &TilingConfig::default()).is_empty());
    }

    #[test]
    fn one_window_fills_the_whole_usable_area() {
        let cfg = TilingConfig { gaps_px: 10, ..Default::default() };
        let rects = compute_layout(AREA, 1, &cfg);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], Rect::new(10, 10, 1900, 1060));
    }

    #[test]
    fn two_windows_split_master_and_one_stack_window() {
        let cfg = TilingConfig { gaps_px: 0, master_ratio: 0.5, stack_side: StackSide::Right };
        let rects = compute_layout(AREA, 2, &cfg);
        assert_eq!(rects.len(), 2);
        // Master takes the left half, the lone stack window the right half.
        assert_eq!(rects[0], Rect::new(0, 0, 960, 1080));
        assert_eq!(rects[1], Rect::new(960, 0, 960, 1080));
    }

    #[test]
    fn stack_windows_are_evenly_divided_and_cover_the_full_height() {
        let cfg = TilingConfig { gaps_px: 0, master_ratio: 0.5, stack_side: StackSide::Right };
        let rects = compute_layout(AREA, 4, &cfg); // 1 master + 3 stacked
        assert_eq!(rects.len(), 4);
        let stack = &rects[1..];
        let total_h: i32 = stack.iter().map(|r| r.h).sum();
        assert_eq!(total_h, 1080, "stack windows must cover the full height with no gap, no overlap");
        // Every stack window starts exactly where the previous one ended.
        for pair in stack.windows(2) {
            assert_eq!(pair[0].y + pair[0].h, pair[1].y);
        }
    }

    #[test]
    fn gaps_are_applied_between_stack_windows_and_outer_edge() {
        let cfg = TilingConfig { gaps_px: 10, master_ratio: 0.5, stack_side: StackSide::Right };
        let rects = compute_layout(AREA, 3, &cfg); // 1 master + 2 stacked
        // Outer gap: nothing should touch x=0/y=0 or the far edges.
        assert!(rects.iter().all(|r| r.x >= 10 && r.y >= 10));
        // The seam between the two stack windows should be exactly one gap wide.
        let (a, b) = (rects[1], rects[2]);
        assert_eq!(b.y - (a.y + a.h), 10);
    }

    #[test]
    fn stack_side_left_puts_master_on_the_right() {
        let cfg = TilingConfig { gaps_px: 0, master_ratio: 0.5, stack_side: StackSide::Left };
        let rects = compute_layout(AREA, 2, &cfg);
        assert_eq!(rects[0].x, 960, "master should be on the right when stack_side is Left");
        assert_eq!(rects[1].x, 0, "stack should be on the left when stack_side is Left");
    }

    #[test]
    fn stack_side_bottom_splits_vertically() {
        let cfg = TilingConfig { gaps_px: 0, master_ratio: 0.6, stack_side: StackSide::Bottom };
        let rects = compute_layout(AREA, 2, &cfg);
        assert_eq!(rects[0], Rect::new(0, 0, 1920, 648)); // 1080 * 0.6
        assert_eq!(rects[1], Rect::new(0, 648, 1920, 432));
    }

    #[test]
    fn ratio_is_clamped_to_a_usable_range() {
        let extreme_low = TilingConfig { master_ratio: 0.0, gaps_px: 0, ..Default::default() };
        let rects = compute_layout(AREA, 2, &extreme_low);
        assert!(rects[0].w >= (AREA.w as f64 * 0.1) as i32 - 1, "clamped ratio should keep master usably wide");

        let extreme_high = TilingConfig { master_ratio: 5.0, gaps_px: 0, ..Default::default() };
        let rects = compute_layout(AREA, 2, &extreme_high);
        assert!(rects[1].w >= (AREA.w as f64 * 0.1) as i32 - 1, "clamped ratio should keep stack usably wide");
    }

    #[test]
    fn nan_ratio_falls_back_to_the_default_instead_of_producing_nan_geometry() {
        let cfg = TilingConfig { master_ratio: f64::NAN, gaps_px: 0, ..Default::default() };
        let rects = compute_layout(AREA, 2, &cfg);
        assert!(rects.iter().all(|r| r.w > 0 && r.h > 0));
    }

    #[test]
    fn tiny_output_degrades_gracefully_instead_of_panicking() {
        let cfg = TilingConfig { gaps_px: 1000, ..Default::default() }; // gaps bigger than the output
        let rects = compute_layout(Rect::new(0, 0, 100, 100), 3, &cfg);
        assert_eq!(rects.len(), 3);
        assert!(rects.iter().all(|r| r.w >= 0 && r.h >= 0));
    }

    #[test]
    fn many_stacked_windows_still_cover_the_full_area_with_no_overlap() {
        let cfg = TilingConfig { gaps_px: 4, master_ratio: 0.5, stack_side: StackSide::Right };
        let rects = compute_layout(AREA, 9, &cfg); // 1 master + 8 stacked
        assert_eq!(rects.len(), 9);
        let stack = &rects[1..];
        for pair in stack.windows(2) {
            assert!(pair[1].y >= pair[0].y + pair[0].h, "stack windows must never overlap");
        }
    }
}
