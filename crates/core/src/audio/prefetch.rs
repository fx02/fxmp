use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::source::ReadSeek;

#[derive(Debug, Clone)]
pub struct PrefetchConfig {
    /// Size of one read issued to the inner reader.
    pub chunk: usize,
    /// How many bytes beyond the read position the background thread fetches.
    pub ahead: usize,
    /// How long `read` waits for data before failing with `TimedOut`.
    pub read_timeout: Duration,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            chunk: 256 * 1024,
            ahead: 8 * 1024 * 1024,
            read_timeout: Duration::from_secs(10),
        }
    }
}

struct State {
    /// Bumped on every out-of-buffer seek; results of stale fetches are dropped.
    generation: u64,
    /// Buffered bytes, starting at absolute offset `start`.
    buf: VecDeque<u8>,
    start: u64,
    /// Reader position.
    pos: u64,
    /// Inner reached its end before `len` (data ends at the end of `buf`).
    eof: bool,
    /// Inner failed at the end of `buf`; surfaces once the reader gets there.
    error: Option<(io::ErrorKind, String)>,
    shutdown: bool,
}

impl State {
    fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }
}

struct Shared {
    state: Mutex<State>,
    cond: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Read-ahead wrapper: a background thread keeps up to `ahead` bytes buffered
/// past the read position so slow ranged reads do not stall decoding.
pub struct PrefetchReader {
    shared: Arc<Shared>,
    len: u64,
    cfg: PrefetchConfig,
}

impl PrefetchReader {
    pub fn new(inner: Box<dyn ReadSeek>, len: u64, cfg: PrefetchConfig) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                generation: 0,
                buf: VecDeque::new(),
                start: 0,
                pos: 0,
                eof: false,
                error: None,
                shutdown: false,
            }),
            cond: Condvar::new(),
        });
        let (s, c) = (shared.clone(), cfg.clone());
        std::thread::Builder::new()
            .name("prefetch".into())
            .spawn(move || fetch_loop(s, inner, len, c))
            .expect("spawn prefetch thread");
        Self { shared, len, cfg }
    }
}

fn fetch_loop(shared: Arc<Shared>, mut inner: Box<dyn ReadSeek>, len: u64, cfg: PrefetchConfig) {
    let chunk = cfg.chunk.max(1);
    let mut scratch = vec![0u8; chunk];
    // Where the inner reader currently is; None when unknown (after an error).
    let mut inner_pos: Option<u64> = Some(0);
    loop {
        let (generation, offset) = {
            let mut st = shared.lock();
            loop {
                if st.shutdown {
                    return;
                }
                let end = st.end();
                let needs = st.error.is_none()
                    && !st.eof
                    && end < len
                    && end.saturating_sub(st.pos) < cfg.ahead as u64;
                if needs {
                    break (st.generation, end);
                }
                st = shared.cond.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let want = chunk.min((len - offset) as usize);
        let result = (|| {
            if inner_pos != Some(offset) {
                inner_pos = None;
                inner.seek(SeekFrom::Start(offset))?;
            }
            let n = inner.read(&mut scratch[..want])?;
            inner_pos = Some(offset + n as u64);
            Ok::<usize, io::Error>(n)
        })();
        let mut st = shared.lock();
        if st.shutdown {
            return;
        }
        if st.generation != generation {
            continue;
        }
        match result {
            Ok(0) => st.eof = true,
            Ok(n) => st.buf.extend(&scratch[..n]),
            Err(e) => st.error = Some((e.kind(), e.to_string())),
        }
        shared.cond.notify_all();
    }
}

impl Read for PrefetchReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + self.cfg.read_timeout;
        let mut st = self.shared.lock();
        loop {
            if st.pos >= self.len {
                return Ok(0);
            }
            if st.pos < st.end() {
                let skip = (st.pos - st.start) as usize;
                let n = out.len().min(st.buf.len() - skip);
                let (a, b) = st.buf.as_slices();
                let mut copied = 0;
                if skip < a.len() {
                    let m = n.min(a.len() - skip);
                    out[..m].copy_from_slice(&a[skip..skip + m]);
                    copied = m;
                }
                if copied < n {
                    let from = skip + copied - a.len();
                    out[copied..n].copy_from_slice(&b[from..from + (n - copied)]);
                }
                st.pos += n as u64;
                // Keep some history for cheap backward seeks, drop the rest.
                let history = self.cfg.ahead as u64;
                if st.pos - st.start > history {
                    let drop_n = (st.pos - st.start - history) as usize;
                    st.buf.drain(..drop_n);
                    st.start += drop_n as u64;
                }
                self.shared.cond.notify_all();
                return Ok(n);
            }
            if let Some((kind, msg)) = &st.error {
                return Err(io::Error::new(*kind, msg.clone()));
            }
            if st.eof {
                return Ok(0);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "prefetch read timed out",
                ));
            }
            st = self
                .shared
                .cond
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Seek for PrefetchReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut st = self.shared.lock();
        let target = match from {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => st.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )
        })?;
        if target < st.start || target > st.end() {
            st.generation += 1;
            st.buf.clear();
            st.start = target;
            st.eof = false;
            st.error = None;
        }
        st.pos = target;
        self.shared.cond.notify_all();
        Ok(target)
    }
}

impl Drop for PrefetchReader {
    fn drop(&mut self) {
        // Signal only: the thread may be stuck in a blocking inner read, so it
        // is detached rather than joined and exits once that read returns.
        self.shared.lock().shutdown = true;
        self.shared.cond.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngCore, SeedableRng};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    const SIZE: usize = 5 * 1024 * 1024;

    fn data() -> Vec<u8> {
        let mut v = vec![0u8; SIZE];
        rand::rngs::StdRng::seed_from_u64(7).fill_bytes(&mut v);
        v
    }

    enum Mode {
        Normal,
        Stall,
        FailAt(usize),
    }

    struct SlowReader {
        inner: Cursor<Vec<u8>>,
        delay: Duration,
        read_bytes: Arc<AtomicUsize>,
        mode: Mode,
    }

    impl SlowReader {
        fn new(d: Vec<u8>, delay: Duration, mode: Mode) -> (Self, Arc<AtomicUsize>) {
            let c = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    inner: Cursor::new(d),
                    delay,
                    read_bytes: c.clone(),
                    mode,
                },
                c,
            )
        }
    }

    impl Read for SlowReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.mode {
                Mode::Stall => loop {
                    std::thread::sleep(Duration::from_secs(3600));
                },
                Mode::FailAt(limit) => {
                    let pos = self.inner.position() as usize;
                    if pos >= limit {
                        return Err(io::Error::new(io::ErrorKind::ConnectionReset, "boom"));
                    }
                    let n = buf.len().min(limit - pos);
                    return self.inner.read(&mut buf[..n]);
                }
                Mode::Normal => {}
            }
            std::thread::sleep(self.delay);
            let n = self.inner.read(buf)?;
            self.read_bytes.fetch_add(n, Ordering::SeqCst);
            Ok(n)
        }
    }

    impl Seek for SlowReader {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.inner.seek(p)
        }
    }

    fn reader(delay_ms: u64) -> (PrefetchReader, Vec<u8>, Arc<AtomicUsize>) {
        let d = data();
        let (s, c) = SlowReader::new(d.clone(), Duration::from_millis(delay_ms), Mode::Normal);
        (
            PrefetchReader::new(Box::new(s), SIZE as u64, PrefetchConfig::default()),
            d,
            c,
        )
    }

    #[test]
    fn reads_match_source() {
        let (mut r, d, _) = reader(0);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert!(out == d);
    }

    #[test]
    fn seek_forward_backward_match_source() {
        let (mut r, d, _) = reader(0);
        let mut b = vec![0u8; 1000];
        for &off in &[
            0usize,
            4000,
            100,
            3_000_000,
            2_999_000,
            10,
            SIZE - 1000,
            500_000,
        ] {
            assert_eq!(r.seek(SeekFrom::Start(off as u64)).unwrap(), off as u64);
            r.read_exact(&mut b).unwrap();
            assert!(b[..] == d[off..off + 1000], "mismatch at {off}");
        }
        r.seek(SeekFrom::End(-500)).unwrap();
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert!(tail[..] == d[SIZE - 500..]);
        assert_eq!(
            r.seek(SeekFrom::Current(-100)).unwrap(),
            (SIZE - 100) as u64
        );
        r.read_exact(&mut b[..100]).unwrap();
        assert!(b[..100] == d[SIZE - 100..]);
        assert_eq!(r.read(&mut b).unwrap(), 0);
    }

    #[test]
    fn prefetches_ahead() {
        let (_r, _, c) = reader(2);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            c.load(Ordering::SeqCst) >= 1024 * 1024,
            "got {}",
            c.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn read_times_out_when_inner_stalls() {
        let (s, _) = SlowReader::new(data(), Duration::ZERO, Mode::Stall);
        let cfg = PrefetchConfig {
            read_timeout: Duration::from_millis(200),
            ..Default::default()
        };
        let mut r = PrefetchReader::new(Box::new(s), SIZE as u64, cfg);
        let t = Instant::now();
        let e = r.read(&mut [0u8; 16]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(t.elapsed() < Duration::from_millis(400));
    }

    #[test]
    fn inner_error_propagates() {
        let d = data();
        let (s, _) = SlowReader::new(d.clone(), Duration::ZERO, Mode::FailAt(300_000));
        let mut r = PrefetchReader::new(Box::new(s), SIZE as u64, PrefetchConfig::default());
        let mut out = Vec::new();
        let e = r.read_to_end(&mut out).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(out.len(), 300_000);
        assert!(out[..] == d[..300_000]);
    }
}
