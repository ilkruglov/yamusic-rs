//! Список ASIO берётся из реестра. Поток плеера открывает выбранный COM-драйвер,
//! который запрашивает данные у микшера Rodio.
use rodio::dynamic_mixer::{mixer, DynamicMixer, DynamicMixerController};
use rodio::{Sink, Source};
use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void, CStr};
use std::ptr::NonNull;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};

unsafe extern "C" {
    fn ya_asio_open(
        id: *const u16,
        out: *mut *mut c_void,
        rate: *mut u32,
        channels: *mut u16,
        frames: *mut u32,
    ) -> *const c_char;
    fn ya_asio_start(
        handle: *mut c_void,
        ctx: *mut c_void,
        render: extern "C" fn(*mut c_void, *mut f32, usize),
    ) -> bool;
    fn ya_asio_close(handle: *mut c_void);
    fn ya_asio_progress(handle: *mut c_void, reset: *mut bool) -> u32;
    #[cfg(test)]
    fn ya_asio_channel(format: i32, src: *const f32, stride: i32, dst: *mut u8, frames: i32);
}

fn drivers() -> Vec<(String, String)> {
    let Ok(keys) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SOFTWARE\ASIO") else {
        return Vec::new();
    };
    let mut drivers: Vec<_> = keys
        .enum_keys()
        .filter_map(Result::ok)
        .filter_map(|name| {
            let key = keys.open_subkey(&name).ok()?;
            let clsid: String = key.get_value("CLSID").ok()?;
            let label = key
                .get_value::<String, _>("Description")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or(name);
            Some((label, clsid))
        })
        .collect();
    drivers.sort();
    drivers.dedup();
    drivers
}

fn choices(drivers: &[(String, String)]) -> impl Iterator<Item = (String, &str)> {
    drivers.iter().map(|(name, id)| {
        let duplicate = drivers
            .iter()
            .filter(|(other, _)| other == name)
            .take(2)
            .count()
            > 1;
        let label = if duplicate {
            format!("{name} [{id}]")
        } else {
            name.clone()
        };
        (label, id.as_str())
    })
}

pub(crate) fn names(configured: Option<&str>) -> Vec<String> {
    let entries = drivers();
    choices(&entries).map(|(label, id)| {
        let qualified = format!("{label} [{id}]");
        if configured == Some(qualified.as_str()) { qualified } else { label }
    }).collect()
}

struct Handle(NonNull<c_void>);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { ya_asio_close(self.0.as_ptr()) }
    }
}

pub(crate) struct Stream {
    // Останавливаем обработчики до освобождения микшера.
    handle: Handle,
    render_state: Box<UnsafeCell<RenderBuffers>>,
    controller: Arc<DynamicMixerController<f32>>,
    rate: u32,
    callbacks: u32,
    last_callback: Instant,
}

struct Mixer {
    stereo: DynamicMixer<f32>,
    channels: u16,
}

impl Mixer {
    fn fill(&mut self, output: &mut [f32]) {
        if self.channels == 1 {
            for sample in output {
                *sample = 0.5
                    * (self.stereo.next().unwrap_or_default()
                        + self.stereo.next().unwrap_or_default());
            }
        } else {
            for sample in output {
                *sample = self.stereo.next().unwrap_or_default();
            }
        }
    }
}

// Декодер может ждать сеть. Вызов драйвера берёт только готовый блок или выдаёт тишину.
struct RenderBuffers {
    channels: usize,
    rate: u32,
    ready: mpsc::Receiver<Box<[f32]>>,
    free: mpsc::SyncSender<Box<[f32]>>,
}

impl RenderBuffers {
    fn new(mut mixer: Mixer, samples: usize) -> std::io::Result<Self> {
        let channels = usize::from(mixer.channels);
        let rate = mixer.stereo.sample_rate();
        let (free, spare) = mpsc::sync_channel(2);
        let (filled, ready) = mpsc::sync_channel(2);
        for _ in 0..2 {
            free.send(vec![0.0; samples].into_boxed_slice()).unwrap();
        }
        std::thread::Builder::new().name("asio-mix".into()).spawn(move || {
            while let Ok(mut block) = spare.recv() {
                mixer.fill(&mut block);
                if filled.send(block).is_err() { break; }
            }
        })?;
        Ok(Self { ready, free, channels, rate })
    }

    fn fill(&mut self, out: &mut [f32]) {
        match self.ready.try_recv() {
            Ok(block) => {
                out.copy_from_slice(&block);
                // Из двух заранее выделенных блоков один сейчас у нас.
                let _ = self.free.try_send(block);
            }
            Err(_) => out.fill(0.0),
        }
        crate::output::TAP.record_asio(out, self.channels, self.rate);
    }
}

extern "C" fn render(ctx: *mut c_void, samples: *mut f32, count: usize) {
    // buffer_switch защищает микшер от одновременного и рекурсивного вызова.
    let buffers = unsafe { &mut *ctx.cast::<RenderBuffers>() };
    buffers.fill(unsafe { std::slice::from_raw_parts_mut(samples, count) });
}

// До первого сэмпла очередь Rodio сообщает формат моно.
// Приводим трек к формату микшера до добавления в очередь
// и сохраняем этот формат во время тишины.
struct StereoQueue {
    queue: rodio::queue::SourcesQueueOutput<f32>,
    rate: u32,
}

impl Iterator for StereoQueue {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        self.queue.next()
    }
}

impl Source for StereoQueue {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

fn track_sink(
    controller: &DynamicMixerController<f32>,
    rate: u32,
    track: impl Source<Item = f32> + Send + 'static,
    volume: f32,
) -> Sink {
    let (sink, queue) = Sink::new_idle();
    sink.pause();
    sink.set_volume(volume);
    sink.append(rodio::source::UniformSourceIterator::<_, f32>::new(
        track, 2, rate,
    ));
    controller.add(StereoQueue { queue, rate });
    sink
}

impl Stream {
    pub(crate) fn open(name: &str) -> Result<Self, String> {
        let fail = |why: &str| format!("Не удалось открыть ASIO-драйвер «{name}»: {why}");
        let drivers = drivers();
        let (_, clsid) = choices(&drivers)
            .find(|(label, _)| label == name)
            .or_else(|| {
                drivers
                    .iter()
                    .find(|(label, id)| format!("{label} [{id}]") == name)
                    .map(|(label, id)| (label.clone(), id.as_str()))
            })
            .ok_or_else(|| fail("драйвер не найден или требует повторного выбора"))?;
        let id: Vec<_> = clsid.encode_utf16().chain([0]).collect();
        let (mut handle, mut rate, mut channels, mut frames) = (std::ptr::null_mut(), 0, 0, 0);
        let error = unsafe { ya_asio_open(id.as_ptr(), &mut handle, &mut rate, &mut channels, &mut frames) };
        if !error.is_null() {
            return Err(fail(&unsafe { CStr::from_ptr(error) }.to_string_lossy()));
        }
        let handle = Handle(NonNull::new(handle).ok_or_else(|| fail("нет устройства"))?);
        let (controller, stereo) = mixer(2, rate);
        let buffers = RenderBuffers::new(Mixer { stereo, channels }, frames as usize * usize::from(channels))
            .map_err(|e| fail(&e.to_string()))?;
        let stream = Self {
            handle,
            controller,
            render_state: Box::new(UnsafeCell::new(buffers)),
            rate,
            callbacks: 0,
            last_callback: Instant::now(),
        };
        let ctx = stream.render_state.get().cast();
        if !unsafe { ya_asio_start(stream.handle.0.as_ptr(), ctx, render) } {
            return Err(fail("ASIOStart"));
        }
        Ok(stream)
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub(crate) fn sink(
        &self,
        track: impl Source<Item = f32> + Send + 'static,
        volume: f32,
    ) -> Sink {
        track_sink(&self.controller, self.rate, track, volume)
    }

    #[cfg(test)]
    pub(crate) fn callback_count(&self) -> u32 {
        unsafe { ya_asio_progress(self.handle.0.as_ptr(), &mut false) }
    }

    pub(crate) fn needs_reopen(&mut self) -> bool {
        let mut reset = false;
        let count = unsafe { ya_asio_progress(self.handle.0.as_ptr(), &mut reset) };
        if count != self.callbacks {
            self.callbacks = count;
            self.last_callback = Instant::now();
        }
        reset || self.last_callback.elapsed() > Duration::from_secs(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_stereo_channels_reach_a_mono_device_and_stereo_stays_separate() {
        for channels in [1, 2] {
            let (controller, stereo) = mixer(2, 48000);
            let track = rodio::buffer::SamplesBuffer::new(2, 48000, [0.0, 0.75].repeat(10000));
            let sink = track_sink(&controller, 48000, track, 1.0);
            sink.play();
            let mut buffer = [0.0; 8192];
            Mixer { stereo, channels }.fill(&mut buffer);
            if channels == 1 {
                assert!(buffer.iter().all(|sample| *sample == 0.375));
            } else {
                assert!(buffer
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|frame| *frame == [0.0, 0.75]));
            }
        }
    }

    #[test]
    fn callback_returns_silence_while_the_source_waits_for_data() {
        let (control, stereo) = mixer(2, 48000);
        let (release, gate) = mpsc::channel();
        let (entered, worker) = mpsc::channel();
        let mut pending = true;
        let track = rodio::buffer::SamplesBuffer::new(2, 48000, vec![0.5; 256]);
        control.add(track.periodic_access(Duration::ZERO, move |_| {
            if pending {
                pending = false;
                entered.send(()).unwrap();
                gate.recv().unwrap();
            }
        }));
        let mut buffers = RenderBuffers::new(Mixer { stereo, channels: 2 }, 16).unwrap();
        worker.recv_timeout(Duration::from_secs(2)).unwrap();
        crate::output::TAP.listen(true);
        let mut out = [1.0; 16];
        buffers.fill(&mut out);
        assert_eq!(out, [0.0; 16]);
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            buffers.fill(&mut out);
            if out == [0.5; 16] { break; }
            assert!(Instant::now() < deadline, "буфер не получил звук после возобновления источника");
            std::thread::yield_now();
        }
        let (_, samples, rate) = crate::output::TAP.snapshot();
        crate::output::TAP.listen(false);
        assert_eq!(rate, 48000);
        assert_eq!(&samples[samples.len() - 8..], &[0.5; 8]);
    }

    #[test]
    fn duplicate_driver_labels_can_be_selected_independently() {
        let drivers = [("DAC".into(), "x".into()), ("DAC".into(), "y".into())];
        assert_eq!(
            choices(&drivers).collect::<Vec<_>>(),
            [("DAC [x]".into(), "x"), ("DAC [y]".into(), "y")]
        );
    }

    #[test]
    fn aligned_pcm24_uses_a_signed_32_bit_container() {
        let samples = [-1.0_f32, 0.5];
        let mut out = [0; 8];
        unsafe { ya_asio_channel(27, samples.as_ptr(), 1, out.as_mut_ptr(), 2); }
        assert_eq!(out, [0, 0, 128, 255, 0, 0, 64, 0]);
    }

    #[test]
    fn packed_pcm24_preserves_stereo_channels_and_byte_order() {
        let samples = [-1.0_f32, 0.5, 0.0, -0.5];
        let mut left = [0; 6];
        let mut right = [0; 6];
        unsafe {
            // Левый канал — ASIOSTInt24LSB, правый — ASIOSTInt24MSB.
            ya_asio_channel(17, samples.as_ptr(), 2, left.as_mut_ptr(), 2);
            ya_asio_channel(1, samples[1..].as_ptr(), 2, right.as_mut_ptr(), 2);
        }
        assert_eq!(left, [0, 0, 128, 0, 0, 0]);
        assert_eq!(right, [64, 0, 0, 192, 0, 0]);
    }
}
