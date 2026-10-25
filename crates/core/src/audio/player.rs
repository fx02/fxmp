//! Player engine: a dedicated thread that mixes tracks into a [`Sink`].
//!
//! Each track ("deck") is opened and decoded on its own thread into a bounded buffer, so a
//! slow or hung source never blocks mixing. The engine thread pulls from at most two
//! audible decks (outgoing + current), applies fades, normalisation and volume, and writes
//! to the sink without blocking.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::deck::{Deck, LoadCtx, Ramp};
use super::dsp::{apply_ceiling, db_to_lin, equal_power, LoudnessMeter};
use super::output::{Sink, SinkFactory, SinkWrite};
use super::prefetch::PrefetchConfig;
use crate::failure::Failure;
use crate::source::MusicSource;
use crate::TrackId;

/// Frames mixed per block.
const BLOCK: usize = 1024;
const NEXT_FADE_SECS: f32 = 0.7;
const SKIP_FADE_SECS: f32 = 0.3;
/// The next track is opened this long (plus the crossfade) before the current one ends.
const PRELOAD_LEAD_SECS: u64 = 10;
const RAMP_SECS: f32 = 0.02;
const MAX_CONSECUTIVE_FAILURES: u32 = 20;
/// How long the mix may hold at a crossfade point for a next track that is still opening.
const READY_GRACE: Duration = Duration::from_millis(100);
/// How long a fading outgoing deck may hold up the mix when its source stalls.
const OUT_STALL_GRACE: Duration = Duration::from_millis(100);
const SINK_RETRY: Duration = Duration::from_secs(1);
const SINK_FULL_WAIT: Duration = Duration::from_millis(10);
const IDLE_WAIT: Duration = Duration::from_millis(500);
/// How often the position of the playing track is reported.
const POSITION_EVERY: Duration = Duration::from_millis(250);
/// Fade-in of a track that starts part way in (resuming where it stopped).
const RESUME_FADE_SECS: f32 = 0.5;

/// A track to play, as handed out by a [`TrackProvider`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlayItem {
    pub id: TrackId,
    pub rel_path: String,
    /// Normalisation gain, applied when normalisation is on.
    pub gain_db: f32,
    /// Where to start, in ms from the beginning (resuming); 0 = the start. If the
    /// track cannot be positioned there it plays from the start.
    pub start_ms: u64,
}

/// Decides what plays next and hears how it went.
pub trait TrackProvider: Send {
    fn next_track(&mut self) -> Option<PlayItem>;
    /// `id` could not be opened or decoded, for this reason.
    fn on_failed(&mut self, id: TrackId, failure: &Failure);
    fn on_played(&mut self, id: TrackId, measured_lufs: Option<f32>);
    /// `id`, handed out by `next_track`, became the current (audible) track. Until
    /// then, `next_track` may be asked again (after [`Command::Play`] or
    /// [`Command::Requeue`]) and should hand out what is due at that moment.
    fn on_started(&mut self, _id: TrackId) {}
}

pub enum Command {
    /// Plays the provider's tracks, fading out what plays now (as `Next` does).
    Play(Box<dyn TrackProvider>),
    /// As `Play`, but paused: the first track is loaded (and reported as started)
    /// without being heard until `Resume`.
    Cue(Box<dyn TrackProvider>),
    /// The provider's upcoming tracks changed: the prepared next track (not started
    /// yet) is dropped and asked for again, and a provider that ran out is asked again.
    Requeue,
    Pause,
    Resume,
    TogglePause,
    Next,
    /// Linear volume, 0..1.
    SetVolume(f32),
    /// Output device (`None` = system default). Ignored when the output is open on
    /// that device already; after a fallback to the default it reopens the device.
    SetDevice(Option<String>),
    SetCrossfade {
        enabled: bool,
        secs: u8,
    },
    SetNormalize(bool),
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TrackStarted {
        id: TrackId,
    },
    TrackSkipped {
        id: TrackId,
        reason: String,
    },
    Paused(bool),
    DeviceChanged(String),
    Idle,
    /// Where the current track is: `ms` of it have been played (counting from the
    /// start of the track). Sent about every 250 ms while playing, when a track
    /// starts, and when a pause has taken effect.
    Position {
        id: TrackId,
        ms: u64,
        /// From the decoder, if the file tells.
        duration_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct PlayerConfig {
    pub prefetch: PrefetchConfig,
}

enum Msg {
    Cmd(Command),
    /// A deck has new data or changed state.
    Wake,
    Shutdown,
}

/// Handle to the engine thread; dropping it stops the engine.
pub struct Player {
    tx: Sender<Msg>,
    /// Engine loop iterations, to check the engine does not spin.
    #[cfg(test)]
    loops: Arc<std::sync::atomic::AtomicU64>,
}

impl Player {
    pub fn spawn(
        source: Arc<dyn MusicSource>,
        sinks: Box<dyn SinkFactory>,
        events: Sender<Event>,
        cfg: PlayerConfig,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let engine = Engine::new(rx, tx.clone(), source, sinks, events, cfg);
        #[cfg(test)]
        let loops = engine.loops.clone();
        std::thread::Builder::new()
            .name("player".into())
            .spawn(move || engine.run())
            .expect("spawn player thread");
        Self {
            tx,
            #[cfg(test)]
            loops,
        }
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(Msg::Cmd(c));
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

// ---------------------------------------------------------------------------------------
// Engine

enum Step {
    Progress,
    Wait(Duration),
}

struct Engine {
    rx: Receiver<Msg>,
    events: Sender<Event>,
    sinks: Box<dyn SinkFactory>,
    load: LoadCtx,

    sink: Option<Box<dyn Sink>>,
    /// Output rate decks are decoded at; 0 until a sink has been opened.
    rate: u32,
    device: Option<String>,
    /// The device was lost: reopen the system default.
    use_default: bool,
    sink_retry_at: Option<Instant>,

    provider: Option<Box<dyn TrackProvider>>,
    /// The provider has no more tracks, or too many failed in a row.
    stopped_feeding: bool,
    /// ...because the provider ran out (it is dropped once playback ends).
    exhausted: bool,
    idle_reported: bool,
    failures: u32,

    /// Outgoing deck (fading out), current deck, and the deck being prepared.
    out: Option<Deck>,
    cur: Option<Deck>,
    next: Option<Deck>,
    pending_skips: u32,
    grace_since: Option<Instant>,
    /// Since when the outgoing deck has had too little audio.
    out_starved_since: Option<Instant>,

    /// When the position was last reported.
    position_at: Option<Instant>,
    paused: bool,
    /// `Paused(true)` has been emitted for the current pause (after its fade-out).
    pause_reported: bool,
    /// Fades output out on pause and in on resume.
    pause_gain: f32,
    volume: f32,
    volume_target: f32,
    crossfade_secs: Option<u8>,
    normalize: bool,

    /// Mixed audio the sink has not accepted yet.
    pending: Vec<f32>,
    pending_pos: usize,
    scratch: Vec<f32>,
    loops: Arc<std::sync::atomic::AtomicU64>,
}

impl Engine {
    fn new(
        rx: Receiver<Msg>,
        wake: Sender<Msg>,
        source: Arc<dyn MusicSource>,
        sinks: Box<dyn SinkFactory>,
        events: Sender<Event>,
        cfg: PlayerConfig,
    ) -> Self {
        Self {
            rx,
            events,
            sinks,
            load: LoadCtx {
                source,
                prefetch: cfg.prefetch,
                wake: Arc::new(move || {
                    let _ = wake.send(Msg::Wake);
                }),
            },
            sink: None,
            rate: 0,
            device: None,
            use_default: false,
            sink_retry_at: None,
            provider: None,
            stopped_feeding: false,
            exhausted: false,
            idle_reported: true,
            failures: 0,
            out: None,
            cur: None,
            next: None,
            pending_skips: 0,
            grace_since: None,
            out_starved_since: None,
            position_at: None,
            paused: false,
            pause_reported: false,
            pause_gain: 1.0,
            volume: 1.0,
            volume_target: 1.0,
            crossfade_secs: Some(5),
            normalize: true,
            pending: Vec::with_capacity(BLOCK * 2),
            pending_pos: 0,
            scratch: Vec::with_capacity(BLOCK * 2),
            loops: Arc::default(),
        }
    }

    fn run(mut self) {
        let mut wait = None;
        loop {
            if !self.drain_messages(wait) {
                return;
            }
            self.loops
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.update();
            wait = match self.step() {
                Step::Progress => None,
                Step::Wait(d) => Some(d),
            };
            self.report_position(false);
        }
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    /// Handles queued messages, first waiting up to `wait` for one. False on shutdown.
    fn drain_messages(&mut self, wait: Option<Duration>) -> bool {
        let first = match wait {
            Some(d) => match self.rx.recv_timeout(d) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return false,
            },
            None => None,
        };
        let mut msg = first.or_else(|| self.rx.try_recv().ok());
        while let Some(m) = msg {
            match m {
                Msg::Shutdown => return false,
                Msg::Wake => {}
                Msg::Cmd(c) => self.command(c),
            }
            msg = self.rx.try_recv().ok();
        }
        true
    }

    fn command(&mut self, c: Command) {
        match c {
            Command::Cue(p) => {
                self.command(Command::Play(p));
                self.set_paused(true);
                if self.cur.is_none() {
                    // Nothing audible to fade out: paused at once, before the track
                    // is reported as started.
                    self.pause_gain = 0.0;
                }
            }
            Command::Requeue => {
                if self.provider.is_some() {
                    self.next = None;
                    self.grace_since = None;
                    if self.exhausted {
                        self.exhausted = false;
                        self.stopped_feeding = false;
                    }
                }
            }
            Command::Play(p) => {
                // A track that already played to its end belongs to the old provider.
                if let Some(mut out) = self.out.take() {
                    self.report_end(&mut out);
                    self.out = Some(out);
                }
                self.next = None;
                self.grace_since = None;
                self.provider = Some(p);
                self.stopped_feeding = false;
                self.exhausted = false;
                self.idle_reported = false;
                self.failures = 0;
                // Fade the current track out towards the new selection.
                self.pending_skips = u32::from(self.cur.is_some());
                if self.paused {
                    self.set_paused(false);
                }
            }
            Command::Pause => self.set_paused(true),
            Command::Resume => {
                self.retry_feeding();
                self.set_paused(false);
            }
            Command::TogglePause => {
                // Play on a player that gave up after failures means "try again",
                // never "pause".
                if self.retry_feeding() {
                    self.set_paused(false);
                } else {
                    self.set_paused(!self.paused);
                }
            }
            Command::Next => {
                if self.retry_feeding() {
                    // Gave up waiting after too many failures: trying again.
                } else if !self.nothing_loaded() || self.provider.is_some() {
                    self.pending_skips += 1;
                }
            }
            Command::SetVolume(v) => {
                if v.is_finite() {
                    self.volume_target = v.clamp(0.0, 1.0);
                }
            }
            // Already open on that very device: nothing to do (a reopen would be
            // a dropout). After a fallback to the default output it reopens.
            Command::SetDevice(d)
                if d == self.device && !self.use_default && self.sink.is_some() => {}
            Command::SetDevice(d) => {
                self.device = d;
                self.use_default = false;
                if self.sink.is_some() {
                    self.sink = None;
                    self.sink_retry_at = None;
                }
            }
            Command::SetCrossfade { enabled, secs } => {
                self.crossfade_secs = enabled.then(|| secs.clamp(1, 10));
            }
            Command::SetNormalize(b) => self.normalize = b,
            Command::Stop => {
                self.out = None;
                self.cur = None;
                self.next = None;
                self.provider = None;
                self.pending_skips = 0;
                self.pending.clear();
                self.pending_pos = 0;
                self.grace_since = None;
                if self.paused {
                    self.set_paused(false);
                }
                self.emit(Event::Idle);
                self.idle_reported = true;
            }
        }
    }

    /// A player that stopped after too many failures in a row (nothing loaded) asks
    /// its provider again. False if it had not stopped.
    fn retry_feeding(&mut self) -> bool {
        if self.provider.is_some() && self.stopped_feeding && self.nothing_loaded() {
            self.stopped_feeding = false;
            self.idle_reported = false;
            self.failures = 0;
            true
        } else {
            false
        }
    }

    /// Pausing fades out over 20 ms first; `Paused(true)` follows once the fade is written.
    /// Resuming before the fade-out finished emits nothing: `Paused(true)` never went out.
    fn set_paused(&mut self, p: bool) {
        if p == self.paused {
            return;
        }
        let was_reported = self.paused && self.pause_reported;
        self.paused = p;
        self.pause_reported = !p;
        if !p && was_reported {
            self.emit(Event::Paused(false));
        }
    }

    fn report_pause(&mut self) {
        if self.paused && !self.pause_reported {
            self.pause_reported = true;
            // Where it stopped, before the pause itself.
            self.report_position(true);
            self.emit(Event::Paused(true));
        }
    }

    /// Reports where the current track is: when `force`d, else at most every
    /// [`POSITION_EVERY`] while playing.
    fn report_position(&mut self, force: bool) {
        let Some(cur) = &self.cur else { return };
        let due = self
            .position_at
            .is_none_or(|t| t.elapsed() >= POSITION_EVERY);
        if !force && (self.paused || !due || self.rate == 0) {
            return;
        }
        let s = cur.status();
        let ms = s.start_ms + cur.played * 1000 / self.rate.max(1) as u64;
        let (id, duration_ms) = (cur.item.id, s.duration_ms);
        self.position_at = Some(Instant::now());
        self.emit(Event::Position {
            id,
            ms,
            duration_ms,
        });
    }

    fn nothing_loaded(&self) -> bool {
        self.out.is_none() && self.cur.is_none() && self.next.is_none()
    }

    fn active(&self) -> bool {
        self.provider.is_some() || !self.nothing_loaded()
    }

    fn secs_to_frames(&self, secs: f32) -> u64 {
        (secs * self.rate as f32) as u64
    }

    fn crossfade_frames(&self) -> u64 {
        self.crossfade_secs
            .map_or(0, |s| s as u64 * self.rate as u64)
    }

    // --- state transitions -------------------------------------------------------------

    fn update(&mut self) {
        if self.active() {
            self.ensure_sink();
        }
        if self.rate == 0 {
            return;
        }
        self.check_early_failures();
        self.apply_skips();
        self.finish_outgoing();
        self.advance_current();
        self.preload();
        if self.pending_skips > 0
            && self.stopped_feeding
            && self.cur.is_none()
            && self.next.is_none()
        {
            self.pending_skips = 0;
        }
        if self.stopped_feeding && self.nothing_loaded() && !self.idle_reported {
            if self.exhausted {
                self.provider = None;
            }
            self.idle_reported = true;
            self.emit(Event::Idle);
        }
    }

    fn ensure_sink(&mut self) {
        if self.sink.is_some() || self.sink_retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let want = if self.use_default {
            None
        } else {
            self.device.clone()
        };
        let mut opened = self.sinks.open(want.as_deref());
        if let Err(e) = &opened {
            log::warn!("cannot open output {want:?}: {e}");
            if want.is_some() {
                opened = self.sinks.open(None);
            }
        }
        match opened {
            Ok((sink, name)) => {
                self.sink_retry_at = None;
                let rate = sink.sample_rate();
                self.sink = Some(sink);
                if rate != self.rate {
                    self.change_rate(rate);
                }
                self.emit(Event::DeviceChanged(name));
            }
            Err(e) => {
                log::warn!("cannot open default output: {e}");
                self.sink_retry_at = Some(Instant::now() + SINK_RETRY);
            }
        }
    }

    /// Decks decode at the sink's rate; re-open them at the new one where they were.
    fn change_rate(&mut self, rate: u32) {
        let old = self.rate;
        self.rate = rate;
        if old == 0 {
            return;
        }
        log::info!("output rate changed {old} -> {rate} Hz");
        self.pending.clear();
        self.pending_pos = 0;
        self.drop_outgoing();
        let ctx = self.load.clone();
        for deck in [&mut self.cur, &mut self.next].into_iter().flatten() {
            deck.reload(&ctx, old, rate);
        }
    }

    /// Reports and drops decks that failed before producing audio.
    fn check_early_failures(&mut self) {
        for slot in [Slot::Cur, Slot::Next] {
            let failed = self.slot(slot).as_ref().and_then(Deck::failed_early);
            if let Some(failure) = failed {
                let deck = self.slot(slot).take().expect("checked above");
                self.grace_since = None;
                log::warn!("skipping {}: {failure}", deck.item.rel_path);
                if let Some(p) = self.provider.as_mut() {
                    p.on_failed(deck.item.id, &failure);
                }
                self.emit(Event::TrackSkipped {
                    id: deck.item.id,
                    reason: failure.message,
                });
                self.failures += 1;
                if self.failures >= MAX_CONSECUTIVE_FAILURES {
                    log::warn!("{} tracks failed in a row; waiting", self.failures);
                    self.stopped_feeding = true;
                }
            }
        }
    }

    fn slot(&mut self, s: Slot) -> &mut Option<Deck> {
        match s {
            Slot::Cur => &mut self.cur,
            Slot::Next => &mut self.next,
        }
    }

    fn next_ready(&self) -> bool {
        self.next.as_ref().is_some_and(Deck::ready)
    }

    /// Makes the prepared next deck the current one.
    fn start_next(&mut self, fade_in: Option<u64>) {
        let mut deck = self.next.take().expect("start_next needs a next deck");
        let resumed = (deck.item.start_ms > 0).then(|| self.secs_to_frames(RESUME_FADE_SECS));
        deck.fade_in = fade_in.or(resumed).map(Ramp::new);
        self.failures = 0;
        self.grace_since = None;
        let id = deck.item.id;
        if let Some(p) = self.provider.as_mut() {
            p.on_started(id);
        }
        self.emit(Event::TrackStarted { id });
        self.cur = Some(deck);
        self.report_position(true);
    }

    /// Drops the outgoing deck, reporting it if it had played to its end.
    fn drop_outgoing(&mut self) {
        self.out_starved_since = None;
        if let Some(mut deck) = self.out.take() {
            self.report_end(&mut deck);
        }
    }

    /// Reports a deck that played to its end (once).
    fn report_end(&mut self, deck: &mut Deck) {
        if !deck.reached_end {
            return;
        }
        deck.reached_end = false;
        let Some(p) = self.provider.as_mut() else {
            return;
        };
        match deck.status().error {
            Some(failure) => {
                log::warn!("{} broke off: {failure}", deck.item.rel_path);
                p.on_failed(deck.item.id, &failure);
                self.emit(Event::TrackSkipped {
                    id: deck.item.id,
                    reason: failure.message,
                });
            }
            None => {
                let lufs = deck.meter.take().and_then(LoudnessMeter::finish);
                p.on_played(deck.item.id, lufs);
            }
        }
    }

    /// Each `Next` press skips one track: the current one fades out (0.7 s crossfade if
    /// the next is ready, else a 0.3 s fade-out). A press while a fade is running ends it
    /// at once, so at most two tracks are ever audible.
    fn apply_skips(&mut self) {
        while self.pending_skips > 0 {
            if let Some(mut cur) = self.cur.take() {
                self.drop_outgoing();
                self.pending_skips -= 1;
                if self.next_ready() {
                    let len = self.secs_to_frames(NEXT_FADE_SECS);
                    cur.fade_out = Some(Ramp::new(len));
                    self.out = Some(cur);
                    self.start_next(Some(len));
                } else {
                    cur.fade_out = Some(Ramp::new(self.secs_to_frames(SKIP_FADE_SECS)));
                    self.out = Some(cur);
                }
            } else if self.next_ready() {
                // The press is for the track that was about to start: it fades in and
                // straight out again (no click), ending any fade still running.
                self.drop_outgoing();
                self.start_next(Some(self.secs_to_frames(SKIP_FADE_SECS)));
            } else {
                // Wait for the next track; a running fade continues meanwhile.
                break;
            }
        }
    }

    fn finish_outgoing(&mut self) {
        let finished = self
            .out
            .as_ref()
            .is_some_and(|d| d.drained() || d.fade_out.as_ref().is_some_and(Ramp::done));
        if finished {
            self.drop_outgoing();
        }
    }

    /// Natural end of the current track: crossfade into the next, or hand over directly.
    fn advance_current(&mut self) {
        if let Some(cur) = &self.cur {
            let s = cur.status();
            if s.done && s.buffered == 0 {
                let mut deck = self.cur.take().expect("checked");
                deck.reached_end = true;
                self.grace_since = None;
                self.report_end(&mut deck);
            } else if s.done
                && (s.buffered as u64) <= self.crossfade_frames()
                && self.next.is_some()
            {
                if self.next_ready() {
                    let len = s.buffered as u64;
                    self.drop_outgoing();
                    let mut deck = self.cur.take().expect("checked");
                    deck.reached_end = true;
                    deck.fade_out = Some(Ramp::new(len));
                    self.out = Some(deck);
                    self.start_next(Some(len));
                } else if self.grace_since.is_none() {
                    self.grace_since = Some(Instant::now());
                }
            }
        }
        if self.cur.is_none() && self.out.is_none() && self.next_ready() {
            self.start_next(None);
        }
    }

    /// Frames left in the current track, as far as known (0 if unknown).
    fn remaining_estimate(&self, deck: &Deck) -> u64 {
        let s = deck.status();
        if s.done {
            return s.buffered as u64;
        }
        match s.duration_ms {
            Some(ms) => (ms * self.rate as u64 / 1000).saturating_sub(deck.played),
            None => 0,
        }
    }

    fn preload(&mut self) {
        if self.next.is_some() || self.stopped_feeding || self.provider.is_none() {
            return;
        }
        let due = match &self.cur {
            None => true,
            Some(cur) => {
                self.pending_skips > 0
                    || self.remaining_estimate(cur)
                        <= self.crossfade_frames() + PRELOAD_LEAD_SECS * self.rate as u64
            }
        };
        if !due {
            return;
        }
        match self.provider.as_mut().and_then(|p| p.next_track()) {
            Some(item) => self.next = Some(Deck::load(&self.load, item, self.rate)),
            None => {
                self.stopped_feeding = true;
                self.exhausted = true;
            }
        }
    }

    // --- mixing and output -------------------------------------------------------------

    fn step(&mut self) -> Step {
        if self.paused && (!self.active() || self.sink.is_none()) {
            self.pause_gain = 0.0;
        }
        if !self.active() {
            self.report_pause();
            return Step::Wait(IDLE_WAIT);
        }
        if self.sink.is_none() {
            self.report_pause();
            let wait = self
                .sink_retry_at
                .map_or(SINK_RETRY, |t| t.saturating_duration_since(Instant::now()));
            return Step::Wait(wait);
        }
        if self.pending_pos < self.pending.len() {
            return self.write_pending();
        }
        if self.paused && self.pause_gain <= 0.0 {
            self.report_pause();
            return Step::Wait(IDLE_WAIT);
        }
        if let Some(since) = self.grace_since {
            let cf_point = self.cur.as_ref().is_some_and(|c| {
                let s = c.status();
                s.done && (s.buffered as u64) <= self.crossfade_frames()
            });
            if cf_point
                && self.next.is_some()
                && !self.next_ready()
                && since.elapsed() < READY_GRACE
            {
                return Step::Wait(READY_GRACE.saturating_sub(since.elapsed()));
            }
        }
        if self.mix_block() == 0 {
            if self.paused {
                // Nothing to fade out: pause at once.
                self.pause_gain = 0.0;
                return Step::Progress;
            }
            if let Some(since) = self.out_starved_since {
                let left = OUT_STALL_GRACE.saturating_sub(since.elapsed());
                if !left.is_zero() {
                    return Step::Wait(left);
                }
            }
            // Waiting for a deck: its thread wakes us when data arrives.
            return Step::Wait(IDLE_WAIT);
        }
        self.write_pending()
    }

    fn write_pending(&mut self) -> Step {
        let sink = self.sink.as_mut().expect("checked by caller");
        match sink.write(&self.pending[self.pending_pos..]) {
            SinkWrite::DeviceLost => {
                log::warn!("output device lost; switching to the default device");
                self.sink = None;
                self.use_default = true;
                self.sink_retry_at = None;
                Step::Progress
            }
            SinkWrite::Ok(n) => {
                self.pending_pos += n;
                if self.pending_pos >= self.pending.len() {
                    self.pending.clear();
                    self.pending_pos = 0;
                    Step::Progress
                } else {
                    Step::Wait(SINK_FULL_WAIT)
                }
            }
        }
    }

    /// Mixes up to one block into `pending`; returns the number of frames (0 = must wait).
    fn mix_block(&mut self) -> usize {
        let mut frames = BLOCK;
        let mut any = false;
        if self.out.is_none() {
            self.out_starved_since = None;
        }
        let cf = self.crossfade_frames();
        for (i, deck) in [&self.out, &self.cur].into_iter().enumerate() {
            let Some(deck) = deck else { continue };
            any = true;
            let s = deck.status();
            let is_cur = i == 1;
            let starved = !s.done && s.buffered < frames;
            if !is_cur {
                if !starved {
                    self.out_starved_since = None;
                } else if deck.fade_out.is_some() {
                    // A fading outgoing deck whose source stalls holds up the mix only
                    // briefly, then contributes silence so the fade (and Next) goes on.
                    let since = *self.out_starved_since.get_or_insert_with(Instant::now);
                    if since.elapsed() < OUT_STALL_GRACE {
                        frames = s.buffered;
                    }
                    continue;
                }
            }
            if !s.done {
                frames = frames.min(s.buffered);
            }
            if is_cur && s.done && s.buffered > 0 && deck.fade_out.is_none() {
                // End the block with the track so the next one follows without a gap.
                frames = frames.min(s.buffered);
            }
            if !is_cur || cf == 0 || deck.fade_out.is_some() {
                continue;
            }
            let buffered = s.buffered as u64;
            if s.done && buffered > cf {
                // Stop exactly at the crossfade point.
                frames = frames.min((buffered - cf) as usize);
            } else if !s.done && self.remaining_estimate(deck) <= cf + self.rate as u64 {
                // Near the end but the decoder has not reached it yet: keep a crossfade's
                // worth in reserve so the crossfade is not cut short.
                frames = frames.min(buffered.saturating_sub(cf) as usize);
            }
        }
        let step = 1.0 / (RAMP_SECS * self.rate as f32);
        if self.paused {
            // Only the rest of the pause fade-out.
            frames = frames.min((self.pause_gain / step).ceil().max(1.0) as usize);
        }
        if !any || frames == 0 {
            return 0;
        }
        self.pending.clear();
        self.pending.resize(frames * 2, 0.0);
        for deck in [&mut self.out, &mut self.cur].into_iter().flatten() {
            mix_deck(
                deck,
                frames,
                &mut self.pending,
                &mut self.scratch,
                self.normalize,
                step,
            );
        }
        let pause_target = if self.paused { 0.0 } else { 1.0 };
        for f in self.pending.as_chunks_mut::<2>().0 {
            self.volume += (self.volume_target - self.volume).clamp(-step, step);
            self.pause_gain += (pause_target - self.pause_gain).clamp(-step, step);
            let g = self.volume * self.pause_gain;
            for s in f {
                *s *= g;
                if !s.is_finite() {
                    *s = 0.0;
                }
            }
        }
        apply_ceiling(&mut self.pending);
        frames
    }
}

#[derive(Clone, Copy)]
enum Slot {
    Cur,
    Next,
}

/// Adds `frames` frames of `deck` (gain, fades) into `mix`. A deck with less audio left
/// contributes what it has.
fn mix_deck(
    deck: &mut Deck,
    frames: usize,
    mix: &mut [f32],
    scratch: &mut Vec<f32>,
    normalize: bool,
    step: f32,
) {
    scratch.clear();
    let got = deck.take(frames, scratch);
    if let Some(m) = deck.meter.as_mut().filter(|_| got > 0) {
        m.push(scratch);
    }
    let target = if normalize {
        db_to_lin(deck.item.gain_db)
    } else {
        1.0
    };
    let mut norm = deck.norm.unwrap_or(target);
    for (out, inp) in mix
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(scratch.as_chunks::<2>().0)
    {
        norm += (target - norm).clamp(-step, step);
        let mut g = norm;
        if let Some(r) = deck.fade_in.as_mut() {
            g *= equal_power(r.progress()).1;
            r.pos += 1;
        }
        if let Some(r) = deck.fade_out.as_mut() {
            g *= equal_power(r.progress()).0;
            r.pos += 1;
        }
        out[0] += inp[0] * g;
        out[1] += inp[1] * g;
    }
    deck.norm = Some(norm);
    // Fades run on the output clock, also through frames the deck could not fill.
    for r in [deck.fade_in.as_mut(), deck.fade_out.as_mut()]
        .into_iter()
        .flatten()
    {
        r.pos += (frames - got) as u64;
    }
    if deck.fade_in.as_ref().is_some_and(Ramp::done) {
        deck.fade_in = None;
    }
    deck.played += got as u64;
}

#[cfg(test)]
mod tests;
