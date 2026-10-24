//! Rendering shortcuts that can be turned off in a running session, to
//! measure what each saves or to rule one out when something draws wrong:
//! `shoji_wm --perf-option NAME=off` (and `=on`), `shoji_wm --perf-options`
//! for their state. Each starts on unless its environment variable is set.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfOption {
    /// A frame that would show nothing new is not repainted
    /// (`crate::backend::frame_gate`).
    FrameGate,
    /// A window change redraws the outputs that draw the window, not all of
    /// them (`ShojiWM::schedule_window_redraw`).
    TargetedRedraw,
    /// A clipped (rounded) window claims its opaque area, so what is under it
    /// is not drawn (`ClippedSurfaceElement::opaque_regions`).
    ClipOpaque,
}

const UNSET: u8 = 0;
const ON: u8 = 1;
const OFF: u8 = 2;

static STATES: [AtomicU8; 3] = [AtomicU8::new(UNSET), AtomicU8::new(UNSET), AtomicU8::new(UNSET)];

impl PerfOption {
    pub const ALL: [Self; 3] = [Self::FrameGate, Self::TargetedRedraw, Self::ClipOpaque];

    pub fn name(self) -> &'static str {
        match self {
            Self::FrameGate => "frame-gate",
            Self::TargetedRedraw => "targeted-redraw",
            Self::ClipOpaque => "clip-opaque",
        }
    }

    /// Set (to anything) at startup, it starts off.
    fn disabling_env(self) -> &'static str {
        match self {
            Self::FrameGate => "SHOJI_DISABLE_FRAME_GATE",
            Self::TargetedRedraw => "SHOJI_DISABLE_TARGETED_REDRAW",
            Self::ClipOpaque => "SHOJI_DISABLE_CLIP_OPAQUE",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|option| option.name() == name)
    }

    fn state(self) -> &'static AtomicU8 {
        &STATES[self as usize]
    }

    pub fn enabled(self) -> bool {
        match self.state().load(Ordering::Relaxed) {
            ON => true,
            OFF => false,
            _ => {
                let on = std::env::var_os(self.disabling_env()).is_none();
                // A switch made meanwhile wins over the environment.
                let _ = self.state().compare_exchange(
                    UNSET,
                    if on { ON } else { OFF },
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                self.enabled()
            }
        }
    }

    pub fn set(self, on: bool) {
        self.state().store(if on { ON } else { OFF }, Ordering::Relaxed);
    }
}

/// Every option's state, as one line of JSON: `{"frame-gate": true, ...}`.
pub fn json() -> String {
    let states = PerfOption::ALL
        .into_iter()
        .map(|option| (option.name().to_owned(), serde_json::Value::Bool(option.enabled())))
        .collect::<serde_json::Map<_, _>>();
    serde_json::Value::Object(states).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_switch_and_report() {
        for option in PerfOption::ALL {
            assert_eq!(PerfOption::parse(option.name()), Some(option));
        }
        assert_eq!(PerfOption::parse("nope"), None);
        let option = PerfOption::ClipOpaque;
        let was = option.enabled();
        option.set(false);
        assert!(!option.enabled());
        let all: serde_json::Value = serde_json::from_str(&json()).unwrap();
        assert_eq!(all["clip-opaque"], false);
        option.set(true);
        assert!(option.enabled());
        option.set(was);
    }
}
