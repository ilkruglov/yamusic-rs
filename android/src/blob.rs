//! Подложка карточки в колесе волн: арка цвета карточки, освещённая сверху справа и
//! тающая книзу. Форма и яркость сняты по пикселям со скриншота оригинала.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::collections::HashMap;

/// Размер арки в dp, как в разметке ui/app.slint.
const WIDTH: f32 = 134.0;
const HEIGHT: f32 = 160.0;
/// Пикселей картинки на dp: подложка размытая, полной плотности экрана ей не нужно.
const DENSITY: f32 = 2.0;

#[derive(Default)]
pub struct Blobs(HashMap<[u8; 3], Image>);

impl Blobs {
    pub fn get(&mut self, color: [u8; 3]) -> Image {
        self.0.entry(color).or_insert_with(|| render(color)).clone()
    }
}

fn render(color: [u8; 3]) -> Image {
    let (w, h) = ((WIDTH * DENSITY) as u32, (HEIGHT * DENSITY) as u32);
    let r = w as f32 / 2.0;
    // Свет падает сверху справа.
    let (lx, ly) = (0.6f32, -0.8f32);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let x = (i as u32 % w) as f32 + 0.5;
        let y = (i as u32 / w) as f32 + 0.5;
        let dx = x - r;
        // Расстояние до края внутрь фигуры и нормаль края.
        let (dist, nx, ny) = if y >= r {
            (r - dx.abs(), dx.signum(), 0.0)
        } else {
            let len = dx.hypot(y - r).max(1e-3);
            (r - len, dx / len, (y - r) / len)
        };
        // Теневой левый край размыт на 10 dp, освещённый правый чёткий.
        let soft = 1.0 + 10.0 * DENSITY * (-nx).max(0.0);
        let cover = ((dist + 0.5) / soft).clamp(0.0, 1.0);
        if cover == 0.0 {
            continue;
        }
        let u = x / w as f32;
        let lit = (nx * lx + ny * ly).max(0.0);
        let rim = lit * (-dist / (2.0 * DENSITY)).exp();
        let body = 0.15 + 0.55 * u + 0.35 * rim;
        let fade = 1.0 - ((y / DENSITY - 15.0) / 140.0).clamp(0.0, 1.0);
        let fade = fade * fade * (3.0 - 2.0 * fade);
        let channel = |c: u8| ((c as f32 * body + 255.0 * 0.12 * rim).min(255.0)) as u8;
        *px = Rgba8Pixel { r: channel(color[0]), g: channel(color[1]), b: channel(color[2]), a: (cover * fade * 255.0) as u8 };
    }
    Image::from_rgba8(buf)
}

/// Цвет подложки — корзина палитры «Моей волны» по оттенку обложки, серые по светлоте.
pub fn tint(cover: Option<[u8; 3]>) -> [u8; 3] {
    use crate::palette::{BUCKETS, LIGHTNESS};
    let by_name = |n: &str| BUCKETS.iter().find(|b| b.0 == n);
    let fallback = by_name("slate").map_or([0x4F, 0x6C, 0x9B], |b| b.3);
    let Some(c) = cover else { return fallback };
    let (h, s, l) = hsl(c);
    let found = if s > 0.0 {
        let h = h as u16;
        BUCKETS.iter().find(|b| if b.1 > b.2 { h >= b.1 || h <= b.2 } else { h >= b.1 && h <= b.2 })
    } else {
        let l = l as u8;
        let last = LIGHTNESS.len() - 1;
        LIGHTNESS
            .iter()
            .enumerate()
            .find(|(i, b)| if *i == last { l >= b.1 && l <= b.2 } else { l >= b.1 && l < b.2 })
            .and_then(|(_, b)| by_name(b.0))
    };
    found.map_or(fallback, |b| b.3)
}

/// Оттенок в градусах, насыщенность и светлота в процентах.
fn hsl(c: [u8; 3]) -> (f32, f32, f32) {
    let [r, g, b] = c.map(|v| v as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, (l * 100.0).round());
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    ((h * 60.0).round(), (s * 100.0).round(), (l * 100.0).round())
}
