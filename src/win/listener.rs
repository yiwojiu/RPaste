//! 剪贴板事件监听 + 全局热键 + 托盘图标。
//!
//! 用 `AddClipboardFormatListener` 而不是轮询：事件驱动、零 CPU 空转、不会漏事件。
//! 窗口是隐藏的顶层窗口（不是 `HWND_MESSAGE`）——托盘图标要求窗口能接收 shell
//! 回调，隐藏顶层窗口比 message-only 窗口更稳妥。
//!
//! 回调里只做「投递」，绝不打开剪贴板：
//! - `WM_CLIPBOARDUPDATE` → 取序列号 → `seq_tx`
//! - `WM_HOTKEY`（默认 `Alt+V`，可配置）→ 切换主窗口可见性
//! - `TRAY_MSG`（托盘）→ `event_tx` 发 `Toggle` / `Quit`
//!
//! 两个 channel 容量都是 1：消费方没来得及取走时直接丢弃。

use std::ptr;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, GetClipboardSequenceNumber, RemoveClipboardFormatListener,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
    PostQuitMessage, RegisterClassW, TranslateMessage, WNDCLASSW,
};

use super::{hotkey, tray, window};

const WM_CLIPBOARDUPDATE: u32 = 0x031D;
const WM_HOTKEY: u32 = 0x0312;
const WM_DESTROY: u32 = 0x0002;

/// 退出时用来通知 worker 收尾。
static CMD_TX: OnceLock<crossbeam_channel::Sender<crate::capture::Command>> = OnceLock::new();

/// 注册退出用的命令通道。必须在 `spawn` 之后、用户点退出之前调用。
pub fn set_cmd_tx(tx: crossbeam_channel::Sender<crate::capture::Command>) {
    let _ = CMD_TX.set(tx);
}

static SEQ_TX: OnceLock<SyncSender<crate::capture::ClipEvent>> = OnceLock::new();

/// 启动监听线程，返回剪贴板变更流。
///
/// 热键注册失败（例如被占用）不会让程序退出，只是打印警告：
/// 剪贴板捕获照常工作，只是没有快捷键呼出。
pub fn spawn() -> Receiver<crate::capture::ClipEvent> {
    let (seq_tx, seq_rx) = sync_channel(1);
    let _ = SEQ_TX.set(seq_tx);

    std::thread::Builder::new()
        .name("rpaste-listener".into())
        .spawn(|| {
            if let Err(err) = run() {
                crate::log_error!("监听线程异常退出: {err:#}");
            }
        })
        .expect("无法创建监听线程");

    seq_rx
}

/// 退出：先让 worker 把 WAL 收干净，再拆掉系统级注册，最后结束进程。
///
/// 不走 eframe 的 `ViewportCommand::Close`——窗口可能正处于隐藏状态，
/// 那时 UI 的 `update` 根本不执行，也没人能处理关闭请求。
fn request_quit(hwnd: HWND) {
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.send(crate::capture::Command::Shutdown);
    }
    // 等 worker 把 WAL 合并回主库再退出。固定睡一个时长不可靠：
    // 库大了合并要几百毫秒，睡短了数据还没落盘就被 exit 掉了。
    for _ in 0..200 {
        if crate::capture::WORKER_DONE.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // `process::exit` 直接终止进程，消息循环里后续的 `WM_DESTROY` 也不会再
    // 派发，所以托盘图标、热键、剪贴板监听必须在这里显式注销：
    // 漏掉托盘图标会在通知区留下一个要鼠标划过才消失的僵尸图标，
    // 漏掉热键会让系统以为组合键仍被占用。
    unsafe {
        RemoveClipboardFormatListener(hwnd);
        tray::remove(hwnd);
        hotkey::unregister(hwnd);
        // 真正销毁窗口，顺带让 `WM_DESTROY` 的清理跑一遍（重复调用无害）。
        DestroyWindow(hwnd);
    }

    std::process::exit(0);
}

fn run() -> anyhow::Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(ptr::null());
        if hinst.is_null() {
            anyhow::bail!("GetModuleHandleW 失败");
        }

        let class_name: Vec<u16> = "RPasteClipboardListener\0".encode_utf16().collect();

        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class_name.as_ptr();
        if RegisterClassW(&wc) == 0 {
            anyhow::bail!("RegisterClassW 失败");
        }

        // 不带 WS_VISIBLE 就是隐藏窗口；parent 传 NULL 成为顶层窗口，
        // 这样托盘图标和热键回调都能可靠收到。
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            hinst,
            ptr::null(),
        );
        if hwnd.is_null() {
            anyhow::bail!("CreateWindowExW 失败");
        }

        // 让读取线程能用这个窗口作为 OpenClipboard 的 owner。
        crate::win::clipboard::set_owner(hwnd);

        // 记下句柄：UI 改完热键后会往这个窗口投递重注册请求。
        hotkey::set_listener(hwnd);

        if AddClipboardFormatListener(hwnd) == 0 {
            anyhow::bail!("AddClipboardFormatListener 失败");
        }

        if let Err(err) = hotkey::register(hwnd) {
            crate::log_warn!("热键 {err}，剪贴板捕获仍正常运行");
        }

        // Win+V 被 Shell 独占，`RegisterHotKey` 抢不到，接管要靠低级键盘钩子。
        // 那条路实测能拦到按键，但存在「第一次按不响应」的时序问题，暂时搁置，
        // 只用 `Alt+V`（见 `hook` 模块，代码保留待后续修）。默认组合可以在
        // 设置面板里改，改完由 `WM_RELOAD_HOTKEY` 回到这个线程重新注册。

        if tray::add(hwnd) {
            crate::log_info!("托盘图标已就绪");
        } else {
            crate::log_warn!("托盘图标添加失败，不影响主功能");
        }

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let ret = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);
            if ret == 0 {
                break;
            }
            if ret == -1 {
                anyhow::bail!("GetMessageW 失败");
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            let seq = unsafe { GetClipboardSequenceNumber() };
            if let Some(tx) = SEQ_TX.get() {
                // 来源必须在这里抓：此刻前台还停在用户按下 Ctrl+C 的那个程序上。
                // 等事件转到 worker 再问就不准了——中间可能切了窗口。
                let app = super::appinfo::foreground_app();
                let _ = tx.try_send(crate::capture::ClipEvent { seq, app });
            }
            0
        }
        WM_HOTKEY => {
            // 录制新组合时不要切窗口：那一刻按下的很可能正是当前生效的热键，
            // 系统已经把它从键盘上吞掉了，再顺手把窗口收起就完全没法操作了。
            if w as i32 == hotkey::ID && !hotkey::is_capturing() {
                // 直接调 Win32 切换可见性，不经过 UI 线程：
                // 窗口隐藏时 UI 的 update 不会执行，事件递不过去。
                window::toggle_main_window();
            }
            0
        }
        hotkey::WM_RELOAD_HOTKEY => {
            // 只能在**注册它的线程**上撤/注册，所以改键必须绕回这里。
            // 失败时 `apply` 内部已经回滚到旧组合，这里只负责记一笔。
            match unsafe { hotkey::reload(hwnd) } {
                Ok(()) => crate::log_info!("热键已切换为 {}", hotkey::Hotkey::load().label()),
                Err(err) => crate::log_warn!("{err}，沿用原热键"),
            }
            0
        }
        tray::TRAY_MSG => {
            match tray::on_mouse(hwnd, l) {
                tray::TrayAction::Toggle => window::toggle_main_window(),
                tray::TrayAction::Settings => {
                    // 顺序不能反：先把「要开设置」记下来，再唤出窗口。
                    // 反过来的话，ShowWindow 触发的那一帧可能先跑完整个 update，
                    // 等标志位落下去已经没有下一帧来消费它了。
                    crate::ui::request_settings();
                    window::show_at_cursor_keep_target();
                    crate::log_info!("托盘：打开设置");
                }
                tray::TrayAction::ToggleAutostart => match super::autostart::toggle() {
                    Ok(on) => crate::log_info!("开机自启已{}", if on { "开启" } else { "关闭" }),
                    Err(err) => crate::log_error!("切换开机自启失败: {err}"),
                },
                tray::TrayAction::Quit => request_quit(hwnd),
                tray::TrayAction::None => {}
            }
            0
        }
        WM_DESTROY => {
            unsafe {
                RemoveClipboardFormatListener(hwnd);
                tray::remove(hwnd);
                hotkey::unregister(hwnd);
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}
