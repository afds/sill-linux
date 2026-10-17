//! A window Aa sized goes back when it stops streaming: Sill for Mac's `WindowRestore` rule
//! (issue #57), ported. Pure.
//!
//! - unmoved and at Sill's size: back to its whole original frame;
//! - moved here since (at Sill's size, its top-left corner elsewhere; at another size, both edges
//!   of an axis gone the same way): its own size where it is;
//! - resized here since: left as it is;
//! - full screen: left; gone: nothing.

use crate::shell::Frame;

const TOLERANCE: i32 = 2;

/// x, y, width, height (logical pixels).
pub type Rect = (i32, i32, i32, i32);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Record {
    /// The frame before Sill first touched it.
    pub original: Frame,
    /// Where Sill last left it, as the app took it (read back after the resize).
    pub set: Rect,
    /// The size Sill last asked for (an app may apply it after the read-back).
    pub asked: (i32, i32),
    /// It was away from where Sill left it at a later fit: from then on it keeps its place.
    pub moved: bool,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Gone,
    /// Already as it was.
    Home,
    /// The app never showed Sill's size: the original written again costs nothing.
    Settle(Frame),
    Leave(&'static str),
    /// To this frame: the original (`whole`), or its own size where it was moved.
    Restore(Frame, bool),
}

fn rect(f: &Frame) -> Rect {
    (f.x, f.y, f.width, f.height)
}

fn near(a: i32, b: i32) -> bool {
    (a - b).abs() <= TOLERANCE
}

fn same_size(a: Rect, w: i32, h: i32) -> bool {
    near(a.2, w) && near(a.3, h)
}

fn same_origin(a: Rect, b: Rect) -> bool {
    near(a.0, b.0) && near(a.1, b.1)
}

/// Both edges of an axis gone the same way, more than the tolerance each.
fn shifted(a: Rect, b: Rect) -> bool {
    let went = |n: i32, f: i32| n.abs() > TOLERANCE && f.abs() > TOLERANCE && (n > 0) == (f > 0);
    went(b.0 - a.0, (b.0 + b.2) - (a.0 + a.2)) || went(b.1 - a.1, (b.1 + b.3) - (a.1 + a.3))
}

pub fn moved_since(r: &Record, now: Rect) -> bool {
    if same_size(now, r.set.2, r.set.3) || same_size(now, r.asked.0, r.asked.1) {
        return !same_origin(now, r.set);
    }
    shifted(r.set, now) && shifted((r.set.0, r.set.1, r.asked.0, r.asked.1), now)
}

/// A fit: from `before`, the app took `after`, asked for `asked`. The first makes the record; a
/// later one keeps the original and notes a move since the last.
pub fn fitted(r: Option<Record>, before: Frame, after: Rect, asked: (i32, i32)) -> Record {
    match r {
        None => Record { original: before, set: after, asked, moved: false },
        Some(mut r) => {
            if moved_since(&r, rect(&before)) {
                r.moved = true;
            }
            r.set = after;
            r.asked = asked;
            r
        }
    }
}

/// `current`: the window's frame and whether it is full screen; None when it is gone.
// shortcut: a moved window's way back is not kept on its monitor (Sill's `keep`); add it if a
// window ends up off screen.
pub fn decide(r: &Record, current: Option<(Frame, bool)>) -> Decision {
    let Some((now, full_screen)) = current else { return Decision::Gone };
    if full_screen {
        return Decision::Leave("it is full screen");
    }
    let (o, c) = (rect(&r.original), rect(&now));
    if same_origin(c, o) && same_size(c, o.2, o.3) && now.maximized == r.original.maximized {
        let asked_other = !same_size(o, r.asked.0, r.asked.1);
        return if asked_other && same_size(r.set, o.2, o.3) { Decision::Settle(r.original) } else { Decision::Home };
    }
    if !(same_size(c, r.set.2, r.set.3) || same_size(c, r.asked.0, r.asked.1)) {
        return Decision::Leave("it was resized here");
    }
    if !r.moved && same_origin(c, r.set) {
        return Decision::Restore(r.original, true);
    }
    let target = Frame { x: now.x, y: now.y, ..r.original };
    if same_origin(c, rect(&target)) && same_size(c, target.width, target.height) && !target.maximized {
        Decision::Home
    } else {
        Decision::Restore(target, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(x: i32, y: i32, w: i32, h: i32) -> Frame {
        Frame { x, y, width: w, height: h, maximized: false }
    }

    #[test]
    fn the_rule() {
        let original = f(100, 100, 800, 600);
        let r = fitted(None, original, (100, 100, 1180, 760), (1180, 760));
        assert_eq!(decide(&r, None), Decision::Gone);
        assert_eq!(decide(&r, Some((f(100, 100, 1180, 760), true))), Decision::Leave("it is full screen"));
        assert_eq!(decide(&r, Some((f(100, 100, 1180, 760), false))), Decision::Restore(original, true), "unmoved: whole");
        assert_eq!(decide(&r, Some((f(101, 99, 1181, 760), false))), Decision::Restore(original, true), "within 2 px");
        assert_eq!(decide(&r, Some((f(300, 50, 1180, 760), false))), Decision::Restore(f(300, 50, 800, 600), false), "moved: its size where it is");
        assert_eq!(decide(&r, Some((f(100, 100, 1000, 700), false))), Decision::Leave("it was resized here"));
        assert_eq!(decide(&r, Some((original, false))), Decision::Home);
        // An app that kept its own size: the original written again.
        let kept = fitted(None, original, (100, 100, 800, 600), (1180, 760));
        assert_eq!(decide(&kept, Some((original, false))), Decision::Settle(original));
        // ...and one that applied the size late is at the asked size by the stop.
        assert_eq!(decide(&kept, Some((f(100, 100, 1180, 760), false))), Decision::Restore(original, true));
        // A maximized original goes back maximized.
        let max = Frame { maximized: true, ..f(0, 0, 1280, 800) };
        let rm = fitted(None, max, (0, 0, 1180, 760), (1180, 760));
        assert_eq!(decide(&rm, Some((f(0, 0, 1180, 760), false))), Decision::Restore(max, true));
    }

    #[test]
    fn moves_and_resizes_between_fits() {
        let original = f(100, 100, 800, 600);
        let r = fitted(None, original, (100, 100, 1180, 760), (1180, 760));
        // Dragged from its left edge, then fitted again: no move, still goes back whole.
        assert!(!moved_since(&r, (40, 100, 1240, 760)));
        let r2 = fitted(Some(r), f(40, 100, 1240, 760), (40, 100, 1180, 760), (1180, 760));
        assert!(!r2.moved);
        assert_eq!(decide(&r2, Some((f(40, 100, 1180, 760), false))), Decision::Restore(original, true));
        // Moved, then fitted again: keeps its place from then on.
        let r3 = fitted(Some(r), f(400, 100, 1180, 760), (400, 100, 1180, 760), (1180, 760));
        assert!(r3.moved);
        assert_eq!(decide(&r3, Some((f(400, 100, 1180, 760), false))), Decision::Restore(f(400, 100, 800, 600), false));
        assert_eq!(r3.original, original, "the original stays the first frame");
    }
}
