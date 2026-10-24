use super::*;
use crate::audio::output::{Sink, SinkWrite};
use crate::source::{FileEntry, LocalSource, ReadSeek, SourceError};
use crate::Result;
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
/// Peak amplitude of the generated tones (ffmpeg's `sine` default).
const AMP: f32 = 0.125;
const FREQS: [u32; 7] = [500, 1500, 2500, 3500, 4500, 5500, 6500];

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn tone(id: TrackId, freq: u32) -> PlayItem {
    PlayItem {
        id,
        rel_path: format!("fixtures-audio/tone{freq}.flac"),
        gain_db: 0.0,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Failed(TrackId),
    Played(TrackId, Option<f32>),
}

struct FakeProvider {
    items: VecDeque<PlayItem>,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl TrackProvider for FakeProvider {
    fn next_track(&mut self) -> Option<PlayItem> {
        self.items.pop_front()
    }
    fn on_failed(&mut self, id: TrackId) {
        self.calls.lock().unwrap().push(Call::Failed(id));
    }
    fn on_played(&mut self, id: TrackId, measured_lufs: Option<f32>) {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Played(id, measured_lufs));
    }
}

/// Records everything written at 48 kHz. Unpaced sinks take samples as fast as they come;
/// paced ones behave like a real device with a 200 ms buffer.
struct FakeSink {
    out: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    paced: Option<Instant>,
    written: usize,
    lose_after: Option<usize>,
}

impl Sink for FakeSink {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn write(&mut self, stereo: &[f32]) -> SinkWrite {
        if self.lose_after.is_some_and(|n| self.written >= n) {
            return SinkWrite::DeviceLost;
        }
        let mut n = stereo.len();
        if let Some(start) = self.paced {
            let allowed = ((start.elapsed().as_secs_f64() + 0.2) * RATE as f64) as usize * 2;
            n = n.min(allowed.saturating_sub(self.written));
        }
        n &= !1;
        self.out.lock().unwrap().extend_from_slice(&stereo[..n]);
        self.written += n;
        SinkWrite::Ok(n)
    }
}

struct FakeFactory {
    out: Arc<Mutex<Vec<f32>>>,
    opened: Arc<Mutex<Vec<Option<String>>>>,
    paced: bool,
    /// The first sink opened reports `DeviceLost` after this many samples.
    lose_first_after: Option<usize>,
    /// Sample rate of sinks after the first one.
    later_rate: u32,
}

impl SinkFactory for FakeFactory {
    fn open(&mut self, device: Option<&str>) -> Result<(Box<dyn Sink>, String)> {
        let mut opened = self.opened.lock().unwrap();
        let lose_after = if opened.is_empty() {
            self.lose_first_after
        } else {
            None
        };
        opened.push(device.map(str::to_string));
        let rate = if opened.len() == 1 {
            RATE
        } else {
            self.later_rate
        };
        let sink = FakeSink {
            out: self.out.clone(),
            rate,
            paced: self.paced.then(Instant::now),
            written: 0,
            lose_after,
        };
        Ok((Box::new(sink), device.unwrap_or("Fake default").to_string()))
    }
}

#[derive(Default)]
struct Opts {
    paced: bool,
    lose_first_after: Option<usize>,
    later_rate: Option<u32>,
    cfg: PlayerConfig,
    source: Option<Arc<dyn MusicSource>>,
}

struct Harness {
    player: Player,
    events: Receiver<Event>,
    seen: Vec<Event>,
    out: Arc<Mutex<Vec<f32>>>,
    calls: Arc<Mutex<Vec<Call>>>,
    opened: Arc<Mutex<Vec<Option<String>>>>,
}

impl Harness {
    fn new(opts: Opts) -> Self {
        let out = Arc::new(Mutex::new(Vec::new()));
        let opened = Arc::new(Mutex::new(Vec::new()));
        let factory = FakeFactory {
            out: out.clone(),
            opened: opened.clone(),
            paced: opts.paced,
            lose_first_after: opts.lose_first_after,
            later_rate: opts.later_rate.unwrap_or(RATE),
        };
        let source = opts
            .source
            .unwrap_or_else(|| Arc::new(LocalSource::new(tests_dir())));
        let (tx, events) = mpsc::channel();
        let player = Player::spawn(source, Box::new(factory), tx, opts.cfg);
        Self {
            player,
            events,
            seen: Vec::new(),
            out,
            calls: Arc::new(Mutex::new(Vec::new())),
            opened,
        }
    }

    fn play(&self, items: Vec<PlayItem>) {
        let provider = FakeProvider {
            items: items.into(),
            calls: self.calls.clone(),
        };
        self.player.send(Command::Play(Box::new(provider)));
    }

    /// Waits for an event matching `pred`; returns it. Panics on timeout or engine death.
    fn wait(&mut self, what: &str, secs: u64, pred: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(e) => {
                    self.seen.push(e.clone());
                    if pred(&e) {
                        return e;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for {what}; saw {:?}", self.seen)
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("engine died waiting for {what}; saw {:?}", self.seen)
                }
            }
        }
    }

    fn started(&self) -> Vec<TrackId> {
        self.seen
            .iter()
            .filter_map(|e| match e {
                Event::TrackStarted { id } => Some(*id),
                _ => None,
            })
            .collect()
    }

    fn output(&self) -> Vec<f32> {
        self.out.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

fn left(stereo: &[f32]) -> Vec<f32> {
    stereo.iter().step_by(2).copied().collect()
}

/// Amplitude of `freq` in `x` (Hann-windowed Goertzel), scaled so a full-window sine of
/// peak amplitude A reads as about A.
fn amplitude(x: &[f32], freq: u32) -> f32 {
    amplitude_at(x, freq, RATE)
}

fn amplitude_at(x: &[f32], freq: u32, rate: u32) -> f32 {
    let n = x.len();
    let w = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let hann = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
        let s0 = v as f64 * hann + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    (power.sqrt() * 4.0 / n as f64) as f32
}

/// Per window: amplitude of each of `FREQS`.
fn spectrum(mono: &[f32], window: usize) -> Vec<[f32; 7]> {
    mono.chunks_exact(window)
        .map(|w| {
            let mut a = [0.0; 7];
            for (k, f) in FREQS.iter().enumerate() {
                a[k] = amplitude(w, *f);
            }
            a
        })
        .collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

#[test]
fn plays_tracks_in_provider_order() {
    let mut h = Harness::new(Opts::default());
    h.play(vec![tone(1, 500), tone(2, 1500), tone(3, 2500)]);
    h.wait("idle", 60, |e| *e == Event::Idle);
    assert_eq!(h.started(), [1, 2, 3]);
    assert_eq!(
        h.calls(),
        [
            Call::Played(1, None),
            Call::Played(2, None),
            Call::Played(3, None)
        ]
    );

    let out = h.output();
    // Three 20 s tracks overlapping twice by the default 5 s crossfade.
    let secs = out.len() as f32 / 2.0 / RATE as f32;
    assert!((secs - 50.0).abs() < 0.5, "output is {secs} s");
    let mut order = Vec::new();
    for a in spectrum(&left(&out), 4800) {
        let (k, peak) = a
            .iter()
            .enumerate()
            .fold((0, 0.0), |b, (k, &v)| if v > b.1 { (k, v) } else { b });
        if peak > AMP * 0.5 && order.last() != Some(&FREQS[k]) {
            order.push(FREQS[k]);
        }
    }
    assert_eq!(order, [500, 1500, 2500]);
}

#[test]
fn broken_track_is_skipped_and_reported() {
    let mut h = Harness::new(Opts::default());
    let broken = PlayItem {
        id: 1,
        rel_path: "fixtures/broken.mp3".into(),
        gain_db: 0.0,
    };
    h.play(vec![broken, tone(2, 500)]);
    h.wait("skip", 10, |e| {
        matches!(e, Event::TrackSkipped { id: 1, .. })
    });
    h.wait("start of 2", 10, |e| *e == Event::TrackStarted { id: 2 });
    h.wait("idle", 30, |e| *e == Event::Idle);
    assert_eq!(h.started(), [2]);
    assert_eq!(h.calls(), [Call::Failed(1), Call::Played(2, None)]);
    let out = left(&h.output());
    assert!(amplitude(&out[RATE as usize..2 * RATE as usize], 500) > AMP * 0.8);
}

#[test]
fn twenty_failures_in_a_row_go_idle_until_next() {
    let mut h = Harness::new(Opts::default());
    let broken = |id| PlayItem {
        id,
        rel_path: "fixtures/broken.mp3".into(),
        gain_db: 0.0,
    };
    let mut items: Vec<PlayItem> = (1..=20).map(broken).collect();
    items.push(tone(21, 500));
    h.play(items);
    h.wait("idle", 10, |e| *e == Event::Idle);
    let skipped = h
        .seen
        .iter()
        .filter(|e| matches!(e, Event::TrackSkipped { .. }))
        .count();
    assert_eq!(skipped, 20);
    assert!(h.started().is_empty());
    // Next tries again.
    h.player.send(Command::Next);
    h.wait("retry", 10, |e| *e == Event::TrackStarted { id: 21 });
}

/// Serves `stall.flac` as a file whose reads never return; everything else from fixtures.
struct StallSource(LocalSource);

struct Stall(u64);

impl Read for Stall {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}

impl Seek for Stall {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.0 = match p {
            SeekFrom::Start(n) => n,
            SeekFrom::End(d) => (1_000_000i64 + d) as u64,
            SeekFrom::Current(d) => (self.0 as i64 + d) as u64,
        };
        Ok(self.0)
    }
}

impl MusicSource for StallSource {
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> std::result::Result<(), SourceError> {
        self.0.walk(on_entry)
    }
    fn open(&self, rel_path: &str) -> std::result::Result<Box<dyn ReadSeek>, SourceError> {
        if rel_path == "stall.flac" {
            return Ok(Box::new(Stall(0)));
        }
        self.0.open(rel_path)
    }
    fn describe(&self) -> String {
        "stall".into()
    }
}

#[test]
fn stalled_source_skips_track() {
    let cfg = PlayerConfig {
        prefetch: PrefetchConfig {
            read_timeout: Duration::from_millis(200),
            ..Default::default()
        },
    };
    let source: Arc<dyn MusicSource> = Arc::new(StallSource(LocalSource::new(tests_dir())));
    let mut h = Harness::new(Opts {
        cfg,
        source: Some(source),
        ..Default::default()
    });
    let t = Instant::now();
    h.play(vec![
        PlayItem {
            id: 1,
            rel_path: "stall.flac".into(),
            gain_db: 0.0,
        },
        tone(2, 500),
    ]);
    h.wait("skip", 5, |e| {
        matches!(e, Event::TrackSkipped { id: 1, .. })
    });
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "skip took {:?}",
        t.elapsed()
    );
    h.wait("start of 2", 5, |e| *e == Event::TrackStarted { id: 2 });
    assert_eq!(h.calls().first(), Some(&Call::Failed(1)));
}

#[test]
fn crossfade_overlaps_for_configured_secs() {
    let mut h = Harness::new(Opts::default());
    h.player.send(Command::SetCrossfade {
        enabled: true,
        secs: 2,
    });
    h.play(vec![tone(1, 500), tone(2, 1500)]);
    h.wait("idle", 60, |e| *e == Event::Idle);
    let window = 480; // 10 ms
    let both = spectrum(&left(&h.output()), window)
        .iter()
        .filter(|a| a[0] > AMP * 0.02 && a[1] > AMP * 0.02)
        .count();
    let secs = both as f32 * window as f32 / RATE as f32;
    assert!((secs - 2.0).abs() <= 0.1, "both tones present for {secs} s");
}

#[test]
fn no_crossfade_means_no_gap() {
    let mut h = Harness::new(Opts::default());
    h.player.send(Command::SetCrossfade {
        enabled: false,
        secs: 5,
    });
    h.play(vec![tone(1, 500), tone(2, 1500)]);
    h.wait("idle", 60, |e| *e == Event::Idle);
    let out = left(&h.output());
    let secs = out.len() as f32 / RATE as f32;
    assert!((secs - 40.0).abs() < 0.1, "output is {secs} s");
    let first = out.iter().position(|s| s.abs() > 1e-3).unwrap();
    let last = out.iter().rposition(|s| s.abs() > 1e-3).unwrap();
    let (mut run, mut longest) = (0, 0);
    for s in &out[first..=last] {
        run = if s.abs() < 1e-3 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    assert!(longest <= RATE as usize / 20, "silence of {longest} frames");
    // Both tracks are there, one after the other.
    assert!(amplitude(&out[10 * RATE as usize..11 * RATE as usize], 500) > AMP * 0.8);
    assert!(amplitude(&out[30 * RATE as usize..31 * RATE as usize], 1500) > AMP * 0.8);
}

#[test]
fn rapid_next_skips_once_per_press() {
    let mut h = Harness::new(Opts {
        paced: true,
        ..Default::default()
    });
    h.play(
        FREQS
            .iter()
            .enumerate()
            .map(|(i, f)| tone(i as TrackId + 1, *f))
            .collect(),
    );
    h.wait("first track", 5, |e| *e == Event::TrackStarted { id: 1 });
    std::thread::sleep(Duration::from_millis(300));
    for _ in 0..5 {
        h.player.send(Command::Next);
        std::thread::sleep(Duration::from_millis(10));
    }
    h.wait("sixth track", 10, |e| *e == Event::TrackStarted { id: 6 });
    std::thread::sleep(Duration::from_millis(500));
    h.player.send(Command::Stop);
    h.wait("idle after stop", 5, |e| *e == Event::Idle);
    assert_eq!(h.started(), [1, 2, 3, 4, 5, 6]);

    let out = left(&h.output());
    for (i, a) in spectrum(&out, 240).iter().enumerate() {
        let present = a.iter().filter(|&&v| v > AMP * 0.25).count();
        assert!(present <= 2, "window {i}: {present} tones at once: {a:?}");
    }
    // The last track is really playing.
    let tail = &out[out.len() - RATE as usize / 10..];
    assert!(amplitude(tail, 5500) > AMP * 0.5);
}

#[test]
fn pause_stops_output() {
    let mut h = Harness::new(Opts {
        paced: true,
        ..Default::default()
    });
    h.play(vec![tone(1, 500), tone(2, 1500)]);
    h.wait("start", 5, |e| *e == Event::TrackStarted { id: 1 });
    std::thread::sleep(Duration::from_millis(200));
    h.player.send(Command::Pause);
    h.wait("paused", 5, |e| *e == Event::Paused(true));
    let n1 = h.output().len();
    assert!(n1 > 0);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(h.output().len(), n1, "output grew while paused");
    h.player.send(Command::TogglePause);
    h.wait("resumed", 5, |e| *e == Event::Paused(false));
    std::thread::sleep(Duration::from_millis(300));
    assert!(h.output().len() > n1, "output did not resume");
}

#[test]
fn device_error_falls_back_to_default() {
    let mut h = Harness::new(Opts {
        lose_first_after: Some(RATE as usize * 2),
        ..Default::default()
    });
    h.player.send(Command::SetDevice(Some("Fake USB".into())));
    h.play(vec![tone(1, 500)]);
    h.wait("first device", 5, |e| {
        *e == Event::DeviceChanged("Fake USB".into())
    });
    h.wait("fallback", 5, |e| {
        *e == Event::DeviceChanged("Fake default".into())
    });
    h.wait("idle", 30, |e| *e == Event::Idle);
    assert_eq!(
        *h.opened.lock().unwrap(),
        [Some("Fake USB".to_string()), None]
    );
    // Nothing was lost in the switch.
    let secs = h.output().len() as f32 / 2.0 / RATE as f32;
    assert!((secs - 20.0).abs() < 0.1, "output is {secs} s");
}

#[test]
fn rate_change_resumes_track_at_new_rate() {
    let lose_after = RATE as usize * 2 * 5; // 5 s at 48 kHz
    let mut h = Harness::new(Opts {
        lose_first_after: Some(lose_after),
        later_rate: Some(44_100),
        ..Default::default()
    });
    h.play(vec![tone(1, 500)]);
    h.wait("fallback", 10, |e| {
        *e == Event::DeviceChanged("Fake default".into())
    });
    h.wait("idle", 30, |e| *e == Event::Idle);
    assert_eq!(h.started(), [1]);
    assert_eq!(h.calls(), [Call::Played(1, None)]);
    let out = left(&h.output());
    // Audio mixed for the old device but not yet written is dropped at the switch.
    let after = &out[lose_after / 2..];
    let secs = after.len() as f32 / 44_100.0;
    assert!((secs - 15.0).abs() < 0.1, "{secs} s after the switch");
    let mid = &after[5 * 44_100..6 * 44_100];
    assert!(
        amplitude_at(mid, 500, 44_100) > AMP * 0.8,
        "wrong pitch after the switch"
    );
}

/// Plays a 0 dB track then a -6 dB one back to back; returns the RMS ratio second/first.
fn gain_ratio(normalize: bool) -> f32 {
    let mut h = Harness::new(Opts::default());
    h.player.send(Command::SetCrossfade {
        enabled: false,
        secs: 5,
    });
    h.player.send(Command::SetNormalize(normalize));
    let quiet = PlayItem {
        gain_db: -6.0,
        ..tone(2, 1500)
    };
    h.play(vec![tone(1, 500), quiet]);
    h.wait("idle", 60, |e| *e == Event::Idle);
    let out = left(&h.output());
    let r = RATE as usize;
    rms(&out[25 * r..35 * r]) / rms(&out[5 * r..15 * r])
}

#[test]
fn normalization_applies_gain() {
    let on = gain_ratio(true);
    assert!((on - 0.5).abs() <= 0.05, "normalised ratio {on}");
    let off = gain_ratio(false);
    assert!((off - 1.0).abs() <= 0.1, "unnormalised ratio {off}");
}
