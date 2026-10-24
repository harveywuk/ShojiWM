//! Skips the render of a frame that would show nothing new.
//!
//! A DRM swapchain renders into the buffer it shows least recently, and
//! smithay's damage tracker repaints that buffer by its age: everything that
//! changed in the frames since it was last shown, even when nothing changed
//! since the frame on screen. So a redraw asked for with nothing new (another
//! output's animation, a runtime tick) still repaints, and commits, the last
//! change once or twice more: each 20 fps wallpaper step cost a 2560x1440
//! output about 2.4 full repaints. The frame on screen already shows those
//! elements, so the gate keeps it there instead.
//!
//! The gate's own tracker sees every frame at age 1, so it tells whether
//! anything changed since the frame drawn last. It must be told when that
//! frame will never be shown after all (`forget`), or a later redraw of the
//! same elements would leave the screen without them.

use smithay::backend::renderer::{damage::OutputDamageTracker, element::Element};

pub struct FrameGate<K, T> {
    tracker: OutputDamageTracker,
    /// How the frame drawn last was drawn, and what it left (`T`) for an
    /// unchanged frame to stand in with.
    drawn: Option<(K, T)>,
}

impl<K: PartialEq, T> FrameGate<K, T> {
    pub fn new(tracker: OutputDamageTracker) -> Self {
        Self { tracker, drawn: None }
    }

    /// What the frame drawn last left, when `elements`, drawn with `key`,
    /// would show nothing new over it; `None` when they must be drawn, and
    /// then `drawn` once they are. Call it for every frame: its tracker has to
    /// see them all.
    pub fn unchanged<E: Element>(&mut self, elements: &[E], key: &K) -> Option<&T> {
        if !matches!(self.tracker.damage_output(1, elements), Ok((None, _))) {
            // Until it is drawn, the frame on screen predates these changes.
            self.drawn = None;
            return None;
        }
        self.drawn.as_ref().filter(|(drawn_key, _)| drawn_key == key).map(|(_, left)| left)
    }

    /// The elements last passed to `unchanged` were drawn with `key`.
    pub fn drawn(&mut self, key: K, left: T) {
        self.drawn = Some((key, left));
    }

    /// The frame drawn last will not be shown after all: draw the next one.
    pub fn forget(&mut self) {
        self.drawn = None;
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Rectangle, Transform};

    use super::*;
    use crate::{backend::damage::DamageOnlyElement, ssd::LogicalRect};

    fn gate() -> FrameGate<u32, &'static str> {
        FrameGate::new(OutputDamageTracker::new((800, 600), 1.0, Transform::Normal))
    }

    #[test]
    fn skips_only_frames_with_nothing_new_over_the_one_drawn() {
        let mut gate = gate();
        let window = DamageOnlyElement::new(Rectangle::new((100, 100).into(), (300, 200).into()));
        let output = Rectangle::from_size((800, 600).into());
        let frame = |rects: &[LogicalRect]| {
            vec![crate::backend::damage::element_for_output("gate-test", rects, output), window.clone()]
        };

        assert_eq!(gate.unchanged(&frame(&[]), &0), None, "the first frame is drawn");
        gate.drawn(0, "first");
        assert_eq!(gate.unchanged(&frame(&[]), &0), Some(&"first"));
        assert_eq!(gate.unchanged(&frame(&[]), &0), Some(&"first"), "and again");
        assert_eq!(gate.unchanged(&frame(&[]), &1), None, "drawn differently, so drawn");
        gate.drawn(1, "second");

        assert_eq!(gate.unchanged(&frame(&[LogicalRect::new(10, 10, 20, 20)]), &1), None);
        // Not drawn after all (the render failed): the next frame, though it
        // changes nothing more, still has to show the change.
        assert_eq!(gate.unchanged(&frame(&[]), &1), None);
        gate.drawn(1, "third");
        assert_eq!(gate.unchanged(&frame(&[]), &1), Some(&"third"));

        // Drawn but dropped before it flipped.
        gate.forget();
        assert_eq!(gate.unchanged(&frame(&[]), &1), None);
        gate.drawn(1, "fourth");
        assert_eq!(gate.unchanged(&[window.clone()], &1), None, "an element gone is a change");
        gate.drawn(1, "fifth");
        assert_eq!(gate.unchanged(&[window], &1), Some(&"fifth"));
        crate::backend::damage::forget_output("gate-test");
    }
}
