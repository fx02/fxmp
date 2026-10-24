//! A deck: one track opened and decoded on its own thread into a bounded buffer, so a
//! slow or hung source never blocks the mixer.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use super::decoder::Decoder;
use super::dsp::LoudnessMeter;
use super::player::PlayItem;
use super::prefetch::{PrefetchConfig, PrefetchReader};
use crate::source::MusicSource;

/// Samples decoded per read on a deck thread.
const DECODE_CHUNK: usize = 4096;
/// Decoded audio a deck keeps ready; more than the longest crossfade, so the end of a
/// track is known before its crossfade has to start, and a stalled read is bridged.
const DECK_BUFFER_SECS: usize = 11;

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
pub(super) struct LoadCtx {
    pub(super) source: Arc<dyn MusicSource>,
    pub(super) prefetch: PrefetchConfig,
    /// Called when a deck has new data or changed state.
    pub(super) wake: Arc<dyn Fn() + Send + Sync>,
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
                (ctx.wake)();
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
        (ctx.wake)();
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
                (ctx.wake)();
            }
            Err(e) => return set_phase(Phase::Failed(e.to_string())),
        }
    }
}

/// A fade over `len` frames.
pub(super) struct Ramp {
    pub(super) pos: u64,
    pub(super) len: u64,
}

impl Ramp {
    pub(super) fn new(len: u64) -> Self {
        Self {
            pos: 0,
            len: len.max(1),
        }
    }
    pub(super) fn progress(&self) -> f32 {
        (self.pos as f32 / self.len as f32).min(1.0)
    }
    pub(super) fn done(&self) -> bool {
        self.pos >= self.len
    }
}

pub(super) struct Status {
    pub(super) buffered: usize,
    /// No more data will arrive.
    pub(super) done: bool,
    pub(super) error: Option<String>,
    pub(super) decoded_any: bool,
    pub(super) duration_ms: Option<u64>,
}

pub(super) struct Deck {
    pub(super) item: PlayItem,
    shared: Arc<DeckShared>,
    /// Loudness of what was played, before any gain.
    pub(super) meter: Option<LoudnessMeter>,
    /// Frames consumed by the mixer.
    pub(super) played: u64,
    pub(super) fade_in: Option<Ramp>,
    pub(super) fade_out: Option<Ramp>,
    /// Current normalisation gain (ramped towards its target).
    pub(super) norm: Option<f32>,
    /// Played to its end; reported to the provider when dropped.
    pub(super) reached_end: bool,
}

impl Deck {
    pub(super) fn load(ctx: &LoadCtx, item: PlayItem, rate: u32) -> Self {
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
    pub(super) fn reload(&mut self, ctx: &LoadCtx, old_rate: u32, rate: u32) {
        self.cancel();
        self.played = self.played * rate as u64 / old_rate.max(1) as u64;
        self.shared = spawn_deck_thread(ctx, &self.item.rel_path, rate, self.played);
        self.meter = Some(LoudnessMeter::new(rate));
        self.fade_in = None;
    }

    pub(super) fn status(&self) -> Status {
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
    pub(super) fn failed_early(&self) -> Option<String> {
        let s = self.status();
        if s.decoded_any {
            None
        } else {
            s.error
        }
    }

    pub(super) fn ready(&self) -> bool {
        let s = self.status();
        s.decoded_any && (s.buffered > 0 || s.done)
    }

    /// No buffered audio left and none coming.
    pub(super) fn drained(&self) -> bool {
        let s = self.status();
        s.done && s.buffered == 0
    }

    /// Appends up to `frames` frames to `out`; returns how many.
    pub(super) fn take(&mut self, frames: usize, out: &mut Vec<f32>) -> usize {
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
