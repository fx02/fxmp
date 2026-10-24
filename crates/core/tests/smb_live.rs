//! Live tests against real SMB servers. Read-only: nothing is ever written,
//! renamed or deleted on the servers.
//!
//! Credentials come from `.env.test` at the workspace root (see `.env.test.example`).
//! Every test runs against the primary server (`SMB_TEST_*`) and, when
//! `SMB_TEST2_HOST` is set, also against the secondary one (`SMB_TEST2_*`, blank
//! values fall back to the primary's).
//!
//! Run with: `cargo test -p smp-core --test smb_live -- --ignored --test-threads=1`

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use smp_core::catalog::Catalog;
use smp_core::scan::{scan, scan_with, ScanOptions};
use smp_core::source::smb::{SmbConfig, SmbSource};
use smp_core::source::{FileEntry, MusicSource, SourceError};
use smp_core::Error;

const CHUNK: usize = 64 * 1024;

fn load_env() -> HashMap<String, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env.test");
    let text = std::fs::read_to_string(&path).expect(".env.test missing (see .env.test.example)");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

/// Configs for the primary server and, if configured, the secondary one.
fn servers() -> Vec<SmbConfig> {
    let env = load_env();
    let get = |key: &str| env.get(key).cloned().filter(|v| !v.is_empty());
    let primary = |field: &str| get(&format!("SMB_TEST_{field}")).unwrap_or_default();
    let mut out = vec![SmbConfig {
        host: primary("HOST"),
        share: primary("SHARE"),
        root: primary("PATH"),
        username: primary("USER"),
        password: primary("PASS"),
        domain: get("SMB_TEST_DOMAIN"),
    }];
    if let Some(host) = get("SMB_TEST2_HOST") {
        let second =
            |field: &str| get(&format!("SMB_TEST2_{field}")).unwrap_or_else(|| primary(field));
        out.push(SmbConfig {
            host,
            share: second("SHARE"),
            root: second("PATH"),
            username: second("USER"),
            password: second("PASS"),
            domain: get("SMB_TEST2_DOMAIN"),
        });
    }
    out
}

fn connect(cfg: &SmbConfig) -> SmbSource {
    let host = cfg.host.clone();
    SmbSource::connect(cfg.clone()).unwrap_or_else(|e| panic!("connect to {host} failed: {e}"))
}

/// First audio file of at least `min_size` bytes, found without walking the whole share.
fn sample_file(src: &SmbSource, min_size: u64) -> FileEntry {
    src.walk_limited(1, &mut |e| e.size >= min_size)
        .expect("walk failed")
        .pop()
        .expect("no audio file found")
}

fn read_exact_at(r: &mut dyn smp_core::source::ReadSeek, pos: u64, len: usize) -> Vec<u8> {
    assert_eq!(r.seek(SeekFrom::Start(pos)).unwrap(), pos);
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).unwrap();
    buf
}

#[test]
#[ignore]
fn live_walk_lists_audio() {
    for cfg in servers() {
        let src = connect(&cfg);
        let found = src.walk_limited(50, &mut |_| true).unwrap();
        assert!(!found.is_empty(), "{}: no audio files", cfg.host);
        for e in &found {
            assert!(!e.rel_path.starts_with('/'), "{}: {}", cfg.host, e.rel_path);
            assert!(!e.rel_path.contains('\\'), "{}: {}", cfg.host, e.rel_path);
            assert!(e.size > 0 && e.mtime > 0, "{}: {:?}", cfg.host, e);
        }
        println!("{}: walk ok, first = {}", cfg.host, found[0].rel_path);
    }
}

#[test]
#[ignore]
fn live_open_read_and_seek() {
    for cfg in servers() {
        let src = connect(&cfg);
        let entry = sample_file(&src, 4 * CHUNK as u64);
        let mid = entry.size / 2;

        let mut a = src.open(&entry.rel_path).unwrap();
        let head = read_exact_at(&mut *a, 0, CHUNK);
        let middle = read_exact_at(&mut *a, mid, CHUNK);
        assert_eq!(a.seek(SeekFrom::End(0)).unwrap(), entry.size);
        let mut rest = Vec::new();
        assert_eq!(a.read_to_end(&mut rest).unwrap(), 0);

        // A second, independent handle sees the same bytes (read in reverse order).
        let mut b = src.open(&entry.rel_path).unwrap();
        assert_eq!(read_exact_at(&mut *b, mid, CHUNK), middle);
        assert_eq!(read_exact_at(&mut *b, 0, CHUNK), head);
        assert_ne!(head, middle);
        println!("{}: read/seek ok on a {} byte file", cfg.host, entry.size);

        assert!(matches!(
            src.open("no/such/file.mp3").err(),
            Some(SourceError::NotFound)
        ));
    }
}

#[test]
#[ignore]
fn live_wrong_password_is_auth_error() {
    for mut cfg in servers() {
        cfg.password = "definitely-not-the-password".into();
        let host = cfg.host.clone();
        match SmbSource::connect(cfg) {
            Err(Error::Source(SourceError::Auth)) => println!("{host}: auth error ok"),
            Err(e) => panic!("{host}: expected Auth, got {e}"),
            Ok(_) => panic!("{host}: connected with a wrong password"),
        }
    }
}

#[test]
#[ignore]
fn live_scan_reads_tags_sample() {
    for cfg in servers() {
        let src = connect(&cfg);
        // Scan the folder two levels above a file at least three levels deep
        // (enough to pass 200 files) so the test does not walk the whole
        // collection. The walk is concurrent, so shallow files come first.
        let first = src
            .walk_limited(1, &mut |e| e.rel_path.matches('/').count() >= 2)
            .expect("walk failed")
            .pop()
            .expect("no audio file three levels deep");
        let parent = |p: &str| {
            p.rsplit_once('/')
                .map_or(String::new(), |(d, _)| d.to_string())
        };
        let dir = parent(&parent(&first.rel_path));
        let mut sub = cfg.clone();
        sub.root = if dir.is_empty() {
            cfg.root.clone()
        } else {
            format!("{}/{dir}", cfg.root)
        };
        let src = connect(&sub);

        let mut cat = Catalog::open_in_memory().unwrap();
        let cancel = AtomicBool::new(false);
        let report = scan(
            &src,
            &mut cat,
            &mut |p| {
                if p.seen >= 200 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .unwrap();
        println!("{}: {report:?}", cfg.host);
        assert!(report.read > 0, "{}: nothing read", cfg.host);
        assert!(report.failed < report.read, "{}: {report:?}", cfg.host);
    }
}

/// Failover only works if both servers use the same relative paths: take 20
/// files spread across the primary (every 25th audio file) and open each one,
/// read-only, on the secondary. Skipped when no secondary is configured.
#[test]
#[ignore]
fn live_sample_paths_exist_on_both_servers() {
    let servers = servers();
    let [primary, secondary] = &servers[..] else {
        println!("no SMB_TEST2_HOST configured, skipping");
        return;
    };
    let (p, s) = (connect(primary), connect(secondary));
    let mut n = 0usize;
    let sample = p
        .walk_limited(20, &mut |_| {
            n += 1;
            n % 25 == 1
        })
        .unwrap();
    assert!(!sample.is_empty(), "{}: no audio files", primary.host);
    let missing: Vec<_> = sample
        .iter()
        .filter(|e| match s.open(&e.rel_path) {
            Ok(_) => false,
            Err(err) => {
                println!("{}: {} -> {err}", secondary.host, e.rel_path);
                true
            }
        })
        .map(|e| e.rel_path.as_str())
        .collect();
    println!(
        "{} of {} sampled paths exist on {}",
        sample.len() - missing.len(),
        sample.len(),
        secondary.host
    );
    assert!(missing.is_empty(), "missing on secondary: {missing:?}");
}

/// The first ~500 files of the primary share, served by a real SMB source but
/// listed from a fixed sample so the test never walks the whole collection.
struct Sample<'a> {
    src: &'a SmbSource,
    files: Vec<FileEntry>,
}

impl MusicSource for Sample<'_> {
    fn walk(&self, on_entry: &mut dyn FnMut(FileEntry)) -> Result<(), SourceError> {
        self.files.iter().cloned().for_each(on_entry);
        Ok(())
    }
    fn open(&self, rel_path: &str) -> Result<Box<dyn smp_core::source::ReadSeek>, SourceError> {
        self.src.open(rel_path)
    }
    fn describe(&self) -> String {
        self.src.describe()
    }
}

/// Times a first scan of ~500 files with one worker and with eight; the
/// parallel scan must not be slower. Prints only durations and counts.
#[test]
#[ignore]
fn live_scan_timing() {
    let cfg = &servers()[0];
    let src = connect(cfg);
    let started = Instant::now();
    let files = src.walk_limited(500, &mut |_| true).unwrap();
    println!(
        "{}: listed {} files in {:?} (concurrent walk)",
        cfg.host,
        files.len(),
        started.elapsed()
    );
    let sample = Sample { src: &src, files };
    let time = |workers: usize| -> Duration {
        let mut cat = Catalog::open_in_memory().unwrap();
        let started = Instant::now();
        let report = scan_with(
            &sample,
            &mut cat,
            &mut |_| {},
            &AtomicBool::new(false),
            &ScanOptions {
                workers,
                ..Default::default()
            },
        )
        .unwrap();
        let took = started.elapsed();
        println!("{}: workers={workers}: {took:?}, {report:?}", cfg.host);
        assert_eq!(report.read, sample.files.len());
        took
    };
    // Parallel first, so any server-side caching favours the sequential run.
    let parallel = time(8);
    let sequential = time(1);
    println!(
        "{}: speed-up {:.1}x",
        cfg.host,
        sequential.as_secs_f64() / parallel.as_secs_f64()
    );
    assert!(parallel <= sequential, "{parallel:?} > {sequential:?}");
}
