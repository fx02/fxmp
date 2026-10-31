//! Left clicks on the tray icon: one click is "Play/Pause", a double click is
//! "Next" and never also pauses. macOS reports single clicks only, so the
//! "Play/Pause" of a first click waits until the double-click window has passed;
//! a second click inside the window cancels it and skips instead. Windows also
//! reports the double click itself (between the two button-ups); it is used there,
//! and the button-up that follows it is swallowed.

use std::time::{Duration, Instant};

/// The shortest double-click window, and the longest one used on macOS, whatever
/// the system says.
pub const MIN_WINDOW: Duration = Duration::from_millis(250);
pub const MAX_WINDOW: Duration = Duration::from_millis(400);
/// The longest double-click window used on Windows: a slower system setting
/// (up to 5 s) would hold up every pause that long.
pub const WINDOWS_MAX_WINDOW: Duration = Duration::from_millis(900);
/// Windows' default `GetDoubleClickTime`, if it cannot be read.
pub const WINDOWS_DEFAULT: Duration = Duration::from_millis(500);

/// What a click does right away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickAction {
    /// A double click: "Next" now.
    Double,
    /// A first click: call [`ClickClassifier::due`] with this sequence number once
    /// the window has passed; it says whether its "Play/Pause" is still wanted.
    Wait(u64),
    /// Nothing (the button-up that ends a Windows double click).
    Ignore,
}

/// The click state, fed with the time of every click (injected, for tests).
#[derive(Debug)]
pub struct ClickClassifier {
    window: Duration,
    /// The first click of a sequence still waiting for its "Play/Pause".
    pending: Option<(u64, Instant)>,
    /// A Windows double click skipped; its trailing button-up until this time is
    /// not a new click.
    swallow_until: Option<Instant>,
    seq: u64,
}

impl ClickClassifier {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: None,
            swallow_until: None,
            seq: 0,
        }
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// A left button-up on the icon at `now`.
    pub fn click(&mut self, now: Instant) -> ClickAction {
        if let Some(until) = self.swallow_until.take() {
            if now <= until {
                return ClickAction::Ignore;
            }
        }
        match self.pending {
            Some((_, first)) if now.saturating_duration_since(first) <= self.window => {
                self.pending = None;
                ClickAction::Double
            }
            _ => {
                self.seq += 1;
                self.pending = Some((self.seq, now));
                ClickAction::Wait(self.seq)
            }
        }
    }

    /// A double click the system reported (Windows) at `now`: skips, cancels the
    /// waiting "Play/Pause", and the button-up that follows is not a click.
    /// If the first click's "Play/Pause" already fired (should the window ever be
    /// shorter than the system's), the double click does nothing and its
    /// button-up is swallowed too: one pause, never also a skip.
    pub fn double_click(&mut self, now: Instant) -> ClickAction {
        self.swallow_until = Some(now + self.window);
        if self.pending.take().is_some() {
            ClickAction::Double
        } else {
            ClickAction::Ignore
        }
    }

    /// Whether the click `seq` still wants its "Play/Pause" at `now` (it was neither
    /// followed by a second click nor replaced). Answers `true` at most once.
    pub fn due(&mut self, seq: u64, now: Instant) -> bool {
        match self.pending {
            Some((s, first)) if s == seq && now.saturating_duration_since(first) >= self.window => {
                self.pending = None;
                true
            }
            _ => false,
        }
    }
}

/// The double-click window: on Windows the system interval (the system reports a
/// double click up to that long after the first click, so "Play/Pause" should not fire
/// earlier), clamped to [`MIN_WINDOW`]..=[`WINDOWS_MAX_WINDOW`]; a double click
/// slower than the cap gives one pause and no skip (see `double_click`).
/// Elsewhere the system interval clamped by [`clamp_window`].
pub fn window_for(system: Option<Duration>, windows: bool) -> Duration {
    if windows {
        system
            .unwrap_or(WINDOWS_DEFAULT)
            .clamp(MIN_WINDOW, WINDOWS_MAX_WINDOW)
    } else {
        clamp_window(system)
    }
}

/// The system double-click interval, clamped to [`MIN_WINDOW`]..=[`MAX_WINDOW`].
pub fn clamp_window(system: Option<Duration>) -> Duration {
    system.unwrap_or(MAX_WINDOW).clamp(MIN_WINDOW, MAX_WINDOW)
}

/// The system's double-click interval; `None` where it is not known.
pub fn system_interval() -> Option<Duration> {
    #[cfg(target_os = "macos")]
    {
        let secs = objc2_app_kit::NSEvent::doubleClickInterval();
        (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs))
    }
    #[cfg(target_os = "windows")]
    {
        // SAFETY: no arguments, no preconditions.
        let ms = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() };
        (ms > 0).then(|| Duration::from_millis(u64::from(ms)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_millis(300);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_single_click_is_next_once_the_window_has_passed() {
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(seq) = c.click(t0) else {
            panic!("a first click waits");
        };
        assert!(!c.due(seq, t0 + ms(100)), "not before the window ends");
        assert!(c.due(seq, t0 + W));
        assert!(!c.due(seq, t0 + ms(900)), "only once");
    }

    #[test]
    fn a_double_click_skips_and_never_pauses() {
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(seq) = c.click(t0) else {
            panic!("a first click waits");
        };
        assert_eq!(c.click(t0 + ms(150)), ClickAction::Double);
        assert!(!c.due(seq, t0 + W), "the first click's pause is cancelled");
    }

    #[test]
    fn three_quick_clicks_skip_once_and_the_third_starts_again() {
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(first) = c.click(t0) else {
            panic!("a first click waits");
        };
        assert_eq!(c.click(t0 + ms(100)), ClickAction::Double);
        let ClickAction::Wait(third) = c.click(t0 + ms(200)) else {
            panic!("the third click starts a new sequence");
        };
        assert_ne!(first, third);
        assert!(!c.due(first, t0 + ms(600)));
        assert!(
            !c.due(third, t0 + ms(400)),
            "its own window, from the third click"
        );
        assert!(c.due(third, t0 + ms(500)));
    }

    #[test]
    fn two_slow_clicks_are_two_nexts() {
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(a) = c.click(t0) else {
            panic!()
        };
        assert!(c.due(a, t0 + W));
        let ClickAction::Wait(b) = c.click(t0 + ms(500)) else {
            panic!("a click after the window is a new first click")
        };
        assert!(c.due(b, t0 + ms(800)));
    }

    #[test]
    fn a_late_second_click_replaces_the_first() {
        // The timer may run late: a click after the window, before the timer, is a
        // new sequence and the old one never fires.
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(a) = c.click(t0) else {
            panic!()
        };
        let ClickAction::Wait(b) = c.click(t0 + ms(350)) else {
            panic!()
        };
        assert!(!c.due(a, t0 + ms(360)));
        assert!(c.due(b, t0 + ms(650)));
    }

    #[test]
    fn a_windows_double_click_skips_and_swallows_its_button_up() {
        // Windows: up (Click), double click, up (Click).
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(seq) = c.click(t0) else {
            panic!()
        };
        assert_eq!(c.double_click(t0 + ms(120)), ClickAction::Double);
        assert_eq!(c.click(t0 + ms(180)), ClickAction::Ignore);
        assert!(!c.due(seq, t0 + W), "the first click's pause is cancelled");
        // The next click is an ordinary first click again.
        assert!(matches!(c.click(t0 + ms(900)), ClickAction::Wait(_)));
    }

    #[test]
    fn the_window_is_the_system_interval_clamped() {
        assert_eq!(clamp_window(Some(ms(500))), MAX_WINDOW);
        assert_eq!(clamp_window(Some(ms(100))), MIN_WINDOW);
        assert_eq!(clamp_window(Some(ms(320))), ms(320));
        assert_eq!(clamp_window(None), MAX_WINDOW);
    }

    #[test]
    fn on_windows_the_window_is_the_system_interval_capped_at_900_ms() {
        // Windows reports the double click up to GetDoubleClickTime after the
        // first click: the window is that long, but a pause never waits more than
        // 0.9 s (a slower double click still only skips, see below).
        assert_eq!(window_for(Some(ms(500)), true), ms(500));
        assert_eq!(window_for(Some(ms(900)), true), ms(900));
        assert_eq!(window_for(Some(ms(5000)), true), WINDOWS_MAX_WINDOW);
        assert_eq!(window_for(Some(ms(100)), true), MIN_WINDOW);
        assert_eq!(window_for(None, true), WINDOWS_DEFAULT);
        assert_eq!(window_for(Some(ms(500)), false), MAX_WINDOW);
        assert_eq!(window_for(Some(ms(100)), false), MIN_WINDOW);
    }

    #[test]
    fn a_slow_windows_double_click_only_skips() {
        // GetDoubleClickTime 500 ms; the second click comes 480 ms after the
        // first, past the macOS clamp: the pause must not have fired yet.
        let w = window_for(Some(ms(500)), true);
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(w);
        let ClickAction::Wait(seq) = c.click(t0) else {
            panic!()
        };
        assert!(
            !c.due(seq, t0 + ms(470)),
            "no pause before Windows' own window ends"
        );
        assert_eq!(c.double_click(t0 + ms(480)), ClickAction::Double);
        assert_eq!(c.click(t0 + ms(520)), ClickAction::Ignore);
        assert!(!c.due(seq, t0 + ms(600)));
    }

    #[test]
    fn a_double_click_after_the_pause_fired_does_nothing_more() {
        // Should the timer ever win anyway: the late double click neither
        // skips nor lets its button-up pause again.
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(seq) = c.click(t0) else {
            panic!()
        };
        assert!(c.due(seq, t0 + W));
        assert_eq!(c.double_click(t0 + ms(320)), ClickAction::Ignore);
        assert_eq!(c.click(t0 + ms(340)), ClickAction::Ignore);
        assert!(matches!(c.click(t0 + ms(1000)), ClickAction::Wait(_)));
    }
}
