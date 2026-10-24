//! Symphonia-based decoder producing interleaved stereo `f32` at a fixed output rate.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Mutex;

use rubato::{FftFixedIn, Resampler};
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{Decoder as CodecDecoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::source::ReadSeek;
use crate::{Error, Result};

const MAX_BAD_READS: u32 = 100;
const RESAMPLE_CHUNK: usize = 1024;
const CENTER_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Adapts a `ReadSeek` to symphonia's `MediaSource` (which requires `Sync`).
struct Source {
    inner: Mutex<Box<dyn ReadSeek>>,
    len: Option<u64>,
}

impl Source {
    fn inner(&mut self) -> &mut Box<dyn ReadSeek> {
        self.inner.get_mut().unwrap_or_else(|e| e.into_inner())
    }
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner().read(buf)
    }
}

impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner().seek(pos)
    }
}

impl MediaSource for Source {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

/// How source channels map onto the stereo output.
struct Downmix {
    /// Per source channel: (gain to left, gain to right).
    gains: Vec<(f32, f32)>,
}

impl Downmix {
    fn new(channels: Channels) -> Self {
        let n = channels.count();
        if n == 1 {
            return Self { gains: vec![(1.0, 1.0)] };
        }
        let flags: Vec<Channels> = channels.iter().collect();
        let has = |c: Channels| flags.contains(&c);
        let has_front_pair = has(Channels::FRONT_LEFT) && has(Channels::FRONT_RIGHT);
        if n == 2 || !has_front_pair {
            // Plain stereo (or an unlabelled layout): first two channels pass through.
            let gains = (0..n)
                .map(|i| match i {
                    0 => (1.0, 0.0),
                    1 => (0.0, 1.0),
                    _ => (0.0, 0.0),
                })
                .collect();
            return Self { gains };
        }
        let left_side = Channels::SIDE_LEFT | Channels::REAR_LEFT;
        let right_side = Channels::SIDE_RIGHT | Channels::REAR_RIGHT;
        let center = has(Channels::FRONT_CENTRE);
        let surround = flags.iter().any(|c| left_side.contains(*c));
        // Normalise by the largest possible sum so the downmix cannot clip.
        let norm = 1.0 + if center { CENTER_GAIN } else { 0.0 } + if surround { CENTER_GAIN } else { 0.0 };
        let gains = flags
            .iter()
            .map(|&c| {
                let (l, r) = if c == Channels::FRONT_LEFT {
                    (1.0, 0.0)
                } else if c == Channels::FRONT_RIGHT {
                    (0.0, 1.0)
                } else if c == Channels::FRONT_CENTRE {
                    (CENTER_GAIN, CENTER_GAIN)
                } else if left_side.contains(c) {
                    (CENTER_GAIN, 0.0)
                } else if right_side.contains(c) {
                    (0.0, CENTER_GAIN)
                } else {
                    (0.0, 0.0) // LFE and exotic channels are dropped
                };
                (l / norm, r / norm)
            })
            .collect();
        Self { gains }
    }
}

/// Rate conversion state for one run of constant source rate.
struct Resample {
    rs: FftFixedIn<f32>,
    /// Source-rate stereo frames waiting for a full resampler chunk (planar).
    pending: [Vec<f32>; 2],
    /// Start of the unconsumed part of `pending`.
    offset: usize,
    /// Allocated once; sized for the resampler's maximum output.
    out: Vec<Vec<f32>>,
}

impl Resample {
    fn new(src_rate: u32, out_rate: u32) -> Result<Self> {
        let rs = FftFixedIn::<f32>::new(src_rate as usize, out_rate as usize, RESAMPLE_CHUNK, 2, 2)
            .map_err(decode_err)?;
        let out = rs.output_buffer_allocate(true);
        Ok(Self { rs, pending: [Vec::new(), Vec::new()], offset: 0, out })
    }
}

pub struct Decoder {
    format: Box<dyn FormatReader>,
    codec: Box<dyn CodecDecoder>,
    track_id: u32,
    downmix: Downmix,
    sample_buf: Option<SampleBuffer<f32>>,
    /// Reused interleaved-stereo scratch for one decoded packet.
    stereo: Vec<f32>,
    resample: Option<Resample>,
    /// Frames still to drop from the resampler output (its start-up delay).
    skip_frames: usize,
    /// Output-rate interleaved stereo ready to hand out.
    ready: VecDeque<f32>,
    duration_ms: Option<u64>,
    got_samples: bool,
    /// Current source layout, to detect mid-stream changes.
    cur_rate: u32,
    cur_channels: Channels,
    /// Source frames fed in / output frames queued since the last (re)build of `resample`;
    /// they bound the flushed resampler output.
    seg_src_frames: u64,
    seg_out_frames: u64,
    out_rate: u32,
    ended: bool,
}

fn decode_err(e: impl std::fmt::Display) -> Error {
    Error::Decode(e.to_string())
}

impl Decoder {
    pub fn open(reader: Box<dyn ReadSeek>, len: Option<u64>, ext: &str, out_rate: u32) -> Result<Self> {
        let src = Source { inner: Mutex::new(reader), len };
        let mss = MediaSourceStream::new(Box::new(src), Default::default());
        let mut hint = Hint::new();
        if !ext.is_empty() {
            hint.with_extension(ext.trim_start_matches('.'));
        }
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions { enable_gapless: true, ..Default::default() }, &MetadataOptions::default())
            .map_err(map_open_err)?;
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| Error::Decode("no decodable audio track".into()))?;
        let params = track.codec_params.clone();
        let track_id = track.id;
        let codec = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(decode_err)?;
        let src_rate = params.sample_rate.ok_or_else(|| Error::Decode("unknown sample rate".into()))?;
        let channels = params.channels.ok_or_else(|| Error::Decode("unknown channel layout".into()))?;
        let duration_ms = match (params.n_frames, params.sample_rate) {
            (Some(n), Some(r)) if r > 0 => Some(n * 1000 / r as u64),
            _ => None,
        };
        let (resample, skip_frames) = Self::make_resample(src_rate, out_rate)?;
        Ok(Self {
            format,
            codec,
            track_id,
            downmix: Downmix::new(channels),
            sample_buf: None,
            stereo: Vec::new(),
            resample,
            skip_frames,
            ready: VecDeque::new(),
            duration_ms,
            got_samples: false,
            cur_rate: src_rate,
            cur_channels: channels,
            seg_src_frames: 0,
            seg_out_frames: 0,
            out_rate,
            ended: false,
        })
    }

    fn make_resample(src_rate: u32, out_rate: u32) -> Result<(Option<Resample>, usize)> {
        if src_rate == out_rate {
            return Ok((None, 0));
        }
        let r = Resample::new(src_rate, out_rate)?;
        let delay = r.rs.output_delay();
        Ok((Some(r), delay))
    }

    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// Fills `out` with interleaved stereo samples and returns how many it wrote.
    /// `out` must hold at least one frame (2 samples); an odd trailing sample is left
    /// untouched. `Ok(0)` means end of stream; otherwise `out` is filled unless the
    /// stream ends first.
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        debug_assert!(out.len() >= 2, "read needs room for at least one stereo frame");
        let want = out.len() & !1; // whole frames only
        let mut n = 0;
        while n < want {
            if self.ready.is_empty() {
                if self.ended {
                    break;
                }
                self.fill()?;
                continue;
            }
            let take = (want - n).min(self.ready.len());
            let (a, b) = self.ready.as_slices();
            let from_a = take.min(a.len());
            out[n..n + from_a].copy_from_slice(&a[..from_a]);
            out[n + from_a..n + take].copy_from_slice(&b[..take - from_a]);
            self.ready.drain(..take);
            n += take;
        }
        Ok(n)
    }

    /// Decodes one more packet (or finishes the stream), appending to `ready`.
    fn fill(&mut self) -> Result<()> {
        let mut bad_reads = 0;
        let packet = loop {
            match self.format.next_packet() {
                Ok(p) if p.track_id() == self.track_id => break p,
                Ok(_) => continue,
                // Container-level damage (e.g. an Ogg page CRC mismatch): resync on the next
                // packet, but give up if the reader makes no progress.
                Err(SymError::DecodeError(msg)) if bad_reads < MAX_BAD_READS => {
                    bad_reads += 1;
                    log::debug!("skipping damaged container data: {msg}");
                    continue;
                }
                Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return self.finish(),
                Err(SymError::ResetRequired) => return self.finish(),
                Err(SymError::IoError(e)) => return Err(Error::Io(e)),
                Err(e) => return Err(decode_err(e)),
            }
        };
        let decoded = match self.codec.decode(&packet) {
            Ok(d) => d,
            // A codec-level failure means this packet is corrupt, not that the source failed
            // (reader errors surface from `next_packet`).
            Err(SymError::DecodeError(msg)) => {
                log::debug!("skipping undecodable packet: {msg}");
                return Ok(());
            }
            Err(SymError::IoError(e)) => {
                log::debug!("skipping corrupt packet: {e}");
                return Ok(());
            }
            Err(e) => return Err(decode_err(e)),
        };
        let frames = decoded.frames();
        if frames == 0 {
            return Ok(());
        }
        let spec = *decoded.spec();
        let nch = spec.channels.count();
        let buf = self
            .sample_buf
            .get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        if buf.capacity() < frames * nch {
            *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        }
        buf.copy_interleaved_ref(decoded);
        // `decoded` borrows the codec; it is no longer needed past this point.
        if spec.rate != self.cur_rate || spec.channels != self.cur_channels {
            self.rebuild_for(spec.rate, spec.channels)?;
        }
        let buf = self.sample_buf.as_ref().expect("sample buffer was just filled");
        self.stereo.clear();
        for frame in buf.samples().chunks_exact(nch) {
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for (s, (gl, gr)) in frame.iter().zip(&self.downmix.gains) {
                l += s * gl;
                r += s * gr;
            }
            self.stereo.push(l);
            self.stereo.push(r);
        }
        self.got_samples = true;
        self.seg_src_frames += frames as u64;
        self.push_source_frames()
    }

    /// The stream changed sample rate or channel layout: drain the old resampler and
    /// continue with a downmix and resampler for the new layout.
    fn rebuild_for(&mut self, rate: u32, channels: Channels) -> Result<()> {
        log::debug!("stream spec changed to {rate} Hz, {} ch", channels.count());
        self.flush_resampler()?;
        self.downmix = Downmix::new(channels);
        let (resample, skip) = Self::make_resample(rate, self.out_rate)?;
        self.resample = resample;
        self.skip_frames = skip;
        self.cur_rate = rate;
        self.cur_channels = channels;
        self.seg_src_frames = 0;
        self.seg_out_frames = 0;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.ended = true;
        if !self.got_samples {
            return Err(Error::Decode("stream ended before any samples were decoded".into()));
        }
        self.flush_resampler()
    }

    /// Emits the resampler's tail (partial chunk plus filter delay), limited to the
    /// true resampled length of the current segment.
    fn flush_resampler(&mut self) -> Result<()> {
        let Some(r) = self.resample.take() else {
            return Ok(());
        };
        let Resample { mut rs, pending, offset, mut out } = r;
        let expected = (self.seg_src_frames * self.out_rate as u64).div_ceil(self.cur_rate as u64);
        let mut budget = expected.saturating_sub(self.seg_out_frames) as usize;
        let rest = [&pending[0][offset..], &pending[1][offset..]];
        if !rest[0].is_empty() {
            let (_, n) = rs.process_partial_into_buffer(Some(&rest), &mut out, None).map_err(decode_err)?;
            Self::queue(&mut self.ready, &mut self.skip_frames, &out, n, Some(&mut budget));
        }
        let (_, n) = rs.process_partial_into_buffer(None::<&[&[f32]]>, &mut out, None).map_err(decode_err)?;
        Self::queue(&mut self.ready, &mut self.skip_frames, &out, n, Some(&mut budget));
        Ok(())
    }

    fn push_source_frames(&mut self) -> Result<()> {
        let Some(r) = self.resample.as_mut() else {
            self.ready.extend(self.stereo.iter().copied());
            return Ok(());
        };
        for f in self.stereo.as_chunks::<2>().0 {
            r.pending[0].push(f[0]);
            r.pending[1].push(f[1]);
        }
        loop {
            let need = r.rs.input_frames_next();
            if r.pending[0].len() - r.offset < need {
                break;
            }
            let input = [&r.pending[0][r.offset..r.offset + need], &r.pending[1][r.offset..r.offset + need]];
            let (_, n) = r.rs.process_into_buffer(&input, &mut r.out, None).map_err(decode_err)?;
            r.offset += need;
            let before = self.ready.len();
            Self::queue(&mut self.ready, &mut self.skip_frames, &r.out, n, None);
            self.seg_out_frames += ((self.ready.len() - before) / 2) as u64;
        }
        // Compact once per packet; what remains is less than one chunk.
        if r.offset > 0 {
            for ch in &mut r.pending {
                ch.drain(..r.offset);
            }
            r.offset = 0;
        }
        Ok(())
    }

    /// Appends the first `frames` frames of planar `out` to `ready`, after dropping
    /// `skip` start-up frames and (when given) at most `budget` frames.
    fn queue(ready: &mut VecDeque<f32>, skip: &mut usize, out: &[Vec<f32>], frames: usize, budget: Option<&mut usize>) {
        let drop = (*skip).min(frames);
        *skip -= drop;
        let mut end = frames;
        if let Some(b) = budget {
            end = end.min(drop + *b);
            *b -= end - drop;
        }
        for (l, r) in out[0][drop..end].iter().zip(&out[1][drop..end]) {
            ready.push_back(*l);
            ready.push_back(*r);
        }
    }
}

fn map_open_err(e: SymError) -> Error {
    match e {
        // A file that ends while probing is truncated or tiny: a bad file, not a source fault.
        SymError::IoError(io) if io.kind() == io::ErrorKind::UnexpectedEof => decode_err(io),
        SymError::IoError(io) => Error::Io(io),
        other => decode_err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::path::PathBuf;

    fn path(dir: &str, name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join(dir).join(name)
    }

    fn open(dir: &str, name: &str, rate: u32) -> Result<Decoder> {
        let p = path(dir, name);
        let f = File::open(&p).unwrap();
        let len = f.metadata().unwrap().len();
        let ext = p.extension().unwrap().to_str().unwrap().to_string();
        Decoder::open(Box::new(f), Some(len), &ext, rate)
    }

    fn drain(d: &mut Decoder) -> Vec<f32> {
        let mut all = Vec::new();
        let mut buf = vec![0.0f32; 4096];
        loop {
            let n = d.read(&mut buf).unwrap();
            if n == 0 {
                return all;
            }
            all.extend_from_slice(&buf[..n]);
        }
    }

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
    }

    #[test]
    fn decodes_mp3_flac_ogg() {
        for name in ["tone.mp3", "tone.flac", "tone.ogg"] {
            let mut d = open("fixtures", name, 48000).unwrap();
            let s = drain(&mut d);
            let frames = s.len() / 2;
            assert!((frames as i64 - 48000).abs() <= 2400, "{name}: {frames} frames");
            assert!(rms(&s) > 0.01, "{name}: rms {}", rms(&s));
            match (name, d.duration_ms()) {
                ("tone.mp3", None) => {} // no frame count without a Xing/Info header
                (_, Some(ms)) => assert!((ms as i64 - 1000).abs() < 100, "{name}: {ms} ms"),
                (_, None) => panic!("{name}: duration should be known"),
            }
        }
    }

    #[test]
    fn native_rate_is_not_resampled() {
        let mut d = open("fixtures", "tone.flac", 44100).unwrap();
        assert_eq!(drain(&mut d).len() / 2, 44100);
        assert_eq!(d.duration_ms(), Some(1000));
    }

    #[test]
    fn broken_file_open_fails() {
        assert!(open("fixtures", "broken.mp3", 48000).is_err());
    }

    #[test]
    fn mono_is_duplicated() {
        let mut d = open("fixtures-audio", "mono.flac", 44100).unwrap();
        let s = drain(&mut d);
        assert!(rms(&s) > 0.01);
        for f in s.as_chunks::<2>().0 {
            assert_eq!(f[0], f[1]);
        }
    }

    #[test]
    fn mono_resampled_is_duplicated() {
        let mut d = open("fixtures-audio", "mono.flac", 48000).unwrap();
        let s = drain(&mut d);
        assert!((s.len() as i64 / 2 - 48000).abs() <= 2400);
        assert!(rms(&s) > 0.01);
        assert!(s.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]));
    }

    #[test]
    fn damaged_packet_mid_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["tone.mp3", "tone.flac", "tone.ogg"] {
            let mut data = std::fs::read(path("fixtures", name)).unwrap();
            let mid = data.len() / 2;
            for b in &mut data[mid..mid + 64] {
                *b ^= 0xA5;
            }
            let p = dir.path().join(name);
            std::fs::write(&p, &data).unwrap();
            let len = data.len() as u64;
            let ext = p.extension().unwrap().to_str().unwrap();
            let opened = Decoder::open(Box::new(File::open(&p).unwrap()), Some(len), ext, 44100);
            let mut d = match opened {
                Ok(d) => d,
                // Symphonia's Ogg reader validates page CRCs while opening, so damage to a
                // small file is reported as a bad file there; it must not be an Io error.
                Err(Error::Decode(_)) if name == "tone.ogg" => continue,
                Err(e) => panic!("{name}: unexpected open error {e:?}"),
            };
            let mut frames = 0;
            let mut buf = vec![0.0f32; 4096];
            loop {
                match d.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => frames += n / 2,
                    Err(e) => panic!("{name}: damaged packet should be skipped, got {e:?}"),
                }
            }
            assert!(frames >= 35280, "{name}: only {frames} frames survived");
        }
    }

    #[test]
    fn truncated_file_is_decode_error_not_io() {
        let data = std::fs::read(path("fixtures", "tone.flac")).unwrap();
        let cut = std::io::Cursor::new(data[..10].to_vec());
        match Decoder::open(Box::new(cut), Some(10), "flac", 44100) {
            Err(Error::Decode(_)) => {}
            Err(other) => panic!("expected Decode, got {other:?}"),
            Ok(_) => panic!("tiny file must not open"),
        }
    }

    #[test]
    fn read_error_mid_stream_is_surfaced() {
        struct Failing(std::io::Cursor<Vec<u8>>, usize);
        impl Read for Failing {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.0.position() as usize >= self.1 {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "stalled"));
                }
                let max = (self.1 - self.0.position() as usize).min(b.len());
                self.0.read(&mut b[..max])
            }
        }
        impl Seek for Failing {
            fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
                self.0.seek(p)
            }
        }
        let data = std::fs::read(path("fixtures", "tone.flac")).unwrap();
        let len = data.len() as u64;
        let r = Failing(std::io::Cursor::new(data), 12_000);
        let mut d = Decoder::open(Box::new(r), Some(len), "flac", 44100).unwrap();
        let mut buf = vec![0.0f32; 4096];
        let err = loop {
            match d.read(&mut buf) {
                Ok(0) => panic!("ended without error"),
                Ok(_) => {}
                Err(e) => break e,
            }
        };
        match err {
            Error::Io(e) => assert_eq!(e.kind(), io::ErrorKind::TimedOut),
            other => panic!("expected Io(TimedOut), got {other:?}"),
        }
    }

    #[test]
    fn downmix_normalised_for_surround() {
        let l51 = Channels::FRONT_LEFT
            | Channels::FRONT_RIGHT
            | Channels::FRONT_CENTRE
            | Channels::LFE1
            | Channels::SIDE_LEFT
            | Channels::SIDE_RIGHT;
        let dm = Downmix::new(l51);
        let (sl, sr) = dm.gains.iter().fold((0.0, 0.0), |a, g| (a.0 + g.0, a.1 + g.1));
        assert!(sl <= 1.0001 && sr <= 1.0001, "{sl} {sr}");
        assert!(sl > 0.99, "full-scale input should reach about full scale: {sl}");
    }
}
