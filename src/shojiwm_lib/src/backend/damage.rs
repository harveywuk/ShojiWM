use std::{cell::RefCell, collections::HashMap};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
        gles::{GlesError, GlesFrame, GlesRenderer},
        utils::{CommitCounter, DamageBag, DamageSet, DamageSnapshot, OpaqueRegions},
    },
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale},
};

use crate::ssd::LogicalRect;

/// Draws nothing; makes the damage tracker repaint `rect`, or, as an
/// output's [`element_for_output`], whatever its history holds.
#[derive(Clone)]
pub struct DamageOnlyElement {
    id: Id,
    rect: Rectangle<i32, Logical>,
    /// An output's damage element: its damage so far, and the output's own
    /// rect, all of which is damage once a tracker falls behind the history.
    history: Option<(DamageSnapshot<i32, Logical>, Rectangle<i32, Logical>)>,
}

impl std::fmt::Debug for DamageOnlyElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DamageOnlyElement")
            .field("id", &self.id)
            .field("rect", &self.rect)
            .field("commit", &self.current_commit())
            .finish()
    }
}

impl DamageOnlyElement {
    pub fn new(rect: Rectangle<i32, Logical>) -> Self {
        Self {
            id: Id::new(),
            rect,
            history: None,
        }
    }
}

/// An output's damage element, kept from frame to frame.
struct OutputDamage {
    id: Id,
    bag: DamageBag<i32, Logical>,
}

/// Commits a tracker may fall behind before it repaints the whole output.
const OUTPUT_DAMAGE_HISTORY: usize = 16;

thread_local! {
    static OUTPUT_DAMAGE: RefCell<HashMap<String, OutputDamage>> = RefCell::new(HashMap::new());
}

/// `rects` (global logical) as damage on output `name`, through one element
/// that stays in its scene: the same id, a fixed pixel at the output's
/// origin, its damage carried by commits. A fresh element per frame instead
/// moves everything under it down the stack, which the damage tracker
/// repaints in full, and repaints in full again the frame after, when it is
/// gone. Its damage is the output's, not its pixel's: the tracker clips
/// element damage to the output only. One pixel keeps it from overlapping
/// overlay-plane candidates.
pub fn element_for_output(
    name: &str,
    rects: &[LogicalRect],
    output_geo: Rectangle<i32, Logical>,
) -> DamageOnlyElement {
    let damage = elements_for_output(rects, output_geo).into_iter().map(|element| element.rect);
    OUTPUT_DAMAGE.with_borrow_mut(|outputs| {
        let output = outputs.entry(name.to_owned()).or_insert_with(|| OutputDamage {
            id: Id::new(),
            bag: DamageBag::new(OUTPUT_DAMAGE_HISTORY),
        });
        let damage = damage.collect::<Vec<_>>();
        if !damage.is_empty() {
            output.bag.add(damage);
        }
        DamageOnlyElement {
            id: output.id.clone(),
            rect: Rectangle::from_size((1, 1).into()),
            history: Some((output.bag.snapshot(), Rectangle::from_size(output_geo.size))),
        }
    })
}

/// Forget output `name`'s damage element (the output went away).
pub fn forget_output(name: &str) {
    OUTPUT_DAMAGE.with_borrow_mut(|outputs| outputs.remove(name));
}

impl Element for DamageOnlyElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.history
            .as_ref()
            .map_or_else(CommitCounter::default, |(history, _)| history.current_commit())
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(
            self.rect
                .size
                .to_f64()
                .to_buffer(1.0, smithay::utils::Transform::Normal),
        )
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.rect.to_physical_precise_round(scale)
    }

    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        let Some((history, output)) = &self.history else {
            if commit != Some(self.current_commit()) {
                return DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)]);
            }
            return DamageSet::default();
        };
        match history.damage_since(commit) {
            Some(damage) => damage.iter().map(|rect| rect.to_physical_precise_up(scale)).collect(),
            None => DamageSet::from_slice(&[output.to_physical_precise_up(scale)]),
        }
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        0.0
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for DamageOnlyElement {
    fn draw(
        &self,
        _frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), GlesError> {
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

pub fn elements_for_output(
    rects: &[LogicalRect],
    output_geo: Rectangle<i32, Logical>,
) -> Vec<DamageOnlyElement> {
    rects
        .iter()
        .filter_map(|rect| {
            let left = rect.x.max(output_geo.loc.x);
            let top = rect.y.max(output_geo.loc.y);
            let right = (rect.x + rect.width).min(output_geo.loc.x + output_geo.size.w);
            let bottom = (rect.y + rect.height).min(output_geo.loc.y + output_geo.size.h);
            (right > left && bottom > top).then(|| {
                DamageOnlyElement::new(Rectangle::new(
                    Point::from((left - output_geo.loc.x, top - output_geo.loc.y)),
                    (right - left, bottom - top).into(),
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use smithay::{backend::renderer::damage::OutputDamageTracker, utils::Transform};

    use super::*;

    /// What an age-1 tracker repaints for one frame of `elements`.
    fn damage(tracker: &mut OutputDamageTracker, elements: &[DamageOnlyElement]) -> Vec<Rectangle<i32, Physical>> {
        tracker.damage_output(1, elements).unwrap().0.cloned().unwrap_or_default()
    }

    #[test]
    fn output_element_damages_only_its_rects_and_never_the_frame_after() {
        let output = Rectangle::from_size((800, 600).into());
        let mut tracker = OutputDamageTracker::new((800, 600), 1.0, Transform::Normal);
        // Something under it, which must never be repainted because it moved down the stack.
        let below = DamageOnlyElement::new(Rectangle::new((100, 100).into(), (300, 200).into()));
        let frame = |rects: &[LogicalRect]| vec![element_for_output("test-a", rects, output), below.clone()];

        assert!(!damage(&mut tracker, &frame(&[])).is_empty(), "first frame repaints");
        assert!(damage(&mut tracker, &frame(&[])).is_empty());
        let clock = LogicalRect::new(45, 94, 202, 202);
        assert_eq!(damage(&mut tracker, &frame(&[clock])), vec![Rectangle::new((45, 94).into(), (202, 202).into())]);
        assert!(damage(&mut tracker, &frame(&[])).is_empty(), "no follow-up frame");
        // Clipped to the output, which may sit anywhere in the layout.
        let offset = Rectangle::new((800, 0).into(), (800, 600).into());
        let mut offset_tracker = OutputDamageTracker::new((800, 600), 1.0, Transform::Normal);
        let offset_frame = |rects: &[LogicalRect]| vec![element_for_output("test-b", rects, offset)];
        damage(&mut offset_tracker, &offset_frame(&[]));
        assert_eq!(
            damage(&mut offset_tracker, &offset_frame(&[LogicalRect::new(700, 10, 200, 20)])),
            vec![Rectangle::new((0, 10).into(), (100, 20).into())]
        );
    }

    #[test]
    fn output_element_repaints_everything_for_a_tracker_that_fell_behind() {
        let output = Rectangle::from_size((800, 600).into());
        let mut tracker = OutputDamageTracker::new((800, 600), 1.0, Transform::Normal);
        damage(&mut tracker, &[element_for_output("test-c", &[], output)]);
        for _ in 0..=OUTPUT_DAMAGE_HISTORY {
            element_for_output("test-c", &[LogicalRect::new(0, 0, 10, 10)], output);
        }
        let element = element_for_output("test-c", &[], output);
        assert_eq!(damage(&mut tracker, &[element]), vec![Rectangle::from_size((800, 600).into())]);
    }
}
