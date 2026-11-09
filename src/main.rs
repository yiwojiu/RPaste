//! R-Paste · Windows 剪贴板历史工具
//!
//! 线程模型（M1）：
//! - `rpaste-listener`：message-only 窗口 + `AddClipboardFormatListener`，只投递序列号
//! - `rpaste-dispatch`：把序列号转发成 worker 命令
//! - `rpaste-worker`：读剪贴板 → 哈希 → 落库，持有写连接
//! - 主线程：eframe 事件循环，持有只读连接
//!
//! 数据流向：listener →(seq)→ worker →(落库)→ 提示 → UI 重查。

// GUI 子系统：双击 exe 时 Windows 不再额外创建一个控制台窗口。
// 只作用于这个 bin target；examples/ 下的测试工具仍是普通控制台程序。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::path::PathBuf;

use crossbeam_channel::{bounded, unbounded};
use eframe::egui;

use rpaste::{capture, store, ui, win};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // GUI 子系统下 stderr 是无效句柄，返回 Err 时 Rust 打的那行错误用户看不到。
    // 包一层把失败原因写进日志文件，双击启动失败时不至于毫无线索。
    if let Err(e) = run() {
        rpaste::log_error!("启动失败: {e:#}");
    }
}

fn run() -> anyhow::Result<()> {
    let root = data_dir();
    std::fs::create_dir_all(&root)?;
    rpaste::log::init(&root);
    rpaste::log_info!("===== R-Paste 启动 =====");

    // 单实例：已经有进程在跑就把它的窗口叫出来，自己退出。
    // 两个实例会各自监听剪贴板，把同一次复制记两遍。
    if !win::single::acquire() {
        rpaste::log_info!("已有实例在运行，唤出它的窗口后退出");
        win::show_at_cursor();
        return Ok(());
    }

    let store = store::Store::open(&root)?;
    let read = store.read_only()?;

    // worker 与 UI 之间的命令 / 通知通道。
    let (cmd_tx, cmd_rx) = unbounded::<capture::Command>();
    let (notify_tx, notify_rx) = bounded::<()>(1);

    // 1. 监听线程：序列号 → worker 命令通道。
    // 热键和托盘的显隐由 listener 线程直接用 Win32 处理，不走 UI 线程——
    // 窗口隐藏时 egui 的 update 不会执行，事件递过去也没人接。
    let events = win::listener::spawn();
    let cmd_tx_listener = cmd_tx.clone();
    let notify_tx_listener = notify_tx.clone();
    std::thread::Builder::new()
        .name("rpaste-dispatch".into())
        .spawn(move || {
            for ev in events {
                if cmd_tx_listener.send(capture::Command::Seq(ev)).is_err() {
                    break;
                }
                // 提示 UI 有新内容。容量 1，UI 每帧合并消费即可。
                let _ = notify_tx_listener.try_send(());
            }
        })
        .expect("无法创建分发线程");

    // 2. worker：持有写连接，处理捕获、写回、删除、收藏。
    let notify_tx_worker = notify_tx.clone();
    let notify_tx_capture = notify_tx.clone();
    std::thread::Builder::new()
        .name("rpaste-worker".into())
        .spawn(move || {
            capture::run(cmd_rx, store, notify_tx_capture);
            let _ = notify_tx_worker.try_send(());
        })
        .expect("无法创建 worker 线程");

    // 托盘「退出」要用它通知 worker 收尾。
    win::listener::set_cmd_tx(cmd_tx.clone());

    // 3. UI：主线程。
    // 窗口图标和托盘图标同源，避免任务栏和通知区长得不一样。
    let window_icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
        .map_err(|e| anyhow::anyhow!("图标解码失败: {e}"))?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 560.0])
            .with_min_inner_size([320.0, 320.0])
            .with_title("R-Paste")
            // 透明帧缓冲：底下的系统材质（亚克力）要能透上来。面板自己画
            // 半透明底色，所以文字对比度仍然由我们控制，不依赖壁纸。
            .with_transparent(true)
            .with_icon(window_icon),
        ..Default::default()
    };

    // 窗口收进托盘后 eframe 的 ControlFlow 会卡在 Poll 空转，见 ui::EGUI_CTX。
    ui::spawn_repaint_guard();

    eframe::run_native(
        "R-Paste",
        options,
        Box::new(move |cc| {
            ui::App::install_cjk_font(&cc.egui_ctx);
            rpaste::theme::install(&cc.egui_ctx);
            // 窗口此刻已经建好（`CreationContext` 在窗口之后回调），套上系统材质。
            rpaste::win::apply_backdrop();
            let _ = ui::EGUI_CTX.set(cc.egui_ctx.clone());
            Ok(Box::new(ui::App::new(
                &cc.egui_ctx,
                read,
                cmd_tx,
                notify_rx,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe 启动失败: {e}"))?;

    Ok(())
}

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("RPaste")
}
