//! 往剪贴板推一张测试图，用来验证图片捕获的三条路径。
//!
//! 用法：
//!     cargo run --example push_image -- png         # 只给 CF_PNG
//!     cargo run --example push_image -- dib-top     # CF_DIB，biHeight 为负（top-down）
//!     cargo run --example push_image -- dib-bottom  # CF_DIB，biHeight 为正（bottom-up，行序要翻转）
//!
//! 图里画了方向标记：左上角红块、顶部绿条、左侧黄条。
//! 如果捕获链路把行序搞反了，存下来的 PNG 会上下颠倒，一眼能看出来。

use std::ptr;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

const CF_DIB: u32 = 8;

fn make_image(w: u32, h: u32) -> image::RgbaImage {
    let mut img = image::RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            // 基础渐变：红 → 蓝
            let t = x as f32 / w as f32;
            let mut px = [((1.0 - t) * 220.0) as u8, 40, (t * 220.0) as u8, 255];
            // 顶部绿条：用来判断有没有上下翻转
            if y < h / 10 {
                px = [30, 200, 60, 255];
            }
            // 左侧黄条
            if x < w / 16 && y >= h / 10 {
                px = [240, 200, 40, 255];
            }
            // 左上角红块
            if x < w / 16 && y < h / 10 {
                px = [220, 30, 30, 255];
            }
            img.put_pixel(x, y, image::Rgba(px));
        }
    }
    img
}

/// 构造 32 位 BGRA 的 DIB。`top_down` 为真时 biHeight 取负数，行序不翻转。
fn build_dib(img: &image::RgbaImage, top_down: bool) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let mut out = Vec::with_capacity(40 + (w * h * 4) as usize);

    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    let height = if top_down { -(h as i32) } else { h as i32 };
    out.extend_from_slice(&height.to_le_bytes()); // biHeight
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(w * h * 4).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    let mut rows: Vec<Vec<u8>> = Vec::with_capacity(h as usize);
    for y in 0..h {
        let mut row = Vec::with_capacity((w * 4) as usize);
        for x in 0..w {
            let p = img.get_pixel(x, y);
            // RGBA → BGRA
            row.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
        rows.push(row);
    }
    // bottom-up 存储要求第一行是图像的最下面一行
    if !top_down {
        rows.reverse();
    }
    for row in rows {
        out.extend_from_slice(&row);
    }
    out
}

fn set_clipboard(fmt: u32, data: &[u8]) {
    unsafe {
        if OpenClipboard(ptr::null_mut()) == 0 {
            eprintln!("OpenClipboard 失败");
            return;
        }
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len());
        if mem.is_null() {
            CloseClipboard();
            eprintln!("GlobalAlloc 失败");
            return;
        }
        let dst = GlobalLock(mem);
        ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, data.len());
        GlobalUnlock(mem);

        EmptyClipboard();
        if SetClipboardData(fmt, mem as HANDLE).is_null() {
            eprintln!("SetClipboardData 失败 (fmt={fmt})");
        }
        CloseClipboard();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "png".into());
    let w: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(640);
    let h: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(400);
    let img = make_image(w, h);

    match mode.as_str() {
        "png" => {
            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .expect("PNG 编码失败");
            let name: Vec<u16> = "PNG\0".encode_utf16().collect();
            let fmt = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
            println!("推送 CF_PNG，{} 字节", png.len());
            set_clipboard(fmt, &png);
        }
        "dib-top" => {
            let dib = build_dib(&img, true);
            println!("推送 CF_DIB（biHeight 负，top-down），{} 字节", dib.len());
            set_clipboard(CF_DIB, &dib);
        }
        "dib-bottom" => {
            let dib = build_dib(&img, false);
            println!("推送 CF_DIB（biHeight 正，bottom-up），{} 字节", dib.len());
            set_clipboard(CF_DIB, &dib);
        }
        other => {
            eprintln!("未知模式 {other}，可选: png / dib-top / dib-bottom");
            std::process::exit(2);
        }
    }
}
