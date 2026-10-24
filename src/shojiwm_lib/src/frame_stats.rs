//! Per-output frame counters behind `shoji_wm --frame-stats`.
//!
//! Kept by the tty backend, cumulative since the compositor started: take two
//! readings and subtract them for rates. Read by the control socket's own
//! thread, so a busy event loop still answers.
//!
//! [`DamageProbe`] (`SHOJI_FRAME_DAMAGE_DEBUG`) logs which elements of a frame
//! changed since the one before: what an age-1 damage tracker would redraw.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
    time::Duration,
};

use serde::Serialize;
use smithay::{
    backend::renderer::{
        damage::OutputDamageTracker,
        element::{Element, Id},
        utils::CommitCounter,
    },
    output::Output,
    utils::{Physical, Rectangle, Scale},
};
use tracing::info;

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct OutputFrames {
    /// Frames committed to the display (page flips queued).
    pub committed: u64,
    /// Renders that found nothing to change, so committed nothing.
    pub empty: u64,
    /// Frames the GPU was still drawing when their render returned, committed
    /// once their fence signalled.
    pub held_for_gpu: u64,
    /// Frames waited for on the main thread instead (a fence that could not
    /// be exported, or a tearing flip).
    pub waited_for_gpu: u64,
    /// From holding a frame to its fence signalling, summed, in microseconds.
    pub gpu_wait_us: u64,
}

static FRAMES: Mutex<BTreeMap<String, OutputFrames>> = Mutex::new(BTreeMap::new());

pub fn record(output: &str, update: impl FnOnce(&mut OutputFrames)) {
    let mut frames = FRAMES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match frames.get_mut(output) {
        Some(counters) => update(counters),
        None => update(frames.entry(output.to_owned()).or_default()),
    }
}

pub fn record_gpu_wait(output: &str, waited: Duration) {
    record(output, |frames| {
        frames.held_for_gpu += 1;
        frames.gpu_wait_us += waited.as_micros() as u64;
    });
}

/// Every output's counters, as one line of JSON: `{"DP-1": {...}, ...}`.
pub fn json() -> String {
    let frames = FRAMES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    serde_json::to_string(&*frames).unwrap_or_else(|_| "{}".to_owned())
}

/// One output's elements as last drawn, to tell which changed.
#[derive(Default)]
pub struct DamageProbe {
    /// By id and occurrence: an element can be drawn more than once a frame.
    last: HashMap<(Id, usize), (CommitCounter, Rectangle<i32, Physical>)>,
    frame: u64,
    /// What smithay's tracker would redraw at buffer age 1.
    tracker: Option<OutputDamageTracker>,
}

impl DamageProbe {
    /// Compare `elements` with the previous frame's and log the ones that are
    /// new, moved, recommitted or gone, with the area an age-1 damage
    /// tracker would redraw; returns whether it would redraw anything.
    pub fn frame<E: Element>(&mut self, output: &Output, elements: &[E]) -> bool {
        let scale = Scale::from(output.current_scale().fractional_scale());
        let tracker = self.tracker.get_or_insert_with(|| OutputDamageTracker::from_output(output));
        let damaged_px: i64 = match tracker.damage_output(1, elements) {
            Ok((Some(rects), _)) => rects.iter().map(|rect| rect.size.w as i64 * rect.size.h as i64).sum(),
            _ => 0,
        };
        let output_px = output.current_mode().map_or(1, |mode| mode.size.w as i64 * mode.size.h as i64).max(1);
        let output = output.name();
        let output = output.as_str();
        self.frame += 1;
        let mut changes = Vec::new();
        let mut seen = HashMap::with_capacity(elements.len());
        for (index, element) in elements.iter().enumerate() {
            let mut nth = 0;
            while seen.contains_key(&(element.id().clone(), nth)) {
                nth += 1;
            }
            let id = (element.id().clone(), nth);
            let commit = element.current_commit();
            let geometry = element.geometry(scale);
            let what = match self.last.get(&id) {
                None => Some("new"),
                Some((_, last)) if *last != geometry => Some("moved"),
                Some((last, _)) if *last != commit => Some("commit"),
                _ => None,
            };
            if let Some(what) = what {
                changes.push(format!(
                    "#{index} {what} {}x{}+{}+{}",
                    geometry.size.w, geometry.size.h, geometry.loc.x, geometry.loc.y
                ));
            }
            seen.insert(id, (commit, geometry));
        }
        let gone = self.last.keys().filter(|id| !seen.contains_key(*id)).count();
        if gone > 0 {
            changes.push(format!("{gone} gone"));
        }
        self.last = seen;
        if !changes.is_empty() || damaged_px > 0 {
            let damaged_pct = (damaged_px * 100 / output_px).min(999);
            info!(output, frame = self.frame, elements = elements.len(), damaged_pct, ?changes, "frame damage");
        }
        damaged_px > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_per_output() {
        record("test-a", |frames| frames.committed += 1);
        record("test-a", |frames| frames.committed += 1);
        record_gpu_wait("test-a", Duration::from_micros(1500));
        record("test-b", |frames| frames.empty += 1);
        let all: serde_json::Value = serde_json::from_str(&json()).unwrap();
        assert_eq!(all["test-a"]["committed"], 2);
        assert_eq!(all["test-a"]["held_for_gpu"], 1);
        assert_eq!(all["test-a"]["gpu_wait_us"], 1500);
        assert_eq!(all["test-b"]["empty"], 1);
    }
}
