//! Цвета и скорость «Моей волны» по параметрам трека, как в официальном Android-клиенте:
//! три пятна, у каждого два оттенка; без трека — жёлтый, пурпурный и красный.

use crate::api::Mood;

pub type Rgb = [f32; 3];

/// Шесть цветов в порядке uniform `vColor`: внешнее пятно, среднее, внутреннее, затем
/// вторые оттенки тех же пятен.
pub type Palette = [Rgb; 6];

pub fn palette(mood: Option<Mood>) -> Palette {
    let mut rng = Rng::new();
    // Оттенки в том порядке, в каком их собирает оригинал: пары (основной, сдвинутый)
    // для внутреннего, среднего и внешнего пятна.
    let hues = match mood {
        None => [50.0, 50.0, 300.0, 320.0, 10.0, 30.0],
        Some(m) => {
            let base = (m.hue + 280.0) % 360.0;
            let mid = keep_range(base + rng.range(40, 80), base);
            let outer = keep_range(base + rng.range(120, 160), base);
            [
                base,
                keep_range(base + rng.range(30, 40), base),
                mid,
                keep_range(mid + rng.range(30, 40), base),
                outer,
                keep_range(outer + rng.range(30, 40), base),
            ]
        }
    };
    let c = hues.map(hue_color);
    [c[5], c[3], c[1], c[4], c[2], c[0]]
}

/// Оттенки не уходят в пурпур дальше 280°, если основной оттенок не лежит там сам.
fn keep_range(hue: f32, base: f32) -> f32 {
    if (280.0..=359.0).contains(&base) {
        hue % 360.0
    } else {
        hue.min(280.0)
    }
}

/// HSL с насыщенностью 100% и светлотой 50%, с округлением до байта, как ColorUtils Android.
fn hue_color(hue: f32) -> Rgb {
    let h = hue.min(359.0);
    let c = 1.0;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r, g, b].map(|v| (v * 255.0).round().clamp(0.0, 255.0) / 255.0)
}

/// Случайность только для разброса оттенков, криптостойкость не нужна.
struct Rng(u64);

impl Rng {
    fn new() -> Rng {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        Rng(seed | 1)
    }

    /// Целое от `lo` до `hi` включительно.
    fn range(&mut self, lo: u32, hi: u32) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (lo + (self.0 % u64::from(hi - lo + 1)) as u32) as f32
    }
}
