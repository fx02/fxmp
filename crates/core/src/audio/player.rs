//! Player engine: a dedicated thread that mixes tracks into a [`Sink`].
//!
//! Each track ("deck") is opened and decoded on its own thread into a bounded buffer, so a
//! slow or hung source never blocks mixing. The engine thread pulls from at most two
//! audible decks (outgoing + current), applies fades, normalisation and volume, and writes
//! to the sink without blocking.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::decoder::Decoder;
use super::dsp::{apply_ceiling, db_to_lin, equal_power, LoudnessMeter};
use super::output::{Sink, SinkFactory, SinkWrite};
use super::prefetch::{PrefetchConfig, PrefetchReader};
use crate::source::MusicSource;
use crate::TrackId;

/// Frames mixed per block.
const BLOCK: usize = 1024;
/// Samples decoded per read on a deck thread.
const DECODE_CHUNK: usize = 4096;
/// Decoded audio a deck keeps ready; more than the longest crossfade, so the end of a
/// track is known before its crossfade has to start, and a stalled read is bridged.
const DECK_BUFFER_SECS: usize = 11;
const NEXT_FADE_SECS: f32 = 0.7;
const SKIP_FADE_SECS: f32 = 0.3;
/// The next track is opened this long (plus the crossfade) before the current one ends.
const PRELOAD_LEAD_SECS: u64 = 10;
const RAMP_SECS: f32 = 0.02;
const MAX_CONSECUTIVE_FAILURES: u32 = 20;
/// How long the mix may hold at a crossfade point for a next track that is still opening.
const READY_GRACE: Duration = Duration::from_millis(100);
const SINK_RETRY: Duration = Duration::from_secs(1);
const SINK_FULL_WAIT: Duration = Duration::from_millis(10);
const IDLE_WAIT: Duration = Duration::from_millis(500);

/// A track to play, as handed out by a [`TrackProvider`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlayItem {
    pub id: TrackId,
    pub rel_path: String,
    /// Normalisation gain, applied when normalisation is on.
    pub gain_db: f32,
}

/// Decides what plays next and hears how it went.
pub trait TrackProvider: Send {
    fn next_track(&mut self) -> Option<PlayItem>;
    fn on_failed(&mut self, id: TrackId);
    fn on_played(&mut self, id: TrackId, measured_lufs: Option<f32>);
}

pub enum Command {
    Play(Box<dyn TrackProvider>),
    Pause,
    Resume,
    TogglePause,
    Next,
    /// Linear volume, 0..1.
    SetVolume(f32),
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
    TrackStarted { id: TrackId },
    TrackSkipped { id: TrackId, reason: String },
    Paused(bool),
    DeviceChanged(String),
    Idle,
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
        std::thread::Builder::new()
            .name("player".into())
            .spawn(move || engine.run())
            .expect("spawn player thread");
        Self { tx }
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
// Decks

#[derive(Clone)]
enum Phase {
    Loading,
    Decoding,
    Ended,
    Failed(String),
}

struct DeckBuf {
    samples: VecDeque<f32>,
    phase: Phase,
    /// Some audio was decoded (the track did not fail before its first sample).
    decoded_any: bool,
    duration_ms: Option<u64>,
    cancelled: bool,
}

struct DeckShared {
    buf: Mutex<DeckBuf>,
    /// Signalled when the engine takes samples or cancels.
    space: Condvar,
}

impl DeckShared {
    fn lock(&self) -> MutexGuard<'_, DeckBuf> {
        self.buf.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What a deck thread needs to open and decode a track.
#[derive(Clone)]
struct LoadCtx {
    source: Arc<dyn MusicSource>,
    prefetch: PrefetchConfig,
    wake: Sender<Msg>,
}

fn spawn_deck_thread(
    ctx: &LoadCtx,
    rel_path: &str,
    rate: u32,
    skip_frames: u64,
) -> Arc<DeckShared> {
    let shared = Arc::new(DeckShared {
        buf: Mutex::new(DeckBuf {
            samples: VecDeque::new(),
            phase: Phase::Loading,
            decoded_any: false,
            duration_ms: None,
            cancelled: false,
        }),
        space: Condvar::new(),
    });
    let (s, ctx, path) = (shared.clone(), ctx.clone(), rel_path.to_string());
    std::thread::Builder::new()
        .name("player-deck".into())
        .spawn(move || {
            let run =
                std::panic::AssertUnwindSafe(|| deck_thread(&s, &ctx, &path, rate, skip_frames));
            if std::panic::catch_unwind(run).is_err() {
                // A decoder panic must not leave the engine waiting on this deck forever.
                s.lock().phase = Phase::Failed("decoder panicked".into());
                let _ = ctx.wake.send(Msg::Wake);
            }
        })
        .expect("spawn deck thread");
    shared
}

fn open_decoder(ctx: &LoadCtx, rel_path: &str, rate: u32) -> crate::Result<Decoder> {
    let mut reader = ctx.source.open(rel_path)?;
    let len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;
    let prefetch = PrefetchReader::new(reader, len, ctx.prefetch.clone());
    let ext = Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    Decoder::open(Box::new(prefetch), Some(len), ext, rate)
}

fn deck_thread(shared: &DeckShared, ctx: &LoadCtx, rel_path: &str, rate: u32, skip_frames: u64) {
    let set_phase = |phase: Phase| {
        shared.lock().phase = phase;
        let _ = ctx.wake.send(Msg::Wake);
    };
    let mut dec = match open_decoder(ctx, rel_path, rate) {
        Ok(d) => d,
        Err(e) => return set_phase(Phase::Failed(e.to_string())),
    };
    {
        let mut b = shared.lock();
        b.duration_ms = dec.duration_ms();
        b.phase = Phase::Decoding;
    }
    let cap = DECK_BUFFER_SECS * rate as usize * 2;
    let mut skip = skip_frames as usize * 2;
    let mut chunk = vec![0.0f32; DECODE_CHUNK];
    loop {
        {
            let mut b = shared.lock();
            while b.samples.len() >= cap && !b.cancelled {
                b = shared.space.wait(b).unwrap_or_else(|e| e.into_inner());
            }
            if b.cancelled {
                return;
            }
        }
        match dec.read(&mut chunk) {
            Ok(0) => return set_phase(Phase::Ended),
            Ok(n) => {
                let drop_n = skip.min(n);
                skip -= drop_n;
                let mut b = shared.lock();
                b.samples.extend(&chunk[drop_n..n]);
                b.decoded_any = true;
                drop(b);
                let _ = ctx.wake.send(Msg::Wake);
            }
            Err(e) => return set_phase(Phase::Failed(e.to_string())),
        }
    }
}

/// A fade over `len` frames.
struct Ramp {
    pos: u64,
    len: u64,
}

impl Ramp {
    fn new(len: u64) -> Self {
        Self {
            pos: 0,
            len: len.max(1),
        }
    }
    fn progress(&self) -> f32 {
        (self.pos as f32 / self.len as f32).min(1.0)
    }
    fn done(&self) -> bool {
        self.pos >= self.len
    }
}

struct Status {
    buffered: usize,
    /// No more data will arrive.
    done: bool,
    error: Option<String>,
    decoded_any: bool,
    duration_ms: Option<u64>,
}

struct Deck {
    item: PlayItem,
    shared: Arc<DeckShared>,
    /// Loudness of what was played, before any gain.
    meter: Option<LoudnessMeter>,
    /// Frames consumed by the mixer.
    played: u64,
    fade_in: Option<Ramp>,
    fade_out: Option<Ramp>,
    /// Current normalisation gain (ramped towards its target).
    norm: Option<f32>,
    /// Played to its end; reported to the provider when dropped.
    reached_end: bool,
}

impl Deck {
    fn load(ctx: &LoadCtx, item: PlayItem, rate: u32) -> Self {
        let shared = spawn_deck_thread(ctx, &item.rel_path, rate, 0);
        Self {
            item,
            shared,
            meter: Some(LoudnessMeter::new(rate)),
            played: 0,
            fade_in: None,
            fade_out: None,
            norm: None,
            reached_end: false,
        }
    }

    /// Restarts decoding at a new output rate, resuming where playback was.
    fn reload(&mut self, ctx: &LoadCtx, old_rate: u32, rate: u32) {
        self.cancel();
        self.played = self.played * rate as u64 / old_rate.max(1) as u64;
        self.shared = spawn_deck_thread(ctx, &self.item.rel_path, rate, self.played);
        self.meter = Some(LoudnessMeter::new(rate));
        self.fade_in = None;
    }

    fn status(&self) -> Status {
        let b = self.shared.lock();
        let (done, error) = match &b.phase {
            Phase::Loading | Phase::Decoding => (false, None),
            Phase::Ended => (true, None),
            Phase::Failed(e) => (true, Some(e.clone())),
        };
        Status {
            buffered: b.samples.len() / 2,
            done,
            error,
            decoded_any: b.decoded_any,
            duration_ms: b.duration_ms,
        }
    }

    /// Opening or decoding failed before any audio came out.
    fn failed_early(&self) -> Option<String> {
        let s = self.status();
        if s.decoded_any {
            None
        } else {
            s.error
        }
    }

    fn ready(&self) -> bool {
        let s = self.status();
        s.decoded_any && (s.buffered > 0 || s.done)
    }

    /// No buffered audio left and none coming.
    fn drained(&self) -> bool {
        let s = self.status();
        s.done && s.buffered == 0
    }

    /// Appends up to `frames` frames to `out`; returns how many.
    fn take(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
        let mut b = self.shared.lock();
        let n = (frames * 2).min(b.samples.len());
        out.extend(b.samples.drain(..n));
        drop(b);
        self.shared.space.notify_all();
        n / 2
    }

    fn cancel(&self) {
        self.shared.lock().cancelled = true;
        self.shared.space.notify_all();
    }
}

impl Drop for Deck {
    fn drop(&mut self) {
        self.cancel();
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

    paused: bool,
    volume: f32,
    volume_target: f32,
    crossfade_secs: Option<u8>,
    normalize: bool,

    /// Mixed audio the sink has not accepted yet.
    pending: Vec<f32>,
    pending_pos: usize,
    scratch: Vec<f32>,
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
                wake,
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
            paused: false,
            volume: 1.0,
            volume_target: 1.0,
            crossfade_secs: Some(5),
            normalize: true,
            pending: Vec::with_capacity(BLOCK * 2),
            pending_pos: 0,
            scratch: Vec::with_capacity(BLOCK * 2),
        }
    }

    fn run(mut self) {
        let mut wait = None;
        loop {
            if !self.drain_messages(wait) {
                return;
            }
            self.update();
            wait = match self.step() {
                Step::Progress => None,
                Step::Wait(d) => Some(d),
            };
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
            Command::Play(p) => {
                self.next = None;
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
            Command::Resume => self.set_paused(false),
            Command::TogglePause => self.set_paused(!self.paused),
            Command::Next => {
                if self.provider.is_some() && self.stopped_feeding && self.nothing_loaded() {
                    // Give up on waiting after too many failures: try again.
                    self.stopped_feeding = false;
                    self.idle_reported = false;
                    self.failures = 0;
                } else if !self.nothing_loaded() || self.provider.is_some() {
                    self.pending_skips += 1;
                }
            }
            Command::SetVolume(v) => {
                if v.is_finite() {
                    self.volume_target = v.clamp(0.0, 1.0);
                }
            }
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

    fn set_paused(&mut self, p: bool) {
        self.paused = p;
        self.emit(Event::Paused(p));
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
            if let Some(reason) = failed {
                let deck = self.slot(slot).take().expect("checked above");
                log::warn!("skipping {}: {reason}", deck.item.rel_path);
                if let Some(p) = self.provider.as_mut() {
                    p.on_failed(deck.item.id);
                }
                self.emit(Event::TrackSkipped {
                    id: deck.item.id,
                    reason,
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
        deck.fade_in = fade_in.map(Ramp::new);
        self.failures = 0;
        self.grace_since = None;
        self.emit(Event::TrackStarted { id: deck.item.id });
        self.cur = Some(deck);
    }

    /// Drops the outgoing deck, reporting it if it had played to its end.
    fn drop_outgoing(&mut self) {
        if let Some(deck) = self.out.take() {
            self.report_end(deck);
        }
    }

    fn report_end(&mut self, deck: Deck) {
        if !deck.reached_end {
            return;
        }
        let Some(p) = self.provider.as_mut() else {
            return;
        };
        match deck.status().error {
            Some(reason) => {
                log::warn!("{} broke off: {reason}", deck.item.rel_path);
                p.on_failed(deck.item.id);
                self.emit(Event::TrackSkipped {
                    id: deck.item.id,
                    reason,
                });
            }
            None => {
                let mut deck = deck;
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
                // The press is for the track that was about to start.
                self.drop_outgoing();
                self.start_next(None);
            } else {
                self.drop_outgoing();
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
                self.report_end(deck);
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
        if self.paused || !self.active() {
            return Step::Wait(IDLE_WAIT);
        }
        if self.sink.is_none() {
            let wait = self
                .sink_retry_at
                .map_or(SINK_RETRY, |t| t.saturating_duration_since(Instant::now()));
            return Step::Wait(wait);
        }
        if self.pending_pos < self.pending.len() {
            return self.write_pending();
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
        let cf = self.crossfade_frames();
        for (i, deck) in [&self.out, &self.cur].into_iter().enumerate() {
            let Some(deck) = deck else { continue };
            any = true;
            let s = deck.status();
            if !s.done {
                frames = frames.min(s.buffered);
            }
            let is_cur = i == 1;
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
        if !any || frames == 0 {
            return 0;
        }
        self.pending.clear();
        self.pending.resize(frames * 2, 0.0);
        let step = 1.0 / (RAMP_SECS * self.rate as f32);
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
        for f in self.pending.as_chunks_mut::<2>().0 {
            self.volume += (self.volume_target - self.volume).clamp(-step, step);
            for s in f {
                *s *= self.volume;
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
    if got == 0 {
        return;
    }
    if let Some(m) = deck.meter.as_mut() {
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
    if deck.fade_in.as_ref().is_some_and(Ramp::done) {
        deck.fade_in = None;
    }
    deck.played += got as u64;
}

#[cfg(test)]
mod tests;
