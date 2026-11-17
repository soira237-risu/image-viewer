//! What the mouse wheel does: a command bound to it in the keymap, previous / next image when
//! the "wheel navigates" setting is on, else zoom at the cursor. Commands fire once per notch.

use std::time::{Duration, Instant};

use crate::commands::Command;
use crate::keymap::{Chord, Keymap};

/// Wheel units in one notch (`WHEEL_DELTA`).
pub const NOTCH: i32 = 120;

/// A partial notch older than this is forgotten, so a touchpad nudge minutes ago does not turn
/// the next small scroll into a step.
const FORGET_AFTER: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelUse {
    Command(Command),
    /// Zoom at the cursor by the wheel's exact amount.
    Zoom,
}

/// What a wheel turn (`up`: away from the user) with these modifiers does.
pub fn wheel_use(keymap: &Keymap, up: bool, ctrl: bool, alt: bool, shift: bool, navigates: bool) -> WheelUse {
    let chord = Chord::new(if up { "WheelUp" } else { "WheelDown" }, ctrl, alt, shift);
    if let Some(cmd) = keymap.lookup(&chord) {
        return WheelUse::Command(cmd);
    }
    if navigates && !ctrl && !alt && !shift {
        return WheelUse::Command(if up { Command::NavPrev } else { Command::NavNext });
    }
    WheelUse::Zoom
}

/// Turns wheel deltas into whole steps: high-resolution wheels and touchpads send small deltas,
/// and a command must fire once per full notch, never on a fraction.
#[derive(Debug, Default)]
pub struct WheelSteps {
    acc: i32,
    last: Option<Instant>,
}

impl WheelSteps {
    /// Adds `delta` (positive: away from the user) and returns the step to take now: +1, −1 or
    /// 0. At most one step per message, so a fast spin does not skip a run of files.
    pub fn push(&mut self, delta: i32, now: Instant) -> i32 {
        let stale = self.last.is_some_and(|t| now.saturating_duration_since(t) > FORGET_AFTER);
        self.last = Some(now);
        // a reversal or a pause starts a new notch
        if stale || self.acc.signum() * delta.signum() < 0 {
            self.acc = 0;
        }
        self.acc = self.acc.saturating_add(delta);
        if self.acc.abs() < NOTCH {
            return 0;
        }
        let step = self.acc.signum();
        // extra whole notches in one message are dropped; the fraction carries over
        self.acc %= NOTCH;
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_step_per_notch_and_none_on_fractions() {
        let t = Instant::now();
        let mut w = WheelSteps::default();
        assert_eq!(w.push(120, t), 1);
        assert_eq!(w.push(-120, t), -1);
        // a precision wheel: 40 + 40 + 40 is one notch
        assert_eq!(w.push(40, t), 0);
        assert_eq!(w.push(40, t), 0);
        assert_eq!(w.push(40, t), 1);
        // a fast spin of three notches in one message is still one step
        assert_eq!(w.push(360, t), 1);
        assert_eq!(w.push(0, t), 0);
    }

    #[test]
    fn reversal_and_pause_drop_the_partial_notch() {
        let t = Instant::now();
        let mut w = WheelSteps::default();
        assert_eq!(w.push(100, t), 0);
        assert_eq!(w.push(-30, t), 0, "reversal starts over instead of cancelling");
        assert_eq!(w.push(-90, t), -1);
        assert_eq!(w.push(100, t), 0);
        assert_eq!(w.push(30, t + Duration::from_secs(2)), 0, "old fraction forgotten");
        assert_eq!(w.push(90, t + Duration::from_secs(2)), 1);
    }

    #[test]
    fn keymap_then_setting_then_zoom() {
        let map = Keymap::defaults();
        assert_eq!(wheel_use(&map, true, false, false, false, false), WheelUse::Zoom);
        assert_eq!(wheel_use(&map, true, false, false, false, true), WheelUse::Command(Command::NavPrev));
        assert_eq!(wheel_use(&map, false, false, false, false, true), WheelUse::Command(Command::NavNext));
        // with the setting on, Ctrl+wheel still zooms
        assert_eq!(wheel_use(&map, false, true, false, false, true), WheelUse::Zoom);
        let mut map = Keymap::defaults();
        map.apply_toml("[bindings]\n\"Shift+WheelUp\" = \"view.zoom_in\"\n\"WheelDown\" = \"nav.last\"\n").unwrap();
        assert_eq!(wheel_use(&map, true, false, false, true, false), WheelUse::Command(Command::ViewZoomIn));
        assert_eq!(wheel_use(&map, false, false, false, false, true), WheelUse::Command(Command::NavLast));
    }
}
