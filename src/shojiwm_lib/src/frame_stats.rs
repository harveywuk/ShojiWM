//! Per-output frame counters behind `shoji_wm --frame-stats`.
//!
//! Kept by the tty backend, cumulative since the compositor started: take two
//! readings and subtract them for rates. Read by the control socket's own
//! thread, so a busy event loop still answers.
//!
//! [`DamageProbe`] logs which elements of a frame changed since the one
//! before: what an age-1 damage tracker would redraw. It runs while frame
//! debugging is on (`SHOJI_FRAME_DAMAGE_DEBUG`, or `shoji_wm --frame-debug on`
//! in a running session), which also counts `schedule_redraw`'s callers;
//! `shoji_wm --frame-debug` takes both, as JSON.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, VecDeque},
    panic::Location,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
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

static DEBUGGING: AtomicBool = AtomicBool::new(false);

/// What frame debugging collected since it was last taken.
struct DebugLog {
    since: Option<Instant>,
    redraw_callers: BTreeMap<&'static Location<'static>, u64>,
    frames: VecDeque<serde_json::Value>,
    frames_dropped: u64,
}

static DEBUG_LOG: Mutex<DebugLog> = Mutex::new(DebugLog {
    since: None,
    redraw_callers: BTreeMap::new(),
    frames: VecDeque::new(),
    frames_dropped: 0,
});

/// Frames kept between two takes; older ones are dropped and counted.
const DEBUG_FRAMES: usize = 600;

fn debug_log() -> std::sync::MutexGuard<'static, DebugLog> {
    DEBUG_LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether frame debugging is on.
pub fn debugging() -> bool {
    DEBUGGING.load(Ordering::Relaxed) || crate::env_flag!("SHOJI_FRAME_DAMAGE_DEBUG")
}

/// Turn frame debugging on or off at runtime, starting a fresh log.
pub fn set_debugging(on: bool) {
    DEBUGGING.store(on, Ordering::Relaxed);
    let mut log = debug_log();
    log.since = on.then(Instant::now);
    log.redraw_callers.clear();
    log.frames.clear();
    log.frames_dropped = 0;
}

/// Count one `schedule_redraw` call from `caller`, while debugging.
pub fn record_redraw_caller(caller: &'static Location<'static>) {
    *debug_log().redraw_callers.entry(caller).or_default() += 1;
}

/// What frame debugging collected since the last take, as one line of JSON,
/// and start collecting again: `seconds`, `redraw_callers` (most first) and
/// `frames` (the changed ones, oldest first).
pub fn take_debug_json() -> String {
    let mut log = debug_log();
    let now = Instant::now();
    let seconds = log.since.map_or(0.0, |since| now.duration_since(since).as_secs_f64());
    log.since = debugging().then_some(now);
    let mut callers = std::mem::take(&mut log.redraw_callers)
        .into_iter()
        .map(|(caller, count)| (format!("{}:{}", caller.file(), caller.line()), count))
        .collect::<Vec<_>>();
    callers.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let frames = std::mem::take(&mut log.frames);
    let dropped = std::mem::take(&mut log.frames_dropped);
    serde_json::json!({
        "enabled": debugging(),
        "seconds": seconds,
        "redraw_callers": callers,
        "frames_dropped": dropped,
        "frames": frames,
    })
    .to_string()
}

fn record_debug_frame(frame: serde_json::Value) {
    let mut log = debug_log();
    if log.frames.len() >= DEBUG_FRAMES {
        log.frames.pop_front();
        log.frames_dropped += 1;
    }
    log.frames.push_back(frame);
}

thread_local! {
    /// The render thread's probe per output.
    static PROBES: RefCell<HashMap<String, DamageProbe>> = RefCell::new(HashMap::new());
}

/// Probe one frame of `output`'s (see [`DamageProbe::frame`]), while
/// debugging; `None` when not debugging.
pub fn probe_frame<E: Element>(output: &Output, elements: &[E]) -> Option<bool> {
    if !debugging() {
        // A later session starts from the next frame, not a stale one.
        if PROBES.with(|probes| !probes.borrow().is_empty()) {
            PROBES.with(|probes| probes.borrow_mut().clear());
        }
        return None;
    }
    let name = output.name();
    Some(PROBES.with(|probes| probes.borrow_mut().entry(name).or_default().frame(output, elements)))
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
            let at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as u64);
            record_debug_frame(serde_json::json!({
                "output": output,
                "frame": self.frame,
                "at_ms": at_ms,
                "elements": elements.len(),
                "damaged_pct": damaged_pct,
                "changes": changes,
            }));
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

    #[test]
    fn debugging_counts_redraw_callers_until_taken() {
        set_debugging(true);
        let here = Location::caller();
        record_redraw_caller(here);
        record_redraw_caller(here);
        let taken: serde_json::Value = serde_json::from_str(&take_debug_json()).unwrap();
        assert_eq!(taken["enabled"], true);
        let callers = taken["redraw_callers"].as_array().unwrap();
        let ours = callers
            .iter()
            .find(|caller| caller[0] == format!("{}:{}", here.file(), here.line()))
            .unwrap();
        assert_eq!(ours[1], 2);
        let again: serde_json::Value = serde_json::from_str(&take_debug_json()).unwrap();
        assert!(again["redraw_callers"].as_array().unwrap().iter().all(|caller| caller[0] != ours[0]));
        set_debugging(false);
        assert!(!DEBUGGING.load(Ordering::Relaxed));
    }
}
