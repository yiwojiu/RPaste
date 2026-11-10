//! 打印 PNG 上若干坐标的像素值。
//!
//! 排查「看起来发灰」这类问题时，眼睛不可靠：同一处像素在 `PrintWindow`
//! 结果和屏幕抓取结果里差多少，只有数字说得清。
//!
//! 用法：`px <图片> x y [x y ...]`

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("用法: px <图片> x y […] 或 px <图片> --hist");
    let img = image::open(&path).expect("读图失败").to_rgba8();

    let rest: Vec<String> = args.collect();
    if rest.first().map(|s| s.as_str()) == Some("--hist") {
        hist(&img);
        return;
    }

    let nums: Vec<u32> = rest.iter().filter_map(|s| s.parse().ok()).collect();
    for pair in nums.chunks_exact(2) {
        let (x, y) = (pair[0], pair[1]);
        if x >= img.width() || y >= img.height() {
            println!("({x},{y}) 超出图片范围 {}x{}", img.width(), img.height());
            continue;
        }
        let p = img.get_pixel(x, y);
        println!("({x},{y}) = rgb({},{},{}) a={}", p[0], p[1], p[2], p[3]);
    }
}

/// 灰度直方图摘要：最亮的几个亮度档各占多少像素、最亮那一点在哪。
///
/// 「整屏发灰」和「某块区域被压暗」在这里是两种完全不同的形状：前者最大亮度
/// 上不去，后者只是暗区像素变多。
fn hist(img: &image::RgbaImage) {
    let mut buckets = [0u64; 16];
    let mut max_lum = 0u8;
    let mut max_at = (0u32, 0u32);
    for (x, y, p) in img.enumerate_pixels() {
        let lum = ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8;
        buckets[(lum / 16) as usize] += 1;
        if lum > max_lum {
            max_lum = lum;
            max_at = (x, y);
        }
    }
    let total: u64 = buckets.iter().sum();
    println!("{}x{} 共 {total} 像素", img.width(), img.height());
    for (i, n) in buckets.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        println!(
            "  亮度 {:3}-{:3}: {:>9} ({:>5.1}%)",
            i * 16,
            i * 16 + 15,
            n,
            *n as f64 * 100.0 / total as f64
        );
    }
    println!("最亮 {max_lum} @ {max_at:?}");
}
