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

pub struct Decoder {
    format: Box<dyn FormatReader>,
    codec: Box<dyn CodecDecoder>,
    track_id: u32,
    downmix: Downmix,
    sample_buf: Option<SampleBuffer<f32>>,
    resampler: Option<FftFixedIn<f32>>,
    /// Frames still to drop from the resampler output (its start-up delay).
    skip_frames: usize,
    /// Source-rate stereo frames waiting for a full resampler chunk.
    pending: [Vec<f32>; 2],
    /// Output-rate interleaved stereo ready to hand out.
    ready: VecDeque<f32>,
    duration_ms: Option<u64>,
    got_samples: bool,
    /// Source frames fed in so far; bounds the flushed resampler output.
    src_frames: u64,
    /// Samples already handed out by `read`.
    emitted_samples: u64,
    out_rate: u32,
    src_rate: u32,
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
        let (resampler, skip_frames) = if src_rate != out_rate {
            let r = FftFixedIn::<f32>::new(src_rate as usize, out_rate as usize, RESAMPLE_CHUNK, 2, 2)
                .map_err(decode_err)?;
            let delay = r.output_delay();
            (Some(r), delay)
        } else {
            (None, 0)
        };
        Ok(Self {
            format,
            codec,
            track_id,
            downmix: Downmix::new(channels),
            sample_buf: None,
            resampler,
            skip_frames,
            pending: [Vec::new(), Vec::new()],
            ready: VecDeque::new(),
            duration_ms,
            got_samples: false,
            src_frames: 0,
            emitted_samples: 0,
            out_rate,
            src_rate,
            ended: false,
        })
    }

    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// Fills `out` with interleaved stereo samples. `Ok(0)` means end of stream;
    /// otherwise at least one frame is written (`out` is filled unless the stream ends).
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let want = out.len() & !1; // whole frames only
        let mut n = 0;
        while n < want {
            if let Some(s) = self.ready.pop_front() {
                out[n] = s;
                n += 1;
                self.emitted_samples += 1;
                continue;
            }
            if self.ended {
                break;
            }
            self.fill()?;
        }
        Ok(n)
    }

    /// Decodes one more packet (or finishes the stream), appending to `ready`.
    fn fill(&mut self) -> Result<()> {
        let packet = loop {
            match self.format.next_packet() {
                Ok(p) if p.track_id() == self.track_id => break p,
                Ok(_) => continue,
                Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return self.finish(),
                Err(SymError::ResetRequired) => return self.finish(),
                Err(SymError::IoError(e)) => return Err(Error::Io(e)),
                Err(e) => return Err(decode_err(e)),
            }
        };
        let decoded = match self.codec.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(msg)) => {
                log::debug!("skipping undecodable packet: {msg}");
                return Ok(());
            }
            Err(SymError::IoError(e)) => return Err(Error::Io(e)),
            Err(e) => return Err(decode_err(e)),
        };
        let frames = decoded.frames();
        if frames == 0 {
            return Ok(());
        }
        let spec = *decoded.spec();
        let buf = self
            .sample_buf
            .get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        if buf.capacity() < frames * spec.channels.count() {
            *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        }
        buf.copy_interleaved_ref(decoded);
        let nch = self.downmix.gains.len();
        let mut stereo = Vec::with_capacity(frames * 2);
        for frame in buf.samples().chunks_exact(nch) {
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for (s, (gl, gr)) in frame.iter().zip(&self.downmix.gains) {
                l += s * gl;
                r += s * gr;
            }
            stereo.push(l);
            stereo.push(r);
        }
        self.got_samples = true;
        self.src_frames += frames as u64;
        self.push_source_frames(&stereo)
    }

    fn finish(&mut self) -> Result<()> {
        self.ended = true;
        if !self.got_samples {
            return Err(Error::Decode("stream ended before any samples were decoded".into()));
        }
        if let Some(rs) = self.resampler.as_mut() {
            // Flush the remaining partial chunk, plus enough silence to drain the filter delay.
            let rest = [std::mem::take(&mut self.pending[0]), std::mem::take(&mut self.pending[1])];
            let mut chunks = vec![rest];
            chunks.push([vec![0.0; RESAMPLE_CHUNK], vec![0.0; RESAMPLE_CHUNK]]);
            for chunk in chunks {
                if chunk[0].is_empty() {
                    continue;
                }
                let out = rs.process_partial(Some(&chunk), None).map_err(decode_err)?;
                Self::queue(&mut self.ready, &mut self.skip_frames, &out);
            }
            // Drop the padding the flush added beyond the true resampled length.
            let expected = (self.src_frames * self.out_rate as u64).div_ceil(self.src_rate as u64);
            let emitted = self.emitted_samples / 2;
            let keep = expected.saturating_sub(emitted) as usize;
            self.ready.truncate(keep * 2);
        }
        Ok(())
    }

    fn push_source_frames(&mut self, stereo: &[f32]) -> Result<()> {
        let Some(rs) = self.resampler.as_mut() else {
            self.ready.extend(stereo.iter().copied());
            return Ok(());
        };
        for f in stereo.chunks_exact(2) {
            self.pending[0].push(f[0]);
            self.pending[1].push(f[1]);
        }
        while self.pending[0].len() >= rs.input_frames_next() {
            let need = rs.input_frames_next();
            let input = [self.pending[0][..need].to_vec(), self.pending[1][..need].to_vec()];
            self.pending[0].drain(..need);
            self.pending[1].drain(..need);
            let out = rs.process(&input, None).map_err(decode_err)?;
            Self::queue(&mut self.ready, &mut self.skip_frames, &out);
        }
        Ok(())
    }

    fn queue(ready: &mut VecDeque<f32>, skip: &mut usize, out: &[Vec<f32>]) {
        let frames = out[0].len();
        let drop = (*skip).min(frames);
        *skip -= drop;
        for i in drop..frames {
            ready.push_back(out[0][i]);
            ready.push_back(out[1][i]);
        }
    }
}

fn map_open_err(e: SymError) -> Error {
    match e {
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
            assert!(d.duration_ms().map_or(true, |ms| (ms as i64 - 1000).abs() < 100), "{name}");
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
        for f in s.chunks_exact(2) {
            assert_eq!(f[0], f[1]);
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
