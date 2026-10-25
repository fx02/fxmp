//! Manual smoke test: plays the first three audio files of a folder through the default
//! output device with a 5 s crossfade.
//!
//! cargo run -p smp-core --example play_local -- <dir> [max_secs]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smp_core::audio::output::{CpalSink, Sink, SinkFactory};
use smp_core::audio::player::{Command, Event, PlayItem, Player, PlayerConfig, TrackProvider};
use smp_core::failure::Failure;
use smp_core::source::{LocalSource, MusicSource};
use smp_core::{Result, TrackId};

struct Queue(VecDeque<PlayItem>);

impl TrackProvider for Queue {
    fn next_track(&mut self) -> Option<PlayItem> {
        self.0.pop_front()
    }
    fn on_failed(&mut self, id: TrackId, failure: &Failure) {
        println!("failed: {id} ({}: {failure})", failure.code.as_str());
    }
    fn on_played(&mut self, id: TrackId, lufs: Option<f32>) {
        println!("played: {id} (lufs {lufs:?})");
    }
}

/// Opens cpal sinks and keeps the latest one's underrun counter for reporting.
struct Factory(Arc<Mutex<Option<Arc<AtomicU64>>>>);

impl SinkFactory for Factory {
    fn open(&mut self, device: Option<&str>) -> Result<(Box<dyn Sink>, String)> {
        let (sink, name) = CpalSink::open(device)?;
        *self.0.lock().unwrap() = Some(sink.underrun_counter());
        Ok((Box::new(sink), name))
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: play_local <dir> [max_secs]");
    let max_secs: u64 = args.next().map_or(120, |s| s.parse().expect("max_secs"));

    let source = Arc::new(LocalSource::new(&dir));
    let mut files = Vec::new();
    source.walk(&mut |e| files.push(e.rel_path)).expect("walk");
    files.sort();
    let items: VecDeque<PlayItem> = files
        .into_iter()
        .take(3)
        .enumerate()
        .map(|(i, rel_path)| PlayItem {
            id: i as TrackId + 1,
            library: smp_core::DEFAULT_LIBRARY,
            rel_path,
            gain_db: 0.0,
            start_ms: 0,
        })
        .collect();
    for it in &items {
        println!("queued {}: {}", it.id, it.rel_path);
    }

    let underruns = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel();
    let player = Player::spawn(
        source,
        Box::new(Factory(underruns.clone())),
        tx,
        PlayerConfig::default(),
    );
    player.send(Command::SetCrossfade {
        enabled: true,
        secs: 5,
    });
    player.send(Command::Play(Box::new(Queue(items))));

    let start = Instant::now();
    let mut last_report = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(e) => {
                println!("{:6.2}s event: {e:?}", start.elapsed().as_secs_f32());
                if e == Event::Idle {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("engine stopped"),
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            let n = underruns
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.load(Ordering::Relaxed));
            println!(
                "{:6.2}s underrun callbacks so far: {n:?}",
                start.elapsed().as_secs_f32()
            );
        }
        if start.elapsed() >= Duration::from_secs(max_secs) {
            println!("time limit reached");
            break;
        }
    }
}
