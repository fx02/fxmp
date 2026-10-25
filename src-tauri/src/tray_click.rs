//! Left clicks on the tray icon: one click is "Next", a double click is
//! "Play/Pause" and never also skips. macOS reports single clicks only, so the
//! "Next" of a first click waits until the double-click window has passed; a
//! second click inside the window cancels it and toggles instead. Windows also
//! reports the double click itself (between the two button-ups); it is used there,
//! and the button-up that follows it is swallowed.

use std::time::{Duration, Instant};

/// The shortest and the longest double-click window used, whatever the system says.
pub const MIN_WINDOW: Duration = Duration::from_millis(250);
pub const MAX_WINDOW: Duration = Duration::from_millis(400);

/// What a click does right away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickAction {
    /// Play/Pause now.
    Toggle,
    /// A first click: call [`ClickClassifier::due`] with this sequence number once
    /// the window has passed; it says whether "Next" is still wanted.
    Wait(u64),
    /// Nothing (the button-up that ends a Windows double click).
    Ignore,
}

/// The click state, fed with the time of every click (injected, for tests).
#[derive(Debug)]
pub struct ClickClassifier {
    window: Duration,
    /// The first click of a sequence still waiting for its "Next".
    pending: Option<(u64, Instant)>,
    /// A Windows double click toggled; its trailing button-up until this time is
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
                ClickAction::Toggle
            }
            _ => {
                self.seq += 1;
                self.pending = Some((self.seq, now));
                ClickAction::Wait(self.seq)
            }
        }
    }

    /// A double click the system reported (Windows) at `now`: toggles, cancels the
    /// waiting "Next", and the button-up that follows is not a click.
    pub fn double_click(&mut self, now: Instant) -> ClickAction {
        self.pending = None;
        self.swallow_until = Some(now + self.window);
        ClickAction::Toggle
    }

    /// Whether the click `seq` still wants its "Next" at `now` (it was neither
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
    fn a_double_click_toggles_and_never_skips() {
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(seq) = c.click(t0) else {
            panic!("a first click waits");
        };
        assert_eq!(c.click(t0 + ms(150)), ClickAction::Toggle);
        assert!(!c.due(seq, t0 + W), "the first click's Next is cancelled");
    }

    #[test]
    fn three_quick_clicks_toggle_once_and_the_third_starts_again() {
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(first) = c.click(t0) else {
            panic!("a first click waits");
        };
        assert_eq!(c.click(t0 + ms(100)), ClickAction::Toggle);
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
    fn a_windows_double_click_toggles_and_swallows_its_button_up() {
        // Windows: up (Click), double click, up (Click).
        let t0 = Instant::now();
        let mut c = ClickClassifier::new(W);
        let ClickAction::Wait(seq) = c.click(t0) else {
            panic!()
        };
        assert_eq!(c.double_click(t0 + ms(120)), ClickAction::Toggle);
        assert_eq!(c.click(t0 + ms(180)), ClickAction::Ignore);
        assert!(!c.due(seq, t0 + W), "the first click's Next is cancelled");
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
}
