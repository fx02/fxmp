//! Audio output: the `Sink` abstraction the player writes to, and its cpal implementation.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::{Error, Result};

/// Length of the ring between the engine thread and the device callback.
const RING_MS: usize = 200;

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
            for (out, lr) in data
                .chunks_exact_mut(channels)
                .zip(a.as_chunks::<2>().0.iter().chain(b.as_chunks::<2>().0))
            {
                if channels == 1 {
                    out[0] = T::from_sample((lr[0] + lr[1]) * 0.5);
                } else {
                    out[0] = T::from_sample(lr[0]);
                    out[1] = T::from_sample(lr[1]);
                    out[2..].fill(silence);
                }
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
