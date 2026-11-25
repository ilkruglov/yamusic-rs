//! Звук декодируется заранее в своём потоке, а аудиопоток только забирает готовые сэмплы:
//! когда процессор занят, декодер и эквалайзер не успевали внутри колбэка, и звук хрипел (#47).

use rodio::source::SeekError;
use rodio::Source;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::Duration;

/// Сэмплов в одной порции.
const CHUNK: usize = 1024;
/// Сколько звука держать готовым. Больше — дольше доходят правки эквалайзера.
const AHEAD: Duration = Duration::from_millis(250);

struct Shared {
    /// Растёт при каждой перемотке: порции со старым номером выбрасываются.
    gen: AtomicU64,
    closed: AtomicBool,
}

pub struct Ahead<S> {
    resampler: Arc<Mutex<Resampler<S>>>,
    shared: Arc<Shared>,
    rx: Receiver<(u64, Vec<f32>)>,
    worker: Thread,
    chunk: Vec<f32>,
    at: usize,
    ended: bool,
    channels: u16,
    rate: u32,
    total: Option<Duration>,
}

impl<S: Source<Item = f32> + Send + 'static> Ahead<S> {
    pub fn new(inner: S, output_rate: u32) -> Self {
        let (channels, total) = (inner.channels(), inner.total_duration());
        let per_sec = output_rate as usize * channels.max(1) as usize;
        let (tx, rx) = sync_channel((per_sec * AHEAD.as_millis() as usize / 1000 / CHUNK).max(2));
        // Очередь rodio делит бесконечный источник на короткие кадры. Пересчитываем
        // частоту до очереди, чтобы микшер не сбрасывал фазу на каждой границе.
        let resampler = Arc::new(Mutex::new(Resampler::new(inner, output_rate)));
        let shared = Arc::new(Shared { gen: AtomicU64::new(0), closed: AtomicBool::new(false) });
        let (src, sh) = (resampler.clone(), shared.clone());
        let worker = thread::Builder::new()
            .name("decode".into())
            .spawn(move || loop {
                let (gen, chunk) = {
                    let mut resampler = src.lock().unwrap();
                    let samples = resampler.by_ref().take(CHUNK).collect::<Vec<f32>>();
                    (sh.gen.load(Ordering::SeqCst), samples)
                };
                let end = chunk.is_empty();
                if tx.send((gen, chunk)).is_err() {
                    return;
                }
                // Конец трека: ждём перемотку назад или закрытия.
                while end && sh.gen.load(Ordering::SeqCst) == gen {
                    if sh.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::park_timeout(Duration::from_millis(200));
                }
            })
            .expect("decode thread")
            .thread()
            .clone();
        Ahead { resampler, shared, rx, worker, chunk: Vec::new(), at: 0, ended: false, channels, rate: output_rate, total }
    }
}

impl<S> Drop for Ahead<S> {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.worker.unpark();
    }
}

impl<S> Iterator for Ahead<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.ended { return None; }
        loop {
            if let Some(&x) = self.chunk.get(self.at) {
                self.at += 1;
                return Some(x);
            }
            let (gen, chunk) = self.rx.recv().ok()?;
            if gen != self.shared.gen.load(Ordering::SeqCst) {
                continue;
            }
            if chunk.is_empty() {
                self.ended = true;
                return None;
            }
            self.chunk = chunk;
            self.at = 0;
        }
    }
}

impl<S: Source<Item = f32>> Source for Ahead<S> {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let res = {
            let mut resampler = self.resampler.lock().unwrap();
            let res = resampler.seek(pos);
            self.shared.gen.fetch_add(1, Ordering::SeqCst);
            res
        };
        self.chunk.clear();
        self.at = 0;
        self.ended = false;
        self.worker.unpark();
        res
    }
}

/// Линейный пересчёт без сброса фазы между порциями. Фаза хранится целым числом,
/// чтобы округление не накапливалось на длинных треках.
struct Resampler<S> {
    source: S,
    input_rate: u64,
    output_rate: u64,
    channels: usize,
    left: Vec<f32>,
    right: Vec<f32>,
    channel: usize,
    phase: u64,
}

impl<S: Source<Item = f32>> Resampler<S> {
    fn new(source: S, output_rate: u32) -> Self {
        let channels = usize::from(source.channels());
        let input_rate = u64::from(source.sample_rate());
        Self {
            source,
            input_rate,
            output_rate: u64::from(output_rate),
            channels,
            left: Vec::with_capacity(channels),
            right: Vec::with_capacity(channels),
            channel: 0,
            phase: 0,
        }
    }

    fn seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.source.try_seek(pos)?;
        self.left.clear();
        self.right.clear();
        self.channel = 0;
        self.phase = 0;
        Ok(())
    }

    fn read_frame(source: &mut S, frame: &mut Vec<f32>, channels: usize) {
        frame.clear();
        frame.extend(source.take(channels));
        if frame.len() != channels {
            frame.clear();
        }
    }
}

impl<S: Source<Item = f32>> Iterator for Resampler<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.input_rate == self.output_rate {
            return self.source.next();
        }

        if self.channel == 0 {
            if self.left.is_empty() {
                Self::read_frame(&mut self.source, &mut self.left, self.channels);
                Self::read_frame(&mut self.source, &mut self.right, self.channels);
            }
            while self.phase >= self.output_rate && !self.left.is_empty() {
                self.phase -= self.output_rate;
                std::mem::swap(&mut self.left, &mut self.right);
                Self::read_frame(&mut self.source, &mut self.right, self.channels);
            }
        }
        let left = *self.left.get(self.channel)?;
        // Последний кадр удерживаем до конца его интервала.
        let right = self.right.get(self.channel).copied().unwrap_or(left);
        let fraction = self.phase as f32 / self.output_rate as f32;
        self.channel += 1;
        if self.channel == self.channels {
            self.channel = 0;
            self.phase += self.input_rate;
        }
        Some(left + (right - left) * fraction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn stereo_ramp() -> rodio::buffer::SamplesBuffer<f32> {
        let samples: Vec<_> = (0..4096).flat_map(|frame| {
            let value = frame as f32 / 4096.0;
            [value, -value]
        }).collect();
        rodio::buffer::SamplesBuffer::new(2, 44_100, samples)
    }

    #[test]
    fn resampling_preserves_duration_and_continuity_through_the_queue() {
        for rate in [192_000, 48_000, 44_100, 32_000] {
            let (input, mut queue) = rodio::queue::queue(false);
            input.append(Ahead::new(stereo_ramp(), rate));
            // До первого сэмпла очередь ещё сообщает параметры пустого источника.
            assert_eq!(queue.next(), Some(0.0));
            assert_eq!(queue.next(), Some(0.0));

            let mut output = rodio::source::UniformSourceIterator::<_, f32>::new(queue, 2, rate);
            // Читаем по одному сэмплу, как в микшере: size_hint в rodio 0.20
            // для пустого буфера посреди стереокадра возвращает неверную оценку.
            let played: Vec<_> = std::iter::from_fn(|| output.next()).collect();
            let expected = ((4096 * rate as usize).div_ceil(44_100) - 1) * 2;
            assert_eq!(played.len(), expected, "длительность при {rate} Гц");
            let max_step = 44_100.0 / rate as f32 / 4096.0 + 1e-6;
            let (frames, _) = played.as_chunks::<2>();
            for (previous, next) in frames.iter().zip(&frames[1..]) {
                assert!((next[0] - previous[0]).abs() < max_step, "разрыв при {rate} Гц");
                assert_eq!(next[0], -next[1]);
            }
        }
    }

    #[test]
    fn resampling_starts_at_the_new_position_after_seek() {
        let mut source = Ahead::new(stereo_ramp(), 192_000);
        assert!(source.by_ref().nth(1257).is_some());
        source.try_seek(Duration::from_millis(20)).unwrap();

        let frame = 882.0 / 4096.0;
        assert_eq!(source.next(), Some(frame));
        assert_eq!(source.next(), Some(-frame));
        let next = (882.0 + 44_100.0 / 192_000.0) / 4096.0;
        assert!((source.next().unwrap() - next).abs() < 1e-6);
    }

    #[test]
    fn resampling_a_finished_track_returns_and_seek_can_replay_it() {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let samples = rodio::buffer::SamplesBuffer::new(1, 48_000, vec![0.5f32; 16]);
            let mut src = rodio::source::UniformSourceIterator::<_, f32>::new(Ahead::new(samples, 48_000), 1, 48_000);
            assert_eq!(src.by_ref().count(), 16);
            assert_eq!(src.next(), None);
            src.try_seek(Duration::ZERO).unwrap();
            tx.send(src.next()).unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Some(0.5));
    }
}
