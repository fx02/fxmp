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

use smp_core::catalog::Catalog;
use smp_core::scan::scan;
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
        // Scan the folder two levels above a sample file (enough to pass 200
        // files) so the test does not walk the whole collection.
        let first = sample_file(&src, 1);
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
