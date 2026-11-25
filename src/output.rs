//! Звуковой выход: поток cpal с микшером rodio, который замечает поломку потока.

use crate::config::log;
use rodio::cpal::traits::{DeviceTrait, StreamTrait};
use rodio::cpal::{self, FromSample, SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig};
use rodio::dynamic_mixer::{self, DynamicMixer, DynamicMixerController};
use rodio::{Sink, StreamError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Output {
    _stream: cpal::Stream,
    mixer: Arc<DynamicMixerController<f32>>,
    sample_rate: u32,
    broken: Arc<AtomicBool>,
}

impl Output {
    /// Поток с настройками устройства по умолчанию, а если не открылся, с любыми поддерживаемыми.
    pub fn open(dev: &cpal::Device) -> Result<Output, StreamError> {
        let default = dev.default_output_config().map_err(StreamError::DefaultStreamConfigError)
            .and_then(|config| Output::open_with(dev, config));
        default.or_else(|e| {
            dev.supported_output_configs()
                .map_err(StreamError::SupportedStreamConfigsError)?
                .flat_map(fallback_formats)
                .find_map(|config| Output::open_with(dev, config).ok())
                .ok_or(e)
        })
    }

    pub fn open_with(dev: &cpal::Device, config: SupportedStreamConfig) -> Result<Output, StreamError> {
        let cfg = config.config();
        let sample_rate = cfg.sample_rate.0;
        let (mixer, out) = dynamic_mixer::mixer::<f32>(cfg.channels, sample_rate);
        let broken = Arc::new(AtomicBool::new(false));
        let stream = match config.sample_format() {
            SampleFormat::F32 => build::<f32>(dev, &cfg, out, &broken),
            SampleFormat::F64 => build::<f64>(dev, &cfg, out, &broken),
            SampleFormat::I8 => build::<i8>(dev, &cfg, out, &broken),
            SampleFormat::I16 => build::<i16>(dev, &cfg, out, &broken),
            SampleFormat::I32 => build::<i32>(dev, &cfg, out, &broken),
            SampleFormat::I64 => build::<i64>(dev, &cfg, out, &broken),
            SampleFormat::U8 => build::<u8>(dev, &cfg, out, &broken),
            SampleFormat::U16 => build::<u16>(dev, &cfg, out, &broken),
            SampleFormat::U32 => build::<u32>(dev, &cfg, out, &broken),
            SampleFormat::U64 => build::<u64>(dev, &cfg, out, &broken),
            _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
        }
        .map_err(StreamError::BuildStreamError)?;
        stream.play().map_err(StreamError::PlayStreamError)?;
        Ok(Output { _stream: stream, mixer, sample_rate, broken })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Новый Sink на этом выходе.
    pub fn sink(&self) -> Sink {
        let (sink, queue) = Sink::new_idle();
        self.mixer.add(queue);
        sink
    }

    /// Поток сообщил об ошибке и больше не надёжен.
    pub fn broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    dev: &cpal::Device,
    cfg: &StreamConfig,
    mut mixer: DynamicMixer<f32>,
    broken: &Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let broken = broken.clone();
    dev.build_output_stream::<T, _, _>(
        cfg,
        move |data, _| data.iter_mut().for_each(|d| *d = T::from_sample(mixer.next().unwrap_or(0.0))),
        // ALSA на POLLERR зовёт сюда без паузы, пока поток жив (#82): пишем только первую ошибку.
        move |e| {
            if !broken.swap(true, Ordering::Relaxed) {
                log(format!("аудиовыход: {e}"));
            }
        },
        None,
    )
}

/// Максимум, распространённые частоты и минимум, без повторов на границах.
fn fallback_formats(config: cpal::SupportedStreamConfigRange) -> impl Iterator<Item = SupportedStreamConfig> {
    let min = config.min_sample_rate();
    let max = config.max_sample_rate();
    let cd = cpal::SampleRate(44_100);
    let studio = cpal::SampleRate(48_000);
    [
        Some(max),
        (min < cd && cd < max).then_some(cd),
        (min < studio && studio < max).then_some(studio),
        (min != max).then_some(min),
    ].into_iter().flatten().map(move |rate| config.with_sample_rate(rate))
}

#[cfg(test)]
#[test]
fn output_rate_fallback_keeps_cd_rate_and_lower_bound() {
    let rates = |min, max| {
        let range = cpal::SupportedStreamConfigRange::new(2, cpal::SampleRate(min), cpal::SampleRate(max),
            cpal::SupportedBufferSize::Unknown, SampleFormat::F32);
        fallback_formats(range).map(|cfg| cfg.sample_rate().0).collect::<Vec<_>>()
    };
    assert_eq!(rates(32_000, 192_000), [192_000, 44_100, 48_000, 32_000]);
    assert_eq!(rates(44_100, 48_000), [48_000, 44_100]);
    assert_eq!(rates(48_000, 48_000), [48_000]);
}
