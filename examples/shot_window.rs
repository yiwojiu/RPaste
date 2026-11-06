//! 直接抓主窗口的画面，不依赖窗口是否在前台、有没有被遮挡。
//!
//! `PrintWindow` + `PW_RENDERFULLCONTENT` 能让 DWM 把窗口内容画到我们的 DC 上，
//! 比截屏可靠——截屏会拍到挡在上面的浏览器，还有抢不到前台的问题。
//!
//! 用法：cargo run --release --example shot_window -- out.png

use std::mem::zeroed;
use std::ptr;

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;

use rpaste::win::window;

/// 让 DWM 渲染完整内容（含 GPU 合成部分），否则 OpenGL 渲染的窗口会抓成黑屏。
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "window_shot.png".to_string());

    let Some(hwnd) = window::main_window() else {
        eprintln!("找不到 R-Paste 窗口，程序没在运行？");
        std::process::exit(1);
    };

    // 隐藏状态下 PrintWindow 抓不到内容，先显示出来（不动位置）
    window::show_main_window();
    std::thread::sleep(std::time::Duration::from_millis(400));

    unsafe {
        let mut rect = zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            eprintln!("GetWindowRect 失败");
            std::process::exit(1);
        }
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w <= 0 || h <= 0 {
            eprintln!("窗口尺寸异常 {w}x{h}");
            std::process::exit(1);
        }

        let screen = GetDC(ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);

        let ok = PrintWindow(hwnd, mem, PW_RENDERFULLCONTENT);
        println!("PrintWindow -> {ok}，窗口 {w}x{h}");

        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // 负数 = top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..zeroed()
        };

        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut _,
            &mut bmi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(ptr::null_mut(), screen);

        if lines == 0 {
            eprintln!("GetDIBits 失败");
            std::process::exit(1);
        }

        // BGRA → RGBA，同时把全透明像素补成不透明，否则保存出来是黑底
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            if px[3] == 0 {
                px[3] = 255;
            }
        }

        let img = image::RgbaImage::from_raw(w as u32, h as u32, buf).expect("像素数量不符");
        img.save(&out).expect("保存失败");
        println!("已保存 {out}");
    }
}
