//! Win32 剪贴板读写。
//!
//! 这里的每个函数都可能阻塞，**只允许在捕获线程里调用**。
//! `GetClipboardData` 遇到延迟渲染（`SetClipboardData(fmt, NULL)` 留下的承诺）时，
//! 会同步等待源程序现场生成数据，系统上限 30 秒。把它放在消息线程或 UI 线程上，
//! 整个程序就会跟着卡住。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::UI::Shell::DragQueryFileW;

use crate::model::{
    Captured, Kind, MAX_PAYLOAD, PREVIEW_CHARS, TAIL_CHARS, make_preview, make_preview_and_tail,
    normalize_newlines,
};

pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_HDROP: u32 = 15;
pub const CF_DIBV5: u32 = 17;

/// DIB 转 PNG 时的中间内存上限。4K RGBA 的 DIB 约 33MB，
/// 解码成位图再编码还要一份，留足余量但挡住异常数据。
const MAX_DIB_BYTES: usize = 96 * 1024 * 1024;

const OPEN_RETRIES: u32 = 8;
const OPEN_RETRY_MS: u64 = 2;

/// 监听线程的 message-only 窗口句柄。`OpenClipboard` 传具体窗口，
/// 而不是 NULL —— 传 NULL 会把剪贴板关联到「当前任务」，
/// 同进程其他线程后续就打不开了。
static OWNER: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

pub fn set_owner(hwnd: HWND) {
    OWNER.store(hwnd, Ordering::Relaxed);
}

fn owner() -> HWND {
    OWNER.load(Ordering::Relaxed)
}

/// RAII 保证 `CloseClipboard` 一定被调用。
/// 不关闭的话，整个系统所有程序的粘贴都会失败——这是剪贴板工具最容易造成的灾难。
struct Guard;

impl Guard {
    fn acquire() -> Option<Self> {
        for attempt in 0..OPEN_RETRIES {
            if unsafe { OpenClipboard(owner()) } != 0 {
                return Some(Guard);
            }
            if attempt + 1 < OPEN_RETRIES {
                std::thread::sleep(Duration::from_millis(OPEN_RETRY_MS));
            }
        }
        None
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

/// 读取当前剪贴板内容。
///
/// 返回 `None` 的情况：剪贴板被占用、内容为空、或对方带隐私标记。
pub fn read() -> Option<Captured> {
    // 剪贴板只在「拷原始数据」这一小段持有，转码和哈希放到关掉之后。
    //
    // 实测 4K 截图的 DIB→PNG 在 release 下要 150ms，debug 下更久；一直占着
    // 剪贴板，系统里其他程序这段时间全粘不上东西——实测会打出
    // 「OpenClipboard 失败」，那已经不是「少记一条」，而是干扰别人干活了。
    let raw = {
        let _guard = match Guard::acquire() {
            Some(g) => g,
            None => {
                // 别的程序（或自己上一轮没收尾）占着剪贴板。这是「复制了但没记下来」
                // 最常见的元凶，必须留下线索。
                crate::log_error!("OpenClipboard 失败，重试 {OPEN_RETRIES} 次仍未拿到剪贴板");
                return None;
            }
        };

        if is_monitoring_opted_out() {
            return None;
        }

        read_raw()?
    };

    raw.finish()
}

/// 锁内读到的原始内容，还没转成 `Captured`。
///
/// 拆这一层是为了把「必须持有剪贴板」的部分压到最短：只有判断格式、
/// 拷字节、读文本三步需要锁，哈希、预览、DIB→PNG 都是纯 CPU，锁外算。
enum Raw {
    /// 换行分隔的绝对路径。
    Files(Vec<String>),
    /// 已经做过换行归一化的文本。
    Text(String),
    /// 已经是 PNG 字节（源程序直接给了 `PNG` 格式）。
    Png(Vec<u8>),
    /// 原始 DIB，还要现场转码。
    Dib(Vec<u8>),
}

impl Raw {
    fn finish(self) -> Option<Captured> {
        match self {
            Raw::Files(paths) => {
                // 预览要能当搜索索引：多选文件时把每个文件名都列出来，否则
                // 「选中 5 个文件」的条目只能搜到第一个，剩下的等于不存在。
                // 上限 20 个名——再长就截断，preview 本身也有字符上限。
                let preview = if paths.len() == 1 {
                    file_name_of(&paths[0])
                } else {
                    let names: Vec<String> =
                        paths.iter().take(20).map(|p| file_name_of(p)).collect();
                    let tail = if paths.len() > names.len() {
                        " 等"
                    } else {
                        ""
                    };
                    make_preview(
                        &format!("{} 个文件：{}{tail}", paths.len(), names.join("、")),
                        PREVIEW_CHARS,
                    )
                };
                let payload = paths.join("\n").into_bytes();
                Some(Captured::new(Kind::Files, payload, preview))
            }
            Raw::Text(text) => {
                // 尾部单独留一份：剪贴板里真正长的东西（配置、日志、整段 SQL）
                // 只有前 512 字能被搜到是绝对不够的。
                let (preview, tail) = make_preview_and_tail(&text, PREVIEW_CHARS, TAIL_CHARS);
                Some(Captured::new(Kind::Text, text.into_bytes(), preview).with_tail(tail))
            }
            Raw::Png(bytes) => {
                let (w, h) = png_size(&bytes)?;
                // 预览只留尺寸：体积已经显示在元数据行，重复一遍是噪音。
                Some(Captured::new(Kind::Image, bytes, format!("图片 {w}×{h}")))
            }
            Raw::Dib(dib) => {
                let png = dib_to_png(&dib)?;
                let (w, h) = png_size(&png)?;
                Some(Captured::new(Kind::Image, png, format!("图片 {w}×{h}")))
            }
        }
    }
}

/// 格式优先级是有讲究的：
/// 1. CF_HDROP 最高——存在就几乎一定是文件复制，此时剪贴板里往往还有一份
///    文件名的纯文本，不能让它抢先生效。
/// 2. 文本次之——Excel 复制单元格会同时塞一张渲染图，用户想要的是文本。
/// 3. 图片最后——截图工具只有图片，没有文本，自然落到这里。
fn read_raw() -> Option<Raw> {
    if let Some(paths) = read_files() {
        return Some(Raw::Files(paths));
    }
    if let Some(text) = read_text() {
        return Some(Raw::Text(text));
    }
    read_image()
}

fn read_text() -> Option<String> {
    let text = read_wide_text(CF_UNICODETEXT)?;
    let normalized = normalize_newlines(&text);
    if normalized.trim().is_empty() {
        return None;
    }
    Some(normalized)
}

/// 读取 CF_HDROP（资源管理器里复制的文件/文件夹）。
///
/// 返回绝对路径列表（存库时拼成换行分隔的一列），预览在锁外拼。
fn read_files() -> Option<Vec<String>> {
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP) == 0 {
            return None;
        }
        let hdrop = GetClipboardData(CF_HDROP);
        if hdrop.is_null() {
            return None;
        }

        // ifile = 0xFFFFFFFF 时返回文件总数。
        let count = DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0);
        if count == 0 {
            return None;
        }

        let mut paths: Vec<String> = Vec::with_capacity(count as usize);
        for index in 0..count {
            let len = DragQueryFileW(hdrop, index, std::ptr::null_mut(), 0);
            if len == 0 {
                continue;
            }
            // len 不含结尾 NUL，多留一格。
            let mut buf = vec![0u16; len as usize + 1];
            let written = DragQueryFileW(hdrop, index, buf.as_mut_ptr(), buf.len() as u32);
            if written == 0 {
                continue;
            }
            buf.truncate(written as usize);
            paths.push(String::from_utf16_lossy(&buf));
        }

        if paths.is_empty() { None } else { Some(paths) }
    }
}

fn file_name_of(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_owned()
}

/// 读取图片。优先 `PNG` 格式（浏览器/现代应用会给，已压缩且带 alpha），
/// 否则交出 `CF_DIBV5` / `CF_DIB` 的原始字节，转码在锁外做。
fn read_image() -> Option<Raw> {
    unsafe {
        let png_fmt = format_id(&FMT_PNG, "PNG");
        // 用 let-chain 一次判完：格式已注册、剪贴板里有、字节像 PNG、能读出尺寸。
        // 任何一步不成立都直接落到下面的 DIB 兜底。
        if png_fmt != 0
            && IsClipboardFormatAvailable(png_fmt) != 0
            && let Some(bytes) = read_blob(png_fmt)
            && bytes.len() > 8
            && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
            && png_size(&bytes).is_some()
        {
            return Some(Raw::Png(bytes));
        }

        for fmt in [CF_DIBV5, CF_DIB] {
            if IsClipboardFormatAvailable(fmt) == 0 {
                continue;
            }
            let Some(dib) = read_blob(fmt) else { continue };
            if dib.len() > MAX_DIB_BYTES {
                continue;
            }
            return Some(Raw::Dib(dib));
        }
    }
    None
}

/// 复制一份剪贴板格式的原始字节。`GlobalSize` 给的是分配大小，可能大于实际数据。
fn read_blob(fmt: u32) -> Option<Vec<u8>> {
    unsafe {
        let handle = GetClipboardData(fmt);
        if handle.is_null() {
            return None;
        }
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            return None;
        }
        let size = GlobalSize(handle).min(MAX_PAYLOAD);
        let slice = std::slice::from_raw_parts(ptr as *const u8, size);
        let out = slice.to_vec();
        GlobalUnlock(handle);
        if out.is_empty() { None } else { Some(out) }
    }
}

/// 从 PNG 的 IHDR 直接读尺寸，不触发全图解码。
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

/// DIB 转 PNG。
///
/// DIB 就是去掉 14 字节 BMP 文件头的位图，所以补一个头交给 `image` 的解码器，
/// 由它处理位深、行对齐、`biHeight` 正负（负值表示 top-down 存储）这些坑，
/// 比自己解析像素靠谱。
fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let header_size = u32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
    if header_size < 40 || header_size > dib.len() {
        return None;
    }

    let pixel_offset = 14 + header_size;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0u8; 4]); // 保留字段
    bmp.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);

    let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).ok()?;

    // 用 Fast 压缩等级：剪贴板图片的诉求是「复制完立刻能用」，
    // 不是「存成最小体积」。默认等级在这张 640×400 图上要 200ms+，
    // 而 worker 的读取超时是 400ms，4K 图会直接超时被丢弃。
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut out);
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut cursor,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    );
    use image::ImageEncoder;
    encoder
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

fn read_wide_text(fmt: u32) -> Option<String> {
    unsafe {
        if IsClipboardFormatAvailable(fmt) == 0 {
            return None;
        }
        let handle = GetClipboardData(fmt);
        if handle.is_null() {
            return None;
        }
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            return None;
        }

        let max_chars = (GlobalSize(handle) / 2).min(MAX_PAYLOAD / 2);
        let src = ptr as *const u16;
        let mut buf: Vec<u16> = Vec::with_capacity(max_chars.min(8192));
        for i in 0..max_chars {
            let unit = *src.add(i);
            if unit == 0 {
                break;
            }
            buf.push(unit);
        }
        GlobalUnlock(handle);

        if buf.is_empty() {
            None
        } else {
            // 剪贴板里可能有孤立代理项（比如 emoji 被上游截断），
            // 用 lossy 而不是直接失败。
            Some(String::from_utf16_lossy(&buf))
        }
    }
}

/// 隐私标记。密码管理器（1Password / Bitwarden）和 Windows 自家的 Win+V
/// 都靠这两个格式退出记录，不遵守就是越界。
fn is_monitoring_opted_out() -> bool {
    let exclude = format_id(&FMT_EXCLUDE, "ExcludeClipboardContentFromMonitorProcessing");
    if exclude != 0 && unsafe { IsClipboardFormatAvailable(exclude) } != 0 {
        return true;
    }

    let can_include = format_id(&FMT_CAN_INCLUDE, "CanIncludeInClipboardHistory");
    if can_include != 0 && unsafe { IsClipboardFormatAvailable(can_include) } != 0 {
        return read_dword(can_include) == Some(0);
    }

    false
}

static FMT_EXCLUDE: OnceLock<u32> = OnceLock::new();
static FMT_CAN_INCLUDE: OnceLock<u32> = OnceLock::new();
static FMT_PNG: OnceLock<u32> = OnceLock::new();

/// 格式 ID 注册一次就够，避免每次事件都分配一个宽字符串。
fn format_id(cache: &'static OnceLock<u32>, name: &str) -> u32 {
    *cache.get_or_init(|| {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
    })
}

fn read_dword(fmt: u32) -> Option<u32> {
    unsafe {
        let handle = GetClipboardData(fmt);
        if handle.is_null() {
            return None;
        }
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            return None;
        }
        let size = GlobalSize(handle);
        let value = if size >= 4 {
            Some(*(ptr as *const u32))
        } else {
            None
        };
        GlobalUnlock(handle);
        value
    }
}

/// 把一块数据挂到一个剪贴板格式上。调用前必须先 `EmptyClipboard`。
unsafe fn put(fmt: u32, data: &[u8]) -> bool {
    use windows_sys::Win32::System::DataExchange::SetClipboardData;
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalUnlock as Unlock};

    unsafe {
        // 零长度也按 1 字节申请：`GlobalAlloc(0)` 返回的是未定义语义的块。
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1));
        if mem.is_null() {
            return false;
        }
        let dst = GlobalLock(mem);
        if dst.is_null() {
            // 没锁住，句柄还在我们手上，必须自己还给系统。
            GlobalFree(mem);
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, data.len());
        Unlock(mem);

        // 只有 `SetClipboardData` 成功，所有权才移交给剪贴板；
        // 失败时句柄仍归调用方，必须 `GlobalFree`。
        // 早先这里直接返回结果，失败一次就漏一块 HGLOBAL（单次最大 96 MB）。
        if SetClipboardData(fmt, mem).is_null() {
            GlobalFree(mem);
            return false;
        }
        true
    }
}

/// 当前剪贴板序列号。
///
/// 不需要 `OpenClipboard`，可以在任意线程调用，开销接近零。
/// 写回剪贴板时用它掐出「这次写入造成了哪几个 seq」，用于精确抑制自激。
pub fn sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// 写回文本。写完返回 `true`，调用方据此登记「下一个变更是我自己造成的」。
pub fn write_text(text: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::EmptyClipboard;

    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();

    let Some(_guard) = Guard::acquire() else {
        return false;
    };

    unsafe {
        if EmptyClipboard() == 0 {
            return false;
        }
        put(CF_UNICODETEXT, &bytes)
    }
}

/// 写回图片。
///
/// 同时挂 `PNG` 和 `CF_DIB` 两个格式：PNG 保留 alpha、体积小，现代应用优先吃它；
/// DIB 是 Windows 上的通用底线，老程序（画图、部分 Office 版本）只认这个。
pub fn write_image(png: &[u8]) -> bool {
    use windows_sys::Win32::System::DataExchange::EmptyClipboard;

    let Some(dib) = png_to_dib(png) else {
        return false;
    };

    let Some(_guard) = Guard::acquire() else {
        return false;
    };

    unsafe {
        if EmptyClipboard() == 0 {
            return false;
        }
        let png_fmt = format_id(&FMT_PNG, "PNG");
        let mut ok = false;
        if png_fmt != 0 {
            ok |= put(png_fmt, png);
        }
        ok |= put(CF_DIB, &dib);
        ok
    }
}

/// 写回文件列表（`CF_HDROP`）。
pub fn write_files(paths: &[String]) -> bool {
    use windows_sys::Win32::System::DataExchange::EmptyClipboard;

    if paths.is_empty() {
        return false;
    }
    let data = build_dropfiles(paths);

    let Some(_guard) = Guard::acquire() else {
        return false;
    };

    unsafe {
        if EmptyClipboard() == 0 {
            return false;
        }
        put(CF_HDROP, &data)
    }
}

/// PNG 解码成 32 位 BGRA 的 DIB。
///
/// `biHeight` 取正数表示 bottom-up，所以像素行要从最后一行往上写。
/// 这类「差一个符号就上下颠倒」的细节只能靠对着规范写，没有捷径。
fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .ok()?
        .to_rgba8();
    let (w, h) = img.dimensions();

    let mut out = Vec::with_capacity(40 + (w as usize * h as usize * 4));
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight > 0 → bottom-up
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(w * h * 4).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    for y in (0..h).rev() {
        for x in 0..w {
            let p = img.get_pixel(x, y);
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    Some(out)
}

/// `DROPFILES` 结构 + 双 NUL 结尾的宽字符路径列表。
fn build_dropfiles(paths: &[String]) -> Vec<u8> {
    // DROPFILES 固定 20 字节：pFiles(4) + pt(8) + fNC(4) + fWide(4)
    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(&20u32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&1i32.to_le_bytes()); // fWide = TRUE

    for p in paths {
        for unit in p.encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        data.extend_from_slice(&0u16.to_le_bytes());
    }
    data.extend_from_slice(&0u16.to_le_bytes());
    data
}
