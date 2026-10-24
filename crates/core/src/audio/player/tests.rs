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
/// paced ones behave like a real device with a 200 ms buffer, running `speed` times real time.
struct FakeSink {
    out: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    paced: Option<(Instant, f64)>,
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
        if let Some((start, speed)) = self.paced {
            let allowed =
                ((start.elapsed().as_secs_f64() * speed + 0.2) * RATE as f64) as usize * 2;
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
    /// Pacing speed (multiple of real time); `None` = unpaced.
    speed: Option<f64>,
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
            paced: self.speed.map(|v| (Instant::now(), v)),
            written: 0,
            lose_after,
        };
        Ok((Box::new(sink), device.unwrap_or("Fake default").to_string()))
    }
}

#[derive(Default)]
struct Opts {
    paced: bool,
    /// Paced at this multiple of real time (overrides `paced`).
    speed: Option<f64>,
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
            speed: opts.speed.or(opts.paced.then_some(1.0)),
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
        self.play_logged(items, self.calls.clone());
    }

    fn play_logged(&self, items: Vec<PlayItem>, calls: Arc<Mutex<Vec<Call>>>) {
        let provider = FakeProvider {
            items: items.into(),
            calls,
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

    /// Waits until at least `secs` of audio have been written.
    fn wait_output_secs(&self, secs: f32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while (self.out.lock().unwrap().len() as f32) < secs * 2.0 * RATE as f32 {
            assert!(Instant::now() < deadline, "output stuck before {secs} s");
            std::thread::sleep(Duration::from_millis(5));
        }
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
    // Exactly two 20 s tracks back to back (within 1 ms).
    let frames = out.len() as i64;
    assert!(
        (frames - 40 * RATE as i64).abs() <= RATE as i64 / 1000,
        "output is {frames} frames"
    );
    let first = out.iter().position(|s| s.abs() > 1e-3).unwrap();
    let last = out.iter().rposition(|s| s.abs() > 1e-3).unwrap();
    let (mut run, mut longest) = (0, 0);
    for s in &out[first..=last] {
        run = if s.abs() < 1e-3 { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    // A sine passes zero in a sample or two; anything longer is inserted silence.
    assert!(longest <= 4, "silence of {longest} frames");
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

/// Serves `stall-after.flac`: the bytes of tone500.flac up to 20 %, then reads never return.
struct StallAfterSource(LocalSource);

struct StallAfter {
    data: std::io::Cursor<Vec<u8>>,
    limit: u64,
}

impl Read for StallAfter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let pos = self.data.position();
        if pos >= self.limit {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        let n = buf.len().min((self.limit - pos) as usize);
        self.data.read(&mut buf[..n])
    }
}

impl Seek for StallAfter {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.data.seek(p)
    }
}

impl MusicSource for StallAfterSource {
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> std::result::Result<(), SourceError> {
        self.0.walk(on_entry)
    }
    fn open(&self, rel_path: &str) -> std::result::Result<Box<dyn ReadSeek>, SourceError> {
        if rel_path == "stall-after.flac" {
            let data = std::fs::read(tests_dir().join("fixtures-audio/tone500.flac")).unwrap();
            let limit = data.len() as u64 / 5;
            return Ok(Box::new(StallAfter {
                data: std::io::Cursor::new(data),
                limit,
            }));
        }
        self.0.open(rel_path)
    }
    fn describe(&self) -> String {
        "stall-after".into()
    }
}

#[test]
fn next_on_stalled_track_is_immediate() {
    let cfg = PlayerConfig {
        prefetch: PrefetchConfig {
            read_timeout: Duration::from_secs(60),
            ..Default::default()
        },
    };
    let source: Arc<dyn MusicSource> = Arc::new(StallAfterSource(LocalSource::new(tests_dir())));
    let mut h = Harness::new(Opts {
        cfg,
        source: Some(source),
        ..Default::default()
    });
    let stalling = PlayItem {
        id: 1,
        rel_path: "stall-after.flac".into(),
        gain_db: 0.0,
    };
    h.play(vec![stalling, tone(2, 1500)]);
    h.wait("first track", 5, |e| *e == Event::TrackStarted { id: 1 });
    // Let the mixer run dry on the stalled source.
    let mut len = 0;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        let now = h.output().len();
        if now == len && now > 0 {
            break;
        }
        len = now;
    }
    h.player.send(Command::Next);
    h.wait("next track", 3, |e| *e == Event::TrackStarted { id: 2 });
}

#[test]
fn play_during_crossfade_reports_to_old_provider() {
    let mut h = Harness::new(Opts {
        paced: true,
        ..Default::default()
    });
    // A 1 s first track is crossfaded into the second over its whole length.
    let short = PlayItem {
        id: 1,
        rel_path: "fixtures-audio/mono.flac".into(),
        gain_db: 0.0,
    };
    h.play(vec![short, tone(2, 500)]);
    h.wait("crossfade", 5, |e| *e == Event::TrackStarted { id: 2 });
    let new_calls = Arc::new(Mutex::new(Vec::new()));
    h.play_logged(vec![tone(3, 1500)], new_calls.clone());
    h.wait("new selection", 5, |e| *e == Event::TrackStarted { id: 3 });
    h.player.send(Command::Stop);
    h.wait("stop", 5, |e| *e == Event::Idle);
    assert_eq!(h.calls(), [Call::Played(1, None)]);
    assert!(
        new_calls.lock().unwrap().is_empty(),
        "{:?}",
        new_calls.lock().unwrap()
    );
}

#[test]
fn queued_skips_fade_in_skipped_tracks() {
    let mut h = Harness::new(Opts::default());
    h.play(vec![tone(1, 500), tone(2, 1500), tone(3, 2500)]);
    // Both presses arrive before anything has loaded.
    h.player.send(Command::Next);
    h.player.send(Command::Next);
    h.wait("third track", 10, |e| *e == Event::TrackStarted { id: 3 });
    h.player.send(Command::Stop);
    h.wait("stop", 5, |e| *e == Event::Idle);
    assert_eq!(h.started(), [1, 2, 3]);
    let out = left(&h.output());
    let spectrum = spectrum(&out, 240);
    for (k, f) in [(0, 500), (1, 1500)] {
        let first = spectrum
            .iter()
            .position(|a| a[k] > AMP * 0.01)
            .expect("track audible");
        // Skipped tracks fade in and out (peak gain 0.5) instead of starting at full level.
        assert!(
            spectrum[first][k] < AMP * 0.2,
            "{f} Hz starts at {}",
            spectrum[first][k]
        );
        let peak = spectrum.iter().map(|a| a[k]).fold(0.0, f32::max);
        assert!(peak < AMP * 0.6, "{f} Hz peaks at {peak}");
    }
}

#[test]
fn pause_and_resume_fade() {
    let mut h = Harness::new(Opts {
        paced: true,
        ..Default::default()
    });
    h.play(vec![tone(1, 500)]);
    h.wait("start", 5, |e| *e == Event::TrackStarted { id: 1 });
    std::thread::sleep(Duration::from_millis(200));
    h.player.send(Command::Pause);
    h.wait("paused", 5, |e| *e == Event::Paused(true));
    let paused_at = left(&h.output()).len();
    h.player.send(Command::Resume);
    h.wait("resumed", 5, |e| *e == Event::Paused(false));
    std::thread::sleep(Duration::from_millis(300));
    h.player.send(Command::Stop);
    h.wait("stop", 5, |e| *e == Event::Idle);
    let out = left(&h.output());
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let period = (RATE / 500) as usize;
    // Full level 20 ms before the pause point, near silence in its last period, and the
    // resumed audio starts near silence too.
    let ramp = RATE as usize / 50;
    assert!(peak(&out[paused_at - ramp - 2 * period..paused_at - ramp]) > AMP * 0.9);
    assert!(
        peak(&out[paused_at - period..paused_at]) < AMP * 0.15,
        "no fade-out"
    );
    assert!(
        peak(&out[paused_at..paused_at + period]) < AMP * 0.15,
        "no fade-in"
    );
    assert!(peak(&out[paused_at + ramp + period..paused_at + ramp + 3 * period]) > AMP * 0.9);
}

#[test]
fn volume_changes_are_ramped() {
    let mut h = Harness::new(Opts::default());
    h.player.send(Command::SetVolume(0.5));
    h.play(vec![tone(1, 500)]);
    h.wait("idle", 30, |e| *e == Event::Idle);
    let out = left(&h.output());
    let r = RATE as usize;
    let ratio = rms(&out[r..2 * r]) / (AMP / 2f32.sqrt());
    assert!((ratio - 0.5).abs() < 0.03, "volume 0.5 gave {ratio}");

    let mut h = Harness::new(Opts::default());
    h.player.send(Command::SetVolume(0.0));
    h.play(vec![tone(1, 500)]);
    h.wait("idle", 30, |e| *e == Event::Idle);
    let out = left(&h.output());
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    // From 1.0 to 0.0 over 20 ms (960 frames): about 0.75..0.5 in frames 240..480.
    let early = peak(&out[240..480]);
    assert!(early > AMP * 0.4 && early < AMP * 0.8, "ramp level {early}");
    assert!(peak(&out[1000..r]) < 1e-4, "not silent after the ramp");
}

#[test]
fn next_with_ready_track_crossfades_for_0_7_s() {
    let mut h = Harness::new(Opts {
        speed: Some(5.0),
        ..Default::default()
    });
    h.play(vec![tone(1, 500), tone(2, 1500)]);
    h.wait("first track", 5, |e| *e == Event::TrackStarted { id: 1 });
    // 7 s into the track the next one has been preloaded (from 5 s).
    h.wait_output_secs(7.0);
    h.player.send(Command::Next);
    h.wait("second track", 5, |e| *e == Event::TrackStarted { id: 2 });
    h.wait_output_secs(9.0);
    h.player.send(Command::Stop);
    h.wait("stop", 5, |e| *e == Event::Idle);
    let window = 480;
    let both = spectrum(&left(&h.output()), window)
        .iter()
        .filter(|a| a[0] > AMP * 0.02 && a[1] > AMP * 0.02)
        .count();
    let secs = both as f32 * window as f32 / RATE as f32;
    assert!(
        (secs - 0.7).abs() <= 0.05,
        "both tones present for {secs} s"
    );
}
