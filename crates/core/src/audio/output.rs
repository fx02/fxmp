//! Audio output: the `Sink` abstraction the player writes to, and its cpal implementation.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::{Error, Result};

/// Length of the ring between the engine thread and the device callback.
const RING_MS: usize = 200;

/// Spread stereo over all the speakers of a multichannel output (see [`upmix`]).
/// Channels [`upmix`] fills; any beyond (rare 9+ channel outputs) stay silent.
const MAX_CHANNELS: usize = 8;

static FILL_SPEAKERS: AtomicBool = AtomicBool::new(true);

/// Turns spreading stereo over all speakers on or off; applies at once.
pub fn set_fill_speakers(on: bool) {
    FILL_SPEAKERS.store(on, Ordering::Relaxed);
}

/// -3 dB: the copies on the centre and rear speakers, so the sum is not louder.
const SIDE_GAIN: f32 = 0.707;

/// One output frame of `out.len()` channels from a stereo frame. Mono gets the
/// average; front left/right get the stereo pair. With `fill`, the other speakers
/// of the usual Windows layouts get it too: quad (back L/R), 5.1 (centre, LFE,
/// back L/R) and 7.1 (as 5.1 plus side L/R); the centre gets the average and the
/// LFE stays silent (the driver routes bass to the subwoofer). Without `fill`, or
/// on other layouts, the extra channels are silent.
pub fn upmix(lr: [f32; 2], fill: bool, out: &mut [f32]) {
    let [l, r] = lr;
    match out.len() {
        0 => {}
        1 => out[0] = (l + r) * 0.5,
        n => {
            out[0] = l;
            out[1] = r;
            out[2..].fill(0.0);
            if !fill {
                return;
            }
            let (sl, sr, c) = (l * SIDE_GAIN, r * SIDE_GAIN, (l + r) * 0.5 * SIDE_GAIN);
            match n {
                4 => out[2..4].copy_from_slice(&[sl, sr]),
                6 => out[2..6].copy_from_slice(&[c, 0.0, sl, sr]),
                8 => out[2..8].copy_from_slice(&[c, 0.0, sl, sr, sl, sr]),
                _ => {}
            }
        }
    }
}

/// Result of handing samples to a sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkWrite {
    /// This many samples (whole stereo frames) were accepted; the rest did not fit.
    Ok(usize),
    /// The device failed or disappeared; the sink is unusable.
    DeviceLost,
}

/// Where the player's interleaved stereo output goes.
pub trait Sink: Send {
    fn sample_rate(&self) -> u32;
    /// Queues as much of `stereo` as fits without blocking.
    fn write(&mut self, stereo: &[f32]) -> SinkWrite;
}

/// Opens sinks by device name (`None` = system default). Returns the sink and the
/// actual device name.
pub trait SinkFactory: Send {
    fn open(&mut self, device: Option<&str>) -> Result<(Box<dyn Sink>, String)>;
}

/// Names of the output devices the system offers.
pub fn list_output_devices() -> Vec<String> {
    match cpal::default_host().output_devices() {
        Ok(devices) => devices.map(|d| d.to_string()).collect(),
        Err(e) => {
            log::warn!("cannot list output devices: {e}");
            Vec::new()
        }
    }
}

pub fn default_output_device() -> Option<String> {
    cpal::default_host()
        .default_output_device()
        .map(|d| d.to_string())
}

fn audio_err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("audio output: {e}"))
}

/// A cpal output stream fed through a lock-free ring of about 200 ms.
pub struct CpalSink {
    _stream: cpal::Stream,
    producer: rtrb::Producer<f32>,
    lost: Arc<AtomicBool>,
    underruns: Arc<AtomicU64>,
    rate: u32,
}

impl CpalSink {
    /// Opens `device` (or the default output) at its default configuration.
    pub fn open(device: Option<&str>) -> Result<(Self, String)> {
        let host = cpal::default_host();
        let dev = match device {
            Some(name) => host
                .output_devices()
                .map_err(audio_err)?
                .find(|d| d.to_string() == name)
                .ok_or_else(|| audio_err(format!("device not found: {name}")))?,
            None => host
                .default_output_device()
                .ok_or_else(|| audio_err("no default output device"))?,
        };
        let name = dev.to_string();
        let supported = dev.default_output_config().map_err(audio_err)?;
        let config = supported.config();
        if config.channels == 0 {
            return Err(audio_err(format!("{name} has no output channels")));
        }
        let rate = config.sample_rate;
        // Whole stereo frames only, so frame boundaries never straddle the ring's wrap.
        let cap = (rate as usize * RING_MS / 1000).max(1) * 2;
        let (producer, consumer) = rtrb::RingBuffer::new(cap);
        let lost = Arc::new(AtomicBool::new(false));
        let underruns = Arc::new(AtomicU64::new(0));
        let parts = Parts {
            consumer,
            lost: lost.clone(),
            underruns: underruns.clone(),
        };
        let stream = match supported.sample_format() {
            SampleFormat::F32 => build::<f32>(&dev, config, parts),
            SampleFormat::F64 => build::<f64>(&dev, config, parts),
            SampleFormat::I16 => build::<i16>(&dev, config, parts),
            SampleFormat::I32 => build::<i32>(&dev, config, parts),
            SampleFormat::U16 => build::<u16>(&dev, config, parts),
            SampleFormat::I8 => build::<i8>(&dev, config, parts),
            SampleFormat::U8 => build::<u8>(&dev, config, parts),
            other => return Err(audio_err(format!("unsupported sample format {other:?}"))),
        }?;
        stream.play().map_err(audio_err)?;
        log::info!("audio output: {name}, {rate} Hz, {} ch", config.channels);
        Ok((
            Self {
                _stream: stream,
                producer,
                lost,
                underruns,
                rate,
            },
            name,
        ))
    }

    /// Number of device callbacks that found too little data (includes pauses).
    pub fn underrun_counter(&self) -> Arc<AtomicU64> {
        self.underruns.clone()
    }
}

struct Parts {
    consumer: rtrb::Consumer<f32>,
    lost: Arc<AtomicBool>,
    underruns: Arc<AtomicU64>,
}

fn build<T>(dev: &cpal::Device, config: StreamConfig, parts: Parts) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let Parts {
        mut consumer,
        lost,
        underruns,
    } = parts;
    let channels = config.channels as usize;
    let silence = T::EQUILIBRIUM;
    let data_cb = move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        // Real-time context: no allocation, no locks.
        let frames = data.len() / channels;
        let n = frames.min(consumer.slots() / 2);
        let mut written = 0;
        if let Ok(chunk) = consumer.read_chunk(n * 2) {
            let (a, b) = chunk.as_slices();
            let fill = FILL_SPEAKERS.load(Ordering::Relaxed);
            let mut frame = [0.0f32; MAX_CHANNELS];
            let frame = &mut frame[..channels.min(MAX_CHANNELS)];
            for (out, lr) in data
                .chunks_exact_mut(channels)
                .zip(a.as_chunks::<2>().0.iter().chain(b.as_chunks::<2>().0))
            {
                upmix(*lr, fill, frame);
                for (o, s) in out.iter_mut().zip(frame.iter()) {
                    *o = T::from_sample(*s);
                }
                out[frame.len()..].fill(silence);
                written += 1;
            }
            chunk.commit_all();
        }
        data[written * channels..].fill(silence);
        if written < frames {
            underruns.fetch_add(1, Ordering::Relaxed);
        }
    };
    let err_cb = move |e: cpal::Error| match e.kind() {
        // Overloads are glitches, and a default-device switch is followed automatically.
        ErrorKind::Xrun | ErrorKind::DeviceChanged => log::debug!("audio output: {e}"),
        _ => {
            log::warn!("audio output failed: {e}");
            lost.store(true, Ordering::Relaxed);
        }
    };
    dev.build_output_stream::<T, _, _>(config, data_cb, err_cb, None)
        .map_err(audio_err)
}

impl Sink for CpalSink {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn write(&mut self, stereo: &[f32]) -> SinkWrite {
        if self.lost.load(Ordering::Relaxed) {
            return SinkWrite::DeviceLost;
        }
        let n = stereo.len().min(self.producer.slots()) & !1;
        let (pushed, _) = self.producer.push_partial_slice(&stereo[..n]);
        SinkWrite::Ok(pushed.len())
    }
}

/// Opens [`CpalSink`]s.
#[derive(Debug, Default)]
pub struct CpalSinkFactory;

impl SinkFactory for CpalSinkFactory {
    fn open(&mut self, device: Option<&str>) -> Result<(Box<dyn Sink>, String)> {
        let (sink, name) = CpalSink::open(device)?;
        Ok((Box::new(sink), name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: usize, fill: bool) -> Vec<f32> {
        let mut out = vec![9.0; n];
        upmix([0.8, 0.4], fill, &mut out);
        out
    }

    fn near(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn stereo_and_mono_are_unchanged() {
        assert_eq!(frame(2, true), [0.8, 0.4]);
        assert!(near(&frame(1, true), &[0.6]));
    }

    #[test]
    fn five_one_fills_centre_and_rears_but_not_the_lfe() {
        let c = 0.6 * SIDE_GAIN;
        let (l, r) = (0.8 * SIDE_GAIN, 0.4 * SIDE_GAIN);
        assert!(near(&frame(6, true), &[0.8, 0.4, c, 0.0, l, r]));
        assert!(near(&frame(8, true), &[0.8, 0.4, c, 0.0, l, r, l, r]));
        assert!(near(&frame(4, true), &[0.8, 0.4, l, r]));
    }

    #[test]
    fn without_fill_only_the_front_pair_plays() {
        assert_eq!(frame(6, false), [0.8, 0.4, 0.0, 0.0, 0.0, 0.0]);
        // An unknown layout is never guessed at.
        assert_eq!(frame(3, true), [0.8, 0.4, 0.0]);
    }
}
