//! 抓一块屏幕的真实像素（BitBlt），写成 PNG。
//!
//! 和 `shot_window` 的区别：那个用 `PrintWindow`，拿到的是**应用自己重绘**
//! 的画面，看不见 DWM 叠在上面的东西（失焦灰化、亚克力材质、系统遮罩）。
//! 要判断「屏幕上到底长什么样」必须走屏幕抓取这条路。
//!
//! 用法：`screen_shot <输出.png> [x y w h]`，缺省抓主显示器全屏。

use windows_sys::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "screen.png".into());
    let nums: Vec<i32> = args.filter_map(|s| s.parse().ok()).collect();

    let (x, y, w, h) = if nums.len() >= 4 {
        (nums[0], nums[1], nums[2], nums[3])
    } else {
        unsafe { (0, 0, GetSystemMetricsSafe(0), GetSystemMetricsSafe(1)) }
    };

    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp as *mut _);

        // CAPTUREBLT：把分层窗口（含 DWM 合成的东西）一起抓进来。
        if BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY | CAPTUREBLT) == 0 {
            eprintln!("BitBlt 失败");
            return;
        }

        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // 负数 = 自上而下，省一次翻转
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..std::mem::zeroed()
        };

        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut _,
            &mut bi,
            DIB_RGB_COLORS,
        );
        if lines == 0 {
            eprintln!("GetDIBits 失败");
            return;
        }

        SelectObject(mem, old);
        DeleteObject(bmp as *mut _);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);

        // GDI 给的是 BGRA，`image` 要 RGBA。
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let img = image::RgbaImage::from_raw(w as u32, h as u32, buf).expect("尺寸对不上");
        img.save(&out).expect("写文件失败");
        println!("已保存 {out}（{w}x{h} @ {x},{y}）");
    }
}

/// `SM_CXSCREEN` = 0、`SM_CYSCREEN` = 1。直接写数字，省得再引一个 feature。
unsafe fn GetSystemMetricsSafe(index: i32) -> i32 {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(index) }
}
