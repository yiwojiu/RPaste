//! 生成图标：写轮眼（Sharingan）徽标。
//!
//! 不做整只眼睛（杏仁眼白在 16px 下只会糊成一坨），只取最有辨识度的
//! 虹膜特写：红色圆盘 + 黑色外环 + 三枚勾玉 + 中心黑瞳。这样既是写轮眼，
//! 又天然是个适合托盘的圆形徽章。
//!
//! 勾玉 = 圆头 + 沿圆周顺时针甩出、宽度线性收细的弧尾，两者叠加成逗号。
//! 三枚按 120° 均分，尾巴同向，形成旋转感。
//!
//! 分两档，因为托盘实际只用 16–32px：
//! - `size < 32`：勾玉尾巴缩短加粗、省略瞳孔外细环，所有黑色件拉开间距，
//!   靠「红底 + 四个黑块」的对比度辨认。
//! - `size >= 32`：完整弧尾 + 瞳孔外细环 + 径向渐变。
//!
//! 用法：cargo run --release --example gen_icon

use image::{Rgba, RgbaImage};

/// 托盘在 100% DPI 下要 16px，150% 要 24px，200% 要 32px。
const SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];

// ── 配色 ────────────────────────────────────────────────
/// 虹膜：中心亮红、边缘压暗。边缘不能压到近黑，否则 16px 下会看成黑块。
const IRIS_IN: [u8; 3] = [0xFF, 0x3B, 0x47];
const IRIS_OUT: [u8; 3] = [0x93, 0x0E, 0x1E];
/// 勾玉、瞳孔、外环：近黑，留一点红调让边缘不死板。
const INK: [u8; 3] = [0x14, 0x05, 0x0A];
/// 圆外霓虹辉光，只做很薄一层，别把图标搞成发光灯泡。
const GLOW: [u8; 3] = [0xFF, 0x2D, 0x3D];

fn ss_for(size: u32) -> u32 {
    if size <= 32 { 8 } else { 4 }
}

/// 标准 source-over 合成。半透明辉光叠在已有像素上必须走这套，
/// 直接覆盖会把底下的渐变抹掉。
fn blend(img: &mut RgbaImage, x: u32, y: u32, rgb: [u8; 3], a: f32) {
    if a <= 0.0 {
        return;
    }
    let sa = a.min(1.0);
    let dst = img.get_pixel(x, y).0;
    let da = dst[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 {
        return;
    }
    let ch = |s: u8, d: u8| -> u8 {
        ((s as f32 * sa + d as f32 * da * (1.0 - sa)) / oa)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    img.put_pixel(
        x,
        y,
        Rgba([
            ch(rgb[0], dst[0]),
            ch(rgb[1], dst[1]),
            ch(rgb[2], dst[2]),
            (oa * 255.0).round() as u8,
        ]),
    );
}

/// 实心圆的有符号距离：负 = 落在圆内。
fn disc_sdf(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let dx = px - cx;
    let dy = py - cy;
    (dx * dx + dy * dy).sqrt() - r
}

/// 圆环的有符号距离：负 = 落在环带上，正 = 在环带外（内外都算外）。
///
/// 必须用 `|dist - r|`：写成 `dist - r - w` 得到的是**实心圆盘**，会把整圆填满。
fn ring_sdf(px: f32, py: f32, cx: f32, cy: f32, radius: f32, half_w: f32) -> f32 {
    let dx = px - cx;
    let dy = py - cy;
    ((dx * dx + dy * dy).sqrt() - radius).abs() - half_w
}

/// 带角度裁剪、且**宽度沿线渐变**的弧，用来画勾玉的尾巴。
///
/// 起笔宽、收笔细，才有「甩出去」的动势；等宽弧看着像一截管子。
/// `from`/`to` 是弧度，按 atan2 的取值区间走，允许 `from > to`（跨 0°）。
/// 返回「到弧带的距离」，落在弧上时为正，方便直接写 `if d > 0.0`。
#[allow(clippy::too_many_arguments)]
fn arc_cover(
    px: f32,
    py: f32,
    cx: f32,
    cy: f32,
    r_start: f32,
    r_end: f32,
    w_start: f32,
    w_end: f32,
    from: f32,
    to: f32,
) -> f32 {
    let dx = px - cx;
    let dy = py - cy;
    let dist = (dx * dx + dy * dy).sqrt();

    let mut ang = dy.atan2(dx);
    if ang < 0.0 {
        ang += std::f32::consts::TAU;
    }

    // rem_euclid 处理跨 0° 的区间，不用分两种情况写
    let rel = (ang - from).rem_euclid(std::f32::consts::TAU);
    let span = (to - from).rem_euclid(std::f32::consts::TAU);
    if rel > span {
        return -1.0;
    }

    let t = if span > f32::EPSILON { rel / span } else { 0.0 };
    let radius = r_start + (r_end - r_start) * t;
    let half_w = w_start + (w_end - w_start) * t;

    let band = half_w - (dist - radius).abs();
    if band <= 0.0 { -1.0 } else { band }
}

/// 在图上填一个实心圆盘。只扫包围盒，别为三个勾玉各跑一遍 1024² 全图。
fn fill_disc(img: &mut RgbaImage, cx: f32, cy: f32, r: f32, rgb: [u8; 3], a: f32) {
    let (w, h) = (img.width(), img.height());
    let x0 = (cx - r - 1.0).floor().max(0.0) as u32;
    let x1 = (cx + r + 1.0).ceil().min(w as f32) as u32;
    let y0 = (cy - r - 1.0).floor().max(0.0) as u32;
    let y1 = (cy + r + 1.0).ceil().min(h as f32) as u32;
    for y in y0..y1 {
        for x in x0..x1 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            if disc_sdf(fx, fy, cx, cy, r) <= 0.0 {
                blend(img, x, y, rgb, a);
            }
        }
    }
}

fn draw(size: u32) -> RgbaImage {
    let ss = ss_for(size);
    let big = size * ss;
    let s = big as f32;
    let mut img = RgbaImage::new(big, big);

    let p = |v: f32| v * s;
    let cx = s / 2.0;
    let cy = s / 2.0;
    let small = size < 32;

    // ── 1. 虹膜：红色径向渐变圆底 ───────────────────────
    let r_base = p(0.485);
    for y in 0..big {
        for x in 0..big {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dx = fx - cx;
            let dy = fy - cy;
            let d = (dx * dx + dy * dy).sqrt();
            if d > r_base {
                continue;
            }
            let t = (d / r_base).powf(1.5);
            let col = [
                (IRIS_IN[0] as f32 + (IRIS_OUT[0] as f32 - IRIS_IN[0] as f32) * t) as u8,
                (IRIS_IN[1] as f32 + (IRIS_OUT[1] as f32 - IRIS_IN[1] as f32) * t) as u8,
                (IRIS_IN[2] as f32 + (IRIS_OUT[2] as f32 - IRIS_IN[2] as f32) * t) as u8,
            ];
            blend(&mut img, x, y, col, 1.0);
        }
    }

    // ── 2. 圆外薄辉光 ──────────────────────────────────
    // 只渗出去一点点，纯装饰；16px 下会被降采样吃掉，不影响辨识。
    let glow_r = p(0.032);
    let g0 = (cx - r_base - glow_r - 1.0).floor().max(0.0) as u32;
    let g1 = (cx + r_base + glow_r + 1.0).ceil().min(s) as u32;
    for y in g0..g1 {
        for x in g0..g1 {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            let dd = d - r_base;
            if dd > 0.0 && dd <= glow_r {
                blend(&mut img, x, y, GLOW, 0.30 * (1.0 - dd / glow_r));
            }
        }
    }

    // ── 3. 虹膜外环（黑） ──────────────────────────────
    let ring_r = p(if small { 0.415 } else { 0.405 });
    let ring_half = p(if small { 0.034 } else { 0.028 });
    for y in 0..big {
        for x in 0..big {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            if ring_sdf(fx, fy, cx, cy, ring_r, ring_half) <= 0.0 {
                blend(&mut img, x, y, INK, 1.0);
            }
        }
    }

    // ── 4. 三枚勾玉：圆头 + 顺时针收细的弧尾 ────────────
    let orbit_r = p(if small { 0.258 } else { 0.248 });
    let head_r = p(0.070);
    // 尾巴长度约等于头直径的 1.3 倍才像逗号；再长三枚就会首尾相连成风车叶片。
    let tail_span = if small { 34.0_f32 } else { 44.0_f32 }.to_radians();
    let tail_w0 = head_r * 0.85;
    let tail_w1 = if small { p(0.022) } else { p(0.008) };

    for i in 0..3 {
        // 270° = 正上，另外两枚落在左下 / 右下，整体左右对称，小尺寸更稳
        let theta = (270.0 + 120.0 * i as f32).to_radians();
        let (hx, hy) = (cx + orbit_r * theta.cos(), cy + orbit_r * theta.sin());

        // 4a. 圆头
        fill_disc(&mut img, hx, hy, head_r, INK, 1.0);

        // 4b. 尾巴：从头的中心角度出发，沿圆周顺时针甩出，宽度线性收细
        let from = theta;
        let to = theta + tail_span;
        let r_end = orbit_r + p(0.020);
        let reach = r_end + tail_w0 + 1.0;
        let bx0 = (cx - reach).floor().max(0.0) as u32;
        let bx1 = (cx + reach).ceil().min(s) as u32;
        for y in bx0..bx1 {
            for x in bx0..bx1 {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                if arc_cover(fx, fy, cx, cy, orbit_r, r_end, tail_w0, tail_w1, from, to) > 0.0 {
                    blend(&mut img, x, y, INK, 1.0);
                }
            }
        }
    }

    // ── 5. 中心黑瞳 ────────────────────────────────────
    let pupil_r = p(if small { 0.105 } else { 0.112 });
    fill_disc(&mut img, cx, cy, pupil_r, INK, 1.0);

    // ── 6. 大尺寸才加的层次：瞳孔外一道细黑环 ───────────
    if !small {
        let inner_r = p(0.155);
        let inner_half = p(0.010);
        for y in 0..big {
            for x in 0..big {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                if ring_sdf(fx, fy, cx, cy, inner_r, inner_half) <= 0.0 {
                    blend(&mut img, x, y, INK, 0.82);
                }
            }
        }
    }

    image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3)
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    let enc = image::codecs::png::PngEncoder::new(&mut out);
    enc.write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgba8,
    )
    .expect("PNG 编码失败");
    out
}

/// 按 ICO 格式打包。每个条目内嵌 PNG（Vista 起支持）。
fn write_ico(path: &std::path::Path) -> std::io::Result<()> {
    let images: Vec<Vec<u8>> = SIZES.iter().map(|&sz| encode_png(&draw(sz))).collect();

    let mut buf = Vec::new();
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buf.extend_from_slice(&1u16.to_le_bytes()); // type: 1 = icon
    buf.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());

    let header_len = 6 + 16 * SIZES.len() as u32;
    let mut offset = header_len;

    for (i, &sz) in SIZES.iter().enumerate() {
        buf.push(if sz >= 256 { 0 } else { sz as u8 });
        buf.push(if sz >= 256 { 0 } else { sz as u8 });
        buf.push(0); // 调色板颜色数
        buf.push(0); // 保留
        buf.extend_from_slice(&1u16.to_le_bytes()); // planes
        buf.extend_from_slice(&32u16.to_le_bytes()); // bit count
        buf.extend_from_slice(&(images[i].len() as u32).to_le_bytes());
        buf.extend_from_slice(&offset.to_le_bytes());
        offset += images[i].len() as u32;
    }

    for img in &images {
        buf.extend_from_slice(img);
    }

    std::fs::write(path, buf)
}

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    std::fs::create_dir_all(&dir).unwrap();

    let ico = dir.join("icon.ico");
    write_ico(&ico).expect("写 ICO 失败");

    for sz in [16u32, 24, 32, 48, 256] {
        let png = dir.join(format!("icon-{sz}.png"));
        std::fs::write(&png, encode_png(&draw(sz))).unwrap();
    }

    println!(
        "已生成 {} ({} 字节) 以及 icon-16/24/32/48/256.png",
        ico.display(),
        std::fs::metadata(&ico).map(|m| m.len()).unwrap_or(0)
    );
}
