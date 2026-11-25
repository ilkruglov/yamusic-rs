//! VectorDrawable из скомпилированного XML Android (AXML) в SVG для Slint. Поддержано то,
//! что встречается в иконках: path, group с переносом, поворотом и масштабом, цвета ARGB.
//! Цвета-ссылки на ресурсы не разрешаются и становятся белыми: иконки всё равно
//! перекрашиваются в Slint через colorize.

/// Атрибут элемента: имя и значение, уже приведённое к тексту.
struct Attr {
    name: String,
    value: String,
}

struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn u8(&self, i: usize) -> Option<u8> {
        self.0.get(i).copied()
    }

    fn u16(&self, i: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.0.get(i..i + 2)?.try_into().ok()?))
    }

    fn u32(&self, i: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.0.get(i..i + 4)?.try_into().ok()?))
    }
}

pub fn to_svg(xml: &[u8]) -> Option<String> {
    let b = Bytes(xml);
    if b.u16(0)? != 0x0003 {
        return None;
    }
    let mut strings: Vec<String> = Vec::new();
    let mut svg = String::new();
    let mut at = b.u16(2)? as usize;
    while at + 8 <= xml.len() {
        let kind = b.u16(at)?;
        let size = b.u32(at + 4)? as usize;
        if size < 8 {
            return None;
        }
        match kind {
            0x0001 => strings = string_pool(xml.get(at..at + size)?)?,
            0x0102 => {
                let name = strings.get(b.u32(at + 20)? as usize)?.clone();
                let attr_start = b.u16(at + 24)? as usize;
                let attr_size = b.u16(at + 26)? as usize;
                let count = b.u16(at + 28)? as usize;
                let mut attrs = Vec::with_capacity(count);
                for i in 0..count {
                    let a = at + 16 + attr_start + i * attr_size;
                    let name = strings.get(b.u32(a + 4)? as usize).cloned().unwrap_or_default();
                    let value = typed_value(b.u8(a + 15)?, b.u32(a + 16)?, b.u32(a + 8)?, &strings);
                    attrs.push(Attr { name, value });
                }
                open_element(&mut svg, &name, &attrs);
            }
            0x0103 => match strings.get(b.u32(at + 20)? as usize)?.as_str() {
                "vector" => svg.push_str("</svg>"),
                "group" => svg.push_str("</g>"),
                _ => {}
            },
            _ => {}
        }
        at += size;
    }
    svg.starts_with("<svg").then_some(svg)
}

fn string_pool(chunk: &[u8]) -> Option<Vec<String>> {
    let b = Bytes(chunk);
    let header = b.u16(2)? as usize;
    let count = b.u32(8)? as usize;
    let utf8 = b.u32(16)? & 0x100 != 0;
    let start = b.u32(20)? as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let mut p = start + b.u32(header + i * 4)? as usize;
        if utf8 {
            // Сначала длина в символах, потом в байтах; каждая занимает 1 или 2 байта.
            let mut len = 0;
            for _ in 0..2 {
                let first = b.u8(p)? as usize;
                len = if first & 0x80 != 0 {
                    p += 2;
                    ((first & 0x7f) << 8) | b.u8(p - 1)? as usize
                } else {
                    p += 1;
                    first
                };
            }
            out.push(String::from_utf8_lossy(chunk.get(p..p + len)?).into_owned());
        } else {
            let mut len = b.u16(p)? as usize;
            p += 2;
            if len & 0x8000 != 0 {
                len = ((len & 0x7fff) << 16) | b.u16(p)? as usize;
                p += 2;
            }
            let units: Vec<u16> = (0..len).map(|k| b.u16(p + k * 2)).collect::<Option<_>>()?;
            out.push(String::from_utf16_lossy(&units));
        }
    }
    Some(out)
}

fn typed_value(kind: u8, data: u32, raw: u32, strings: &[String]) -> String {
    match kind {
        0x03 => strings.get(data as usize).cloned().unwrap_or_default(),
        0x04 => f32::from_bits(data).to_string(),
        // Размер в dp: мантисса и основание, как TypedValue.complexToFloat.
        0x05 => {
            const MULT: [f32; 4] = [1.0 / 256.0, 1.0 / 32_768.0, 1.0 / 8_388_608.0, 1.0 / 2_147_483_648.0];
            ((data & 0xffff_ff00) as i32 as f32 * MULT[((data >> 4) & 3) as usize]).to_string()
        }
        0x10 | 0x11 => (data as i32).to_string(),
        0x12 => (data != 0).to_string(),
        0x1c..=0x1f => format!("#{data:08x}"),
        _ if raw != u32::MAX => strings.get(raw as usize).cloned().unwrap_or_default(),
        _ => "#ffffffff".into(),
    }
}

fn attr<'a>(attrs: &'a [Attr], name: &str) -> Option<&'a str> {
    attrs.iter().find(|a| a.name == name).map(|a| a.value.as_str())
}

/// `#aarrggbb` → цвет SVG и прозрачность.
fn svg_color(argb: &str) -> (String, f32) {
    let hex = argb.trim_start_matches('#');
    let v = u32::from_str_radix(hex, 16).unwrap_or(0xffff_ffff);
    let alpha = if hex.len() == 8 { (v >> 24) as f32 / 255.0 } else { 1.0 };
    (format!("#{:06x}", v & 0xff_ffff), alpha)
}

fn open_element(svg: &mut String, name: &str, attrs: &[Attr]) {
    let num = |k: &str, d: f32| attr(attrs, k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    match name {
        "vector" => {
            let (w, h) = (num("width", 24.0), num("height", 24.0));
            let (vw, vh) = (num("viewportWidth", w), num("viewportHeight", h));
            svg.push_str(&format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {vw} {vh}\">"
            ));
        }
        "group" => {
            let (px, py) = (num("pivotX", 0.0), num("pivotY", 0.0));
            svg.push_str(&format!(
                "<g transform=\"translate({} {}) rotate({}) scale({} {}) translate({} {})\">",
                num("translateX", 0.0) + px,
                num("translateY", 0.0) + py,
                num("rotation", 0.0),
                num("scaleX", 1.0),
                num("scaleY", 1.0),
                -px,
                -py
            ));
        }
        "path" => {
            let Some(d) = attr(attrs, "pathData") else { return };
            let mut el = format!("<path d=\"{d}\"");
            match attr(attrs, "fillColor") {
                Some(c) => {
                    let (rgb, a) = svg_color(c);
                    el.push_str(&format!(" fill=\"{rgb}\" fill-opacity=\"{}\"", a * num("fillAlpha", 1.0)));
                }
                None => el.push_str(" fill=\"none\""),
            }
            if attr(attrs, "fillType") == Some("1") {
                el.push_str(" fill-rule=\"evenodd\"");
            }
            if let Some(c) = attr(attrs, "strokeColor") {
                let (rgb, a) = svg_color(c);
                el.push_str(&format!(
                    " stroke=\"{rgb}\" stroke-opacity=\"{}\" stroke-width=\"{}\"",
                    a * num("strokeAlpha", 1.0),
                    num("strokeWidth", 0.0)
                ));
                match attr(attrs, "strokeLineCap") {
                    Some("1") => el.push_str(" stroke-linecap=\"round\""),
                    Some("2") => el.push_str(" stroke-linecap=\"square\""),
                    _ => {}
                }
                match attr(attrs, "strokeLineJoin") {
                    Some("1") => el.push_str(" stroke-linejoin=\"round\""),
                    Some("2") => el.push_str(" stroke-linejoin=\"bevel\""),
                    _ => {}
                }
            }
            el.push_str("/>");
            svg.push_str(&el);
        }
        _ => {}
    }
}
