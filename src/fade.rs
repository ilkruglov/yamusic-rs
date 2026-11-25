//! Громкость трека плавно нарастает в начале и плавно уходит по команде: так треки
//! перетекают друг в друга при кроссфейде.

use rodio::source::SeekError;
use rodio::Source;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Как часто источник проверяет, не пора ли затухать.
const CHECK_EVERY: u32 = 512;

/// Пульт затухания: живёт у плеера, пока сам трек играет в звуковом потоке.
#[derive(Default)]
pub struct FadeOut {
    ms: AtomicU32,
}

impl FadeOut {
    /// Начать затухание: за `over` трек сойдёт на нет и закончится.
    pub fn start(&self, over: Duration) {
        self.ms.store(over.as_millis().clamp(1, u32::MAX as u128) as u32, Ordering::Relaxed);
    }
}

/// Источник, громкость которого нарастает в начале и уходит по команде [`FadeOut`].
pub struct Fade<S> {
    inner: S,
    out: Arc<FadeOut>,
    gain: f32,
    step: f32,
    fading_out: bool,
    count: u32,
}

impl<S: Source<Item = f32>> Fade<S> {
    /// `fade_in` — за сколько громкость дойдёт до полной; ноль — сразу полная.
    pub fn new(inner: S, fade_in: Duration, out: Arc<FadeOut>) -> Self {
        let step = step_for(&inner, fade_in);
        let gain = if step > 0.0 { 0.0 } else { 1.0 };
        Fade { inner, out, gain, step, fading_out: false, count: 0 }
    }
}

/// Шаг громкости на сэмпл, чтобы пройти от 0 до 1 за `over`.
fn step_for<S: Source<Item = f32>>(s: &S, over: Duration) -> f32 {
    let samples = over.as_secs_f32() * s.sample_rate() as f32 * s.channels().max(1) as f32;
    if samples >= 1.0 { 1.0 / samples } else { 0.0 }
}

impl<S: Source<Item = f32>> Iterator for Fade<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if !self.fading_out {
            self.count += 1;
            if self.count >= CHECK_EVERY {
                self.count = 0;
                let ms = self.out.ms.load(Ordering::Relaxed);
                if ms > 0 {
                    self.fading_out = true;
                    self.step = -step_for(&self.inner, Duration::from_millis(ms as u64));
                }
            }
        }
        if self.step != 0.0 {
            self.gain = (self.gain + self.step).clamp(0.0, 1.0);
            if self.fading_out && self.gain <= 0.0 {
                return None;
            }
            if !self.fading_out && self.gain >= 1.0 {
                self.step = 0.0;
            }
        }
        Some(self.inner.next()? * self.gain)
    }
}

impl<S: Source<Item = f32>> Source for Fade<S> {
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }

    fn channels(&self) -> u16 {
        self.inner.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)
    }
}
