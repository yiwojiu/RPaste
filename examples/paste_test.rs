//! 端到端验证「双击直接粘贴到光标处」。
//!
//! 记事本在 Win11 上是 Store 应用，PID 找不准，所以这里自己造一个靶子：
//! 一个带 EDIT 控件的普通顶层窗口，跑真实消息循环，模拟用户正在打字的输入框。
//!
//! 流程：靶子在前台 → 弹 R-Paste（记录前台上是靶子）→ 写剪贴板
//! → 隐藏 R-Paste → 切回靶子发 Ctrl+V → 读 EDIT 内容验证。
//!
//! 用法：cargo run --release --example paste_test

use std::mem::zeroed;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, ES_AUTOVSCROLL, ES_MULTILINE, GetMessageW,
    GetWindowTextW, MSG, PostMessageW, RegisterClassW, SetForegroundWindow, TranslateMessage,
    WM_CLOSE, WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use rpaste::win;

const MARKER: &str = "R-Paste 自动粘贴测试 OK";

/// EDIT 子控件句柄。窗口线程创建、主线程读取，用原子变量传。
static EDIT_HWND: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        // WM_CREATE：建一个多行编辑框并给它焦点
        0x0001 => {
            let edit = unsafe {
                CreateWindowExW(
                    0,
                    wide("EDIT").as_ptr(),
                    wide("").as_ptr(),
                    WS_CHILD | WS_VISIBLE | (ES_MULTILINE as u32) | (ES_AUTOVSCROLL as u32),
                    10,
                    10,
                    560,
                    360,
                    hwnd,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            unsafe {
                EDIT_HWND.store(edit, Ordering::SeqCst);
                SetFocus(edit);
            }
            0
        }
        // WM_SETFOCUS：窗口自己被激活时，把焦点转给编辑框。
        // 少了这一步，SetForegroundWindow 之后焦点停在顶层窗口上，
        // Ctrl+V 就没有接收者。
        0x0007 => {
            let edit = EDIT_HWND.load(Ordering::SeqCst);
            if !edit.is_null() {
                unsafe { SetFocus(edit) };
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// 在独立线程里创建靶子窗口并跑消息循环，返回 (顶层窗口, 编辑框)。
///
/// HWND 是裸指针、不是 `Send`，所以过 channel 时转成 usize 再转回来。
fn spawn_target() -> Option<(HWND, HWND)> {
    let (tx, rx) = mpsc::channel::<(usize, usize)>();

    std::thread::spawn(move || unsafe {
        let hinst = GetModuleHandleW(ptr::null());
        let class = wide("RPastePasteTarget");
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("R-Paste 粘贴靶子").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            700,
            300,
            600,
            420,
            ptr::null_mut(),
            ptr::null_mut(),
            hinst,
            ptr::null(),
        );

        let _ = tx.send((hwnd as usize, EDIT_HWND.load(Ordering::SeqCst) as usize));

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });

    rx.recv_timeout(Duration::from_secs(3))
        .ok()
        .map(|(t, e)| (t as HWND, e as HWND))
}

fn edit_text(hwnd: HWND) -> String {
    unsafe {
        let mut buf = vec![0u16; 1024];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        buf.truncate(n.max(0) as usize);
        String::from_utf16_lossy(&buf)
    }
}

fn main() {
    let Some((target, edit)) = spawn_target() else {
        eprintln!("靶子窗口创建失败");
        std::process::exit(1);
    };
    println!("靶子 HWND = {target:?}  EDIT = {edit:?}");
    std::thread::sleep(Duration::from_millis(600));

    // 让靶子窗口在前台
    unsafe {
        SetForegroundWindow(target);
    }
    std::thread::sleep(Duration::from_millis(500));
    println!("已切到靶子前台，编辑框内容初始 = {:?}", edit_text(edit));

    // 弹 R-Paste：这一步会把靶子记成「前一个前台窗口」
    win::show_at_cursor();
    std::thread::sleep(Duration::from_millis(400));

    // 写剪贴板（模拟用户双击条目时 worker 做的事）
    let ok = win::clipboard::write_text(MARKER);
    println!("写剪贴板 -> {ok}");
    std::thread::sleep(Duration::from_millis(200));

    // 直接调粘贴：内部会先还焦点给靶子，再隐藏我们的窗口，最后发 Ctrl+V。
    // 不要在外面预先 hide——那会丢掉前台权限。
    let pasted = win::paste_to_previous();
    println!("paste_to_previous -> {pasted}");

    std::thread::sleep(Duration::from_millis(700));

    let got = edit_text(edit);
    println!("编辑框内容 = {got:?}");

    let success = got.contains(MARKER);
    println!(
        "\n结果：{}",
        if success {
            "✓ 内容成功插入到靶子的输入框"
        } else {
            "✗ 编辑框没收到内容"
        }
    );

    unsafe {
        PostMessageW(target, WM_CLOSE, 0, 0);
    }

    std::process::exit(if success { 0 } else { 1 });
}
