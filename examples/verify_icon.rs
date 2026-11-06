//! 验证 `assets/icon.ico` 能被 `CreateIconFromResourceEx` 正确解析，
//! 并把生成的 HICON 实际画出来导出成 PNG——这样能看出图标在托盘里的真实样子，
//! 而不是只看「创建成功」这个返回值。
//!
//! 用法：cargo run --release --example verify_icon

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, DI_NORMAL, DestroyIcon, DrawIconEx, LR_DEFAULTCOLOR,
};

const ICO: &[u8] = include_bytes!("../assets/icon.ico");

fn pick(ico: &[u8], want: u32) -> Option<(&[u8], u32)> {
    if ico.len() < 6 || ico[2] != 1 {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut best: Option<(u32, usize, usize)> = None;
    for i in 0..count {
        let e = 6 + i * 16;
        let dim = if ico[e] == 0 { 256 } else { ico[e] as u32 };
        let len = u32::from_le_bytes(ico[e + 8..e + 12].try_into().ok()?) as usize;
        let off = u32::from_le_bytes(ico[e + 12..e + 16].try_into().ok()?) as usize;
        if off + len > ico.len() {
            continue;
        }
        if best.is_none_or(|(bs, _, _)| dim.abs_diff(want) < bs.abs_diff(want)) {
            best = Some((dim, off, len));
        }
    }
    best.map(|(d, o, l)| (&ico[o..o + l], d))
}

/// 把 HICON 画进一块 32 位 DIB，返回 RGBA 像素。
fn render(hicon: *mut c_void, size: u32) -> Option<Vec<u8>> {
    unsafe {
        let screen = GetDC(ptr::null_mut());
        if screen.is_null() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                // 负数 = top-down，省得后面再翻行序
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut c_void = ptr::null_mut();
        let dib = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(mem);
            ReleaseDC(ptr::null_mut(), screen);
            return None;
        }

        let old = SelectObject(mem, dib);

        // 先刷成洋红，凡是没被图标覆盖的像素一眼就能看出来
        let px = bits as *mut u8;
        let n = (size * size) as usize;
        for i in 0..n {
            *px.add(i * 4) = 255;
            *px.add(i * 4 + 1) = 0;
            *px.add(i * 4 + 2) = 255;
            *px.add(i * 4 + 3) = 255;
        }

        let ok = DrawIconEx(
            mem,
            0,
            0,
            hicon,
            size as i32,
            size as i32,
            0,
            ptr::null_mut(),
            DI_NORMAL,
        );

        let mut rgba = Vec::with_capacity(n * 4);
        if ok != 0 {
            for i in 0..n {
                let b = *px.add(i * 4);
                let g = *px.add(i * 4 + 1);
                let r = *px.add(i * 4 + 2);
                let a = *px.add(i * 4 + 3);
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }

        SelectObject(mem, old);
        DeleteObject(dib);
        DeleteDC(mem);
        ReleaseDC(ptr::null_mut(), screen);

        if ok == 0 { None } else { Some(rgba) }
    }
}

fn main() {
    let want = 32;
    let Some((data, dim)) = pick(ICO, want) else {
        eprintln!("ICO 解析失败");
        std::process::exit(1);
    };
    println!("选中 {dim}px 条目，{} 字节", data.len());

    let hicon = unsafe {
        CreateIconFromResourceEx(
            data.as_ptr(),
            data.len() as u32,
            1,
            0x0003_0000,
            dim as i32,
            dim as i32,
            LR_DEFAULTCOLOR,
        )
    };
    println!("CreateIconFromResourceEx -> {:?}", hicon);
    if hicon.is_null() {
        std::process::exit(1);
    }

    // 用更大的画布渲染，方便肉眼检查
    let out_size = 128;
    let pixels = match render(hicon, out_size) {
        Some(p) => p,
        None => {
            eprintln!("DrawIconEx 渲染失败");
            std::process::exit(1);
        }
    };

    let img = image::RgbaImage::from_raw(out_size, out_size, pixels).expect("像素数量不对");
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("icon_rendered.png");
    img.save(&path).expect("保存失败");
    println!("已渲染到 {}", path.display());

    // 统计一下非洋红像素，确认图标真的画上去了
    let mut drawn = 0;
    for p in img.pixels() {
        if !(p[0] == 255 && p[1] == 0 && p[2] == 255) {
            drawn += 1;
        }
    }
    println!("非背景像素 {drawn} / {}", out_size * out_size);

    unsafe { DestroyIcon(hicon) };
}
