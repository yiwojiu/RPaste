//! 捕获 worker。
//!
//! 这是一个独立常驻线程：从监听线程拿到序列号，去读剪贴板，再落库。
//! 读取动作本身再丢进一次性线程 + `recv_timeout`，这样即使某个源程序的
//! 延迟渲染卡死（系统上限 30 秒），worker 也能在 400ms 后放弃并继续，
//! 不会把整条流水线堵死。
//!
//! 与 UI 的通信走 crossbeam-channel：
//! - `Command::Seq(u32)`：监听线程 → worker，触发一次捕获
//! - `Command::CopyText/CopyImage/CopyFiles`：UI → worker，按类型写回剪贴板
//!
//! worker 是 `Store` 的唯一持有者，UI 不直接碰数据库，避免跨线程锁竞争。

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::model::{Captured, Kind};
use crate::store::{InsertOutcome, Store};
use crate::win::clipboard;

const READ_TIMEOUT: Duration = Duration::from_millis(400);

/// worker 是否已经把 WAL 合并完、准备退出了。
///
/// 退出流程要等这个标志：合并 WAL 要多久取决于库有多大，睡一个固定的
/// 时长不可靠——短了数据没落盘，长了白等。
pub static WORKER_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// UI 要求把一条历史写回剪贴板。
pub struct CopyRequest {
    pub kind: crate::model::Kind,
    /// 文本是 UTF-8 字节，图片是 PNG 字节，文件是换行分隔的路径。
    pub payload: Vec<u8>,
    /// 写完后要不要再模拟一次 Ctrl+V 直接粘到原来的窗口。
    pub paste: bool,
}

/// UI 发给 worker 的命令。
pub enum Command {
    /// 捕获事件，携带系统剪贴板序列号。
    Seq(u32),
    /// 写回剪贴板，可选地在写完后自动粘贴。
    Copy(CopyRequest),
    /// 删除一条历史。
    Delete(i64),
    /// 清空历史（收藏项保留）。回调里带要重查的信号由 UI 自己处理。
    Clear,
    /// 切换收藏标记。
    SetPinned(i64, bool),
    /// 关闭 worker，让主循环自然退出。
    #[allow(dead_code)]
    Shutdown,
}

#[derive(Default)]
pub struct Stats {
    pub events: u64,
    pub stored: u64,
    pub dup: u64,
    pub empty: u64,
    pub timeouts: u64,
}

/// worker 的入口，阻塞运行直到收到 `Shutdown`。
pub fn run(rx: Receiver<Command>, store: Store, notify: crossbeam_channel::Sender<()>) {
    let mut cap = Capture {
        store,
        last_seq: 0,
        self_write: None,
        stats: Stats::default(),
        notify,
    };

    for cmd in rx {
        match cmd {
            Command::Seq(seq) => cap.handle(seq),
            Command::Copy(req) => cap.copy(req),
            Command::Delete(id) => {
                match cap.store.delete(id) {
                    Ok(true) => crate::log_info!("已删除 #{id}"),
                    Ok(false) => {}
                    Err(e) => crate::log_error!("删除 #{id} 失败: {e:#}"),
                }
                cap.nudge_ui();
            }
            Command::Clear => {
                match cap.store.clear(true) {
                    Ok(n) => crate::log_info!("清空历史，删除 {n} 条（收藏项保留）"),
                    Err(e) => crate::log_error!("清空历史失败: {e:#}"),
                }
                cap.nudge_ui();
            }
            Command::SetPinned(id, pinned) => {
                if let Err(e) = cap.store.set_pinned(id, pinned) {
                    crate::log_warn!("收藏 #{id} 失败: {e:#}");
                }
                cap.nudge_ui();
            }
            Command::Shutdown => {
                // 退出前把 WAL 合并回主库：不合并的话，主 db 文件可能还是
                // 几天前的旧快照（数据全在 -wal 里），拷它当备份会拿到空库。
                if let Err(e) = cap.store.checkpoint() {
                    crate::log_warn!("退出前合并 WAL 失败: {e:#}");
                }
                break;
            }
        }
    }

    WORKER_DONE.store(true, std::sync::atomic::Ordering::Release);

    let s = &cap.stats;
    crate::log_info!(
        "worker 退出。事件 {} · 新增 {} · 去重 {} · 空 {} · 超时 {}",
        s.events,
        s.stored,
        s.dup,
        s.empty,
        s.timeouts
    );
}

struct Capture {
    store: Store,
    last_seq: u32,
    /// 自己写回剪贴板造成的变更，喂回来时抑制掉。
    self_write: Option<SelfWrite>,
    stats: Stats,
    /// UI 的「内容变了」提示。删除 / 清空 / 收藏之后要让列表重查一次，
    /// 否则用户点了删除，界面上那条还在。
    notify: crossbeam_channel::Sender<()>,
}

/// 一次写回在剪贴板上留下的痕迹。
///
/// 为什么是「序列号区间」而不是一个布尔标志：`write_image` 会挂 PNG 和 DIB
/// 两个格式，每次 `SetClipboardData` 都会推一个 seq，布尔只丢得掉第一条，
/// 第二条会被当成新复制记进库——粘贴一张图后该条目 `hit_count` +1、时间被
/// 刷新、位置跳到顶部。而标志在写失败时还会残留，反过来吞掉下一条真实复制。
///
/// 为什么是区间而不是时间窗口：区间之外的事件**必然**不是自己产生的，
/// 不会因为「窗口还没过期」误吞用户紧接着复制的内容。
#[derive(Clone, Copy)]
struct SelfWrite {
    /// 写入前的序列号基线。
    lo: u32,
    /// 写入后的序列号，`(lo, hi]` 就是这次写入造成的那几个事件。
    hi: u32,
    /// 写入内容的指纹。用于应对填进区间里的并发复制：内容对不上就不丢。
    hash: [u8; 32],
}

impl Capture {
    fn nudge_ui(&self) {
        let _ = self.notify.try_send(());
    }
    /// 写回剪贴板，可选地自动粘贴到唤出前的窗口。
    fn copy(&mut self, req: CopyRequest) {
        // 写回前记下序列号基线。这次写入造成的所有事件都落在 `(lo, hi]`，
        // 等 listener 把它们喂回来时按区间丢弃，避免自激振荡。
        let seq_before = clipboard::sequence_number();

        let ok = match req.kind {
            Kind::Text | Kind::RichText => {
                clipboard::write_text(&String::from_utf8_lossy(&req.payload))
            }
            Kind::Image => clipboard::write_image(&req.payload),
            Kind::Files => {
                let text = String::from_utf8_lossy(&req.payload);
                let paths: Vec<String> = text.lines().map(|s| s.to_owned()).collect();
                clipboard::write_files(&paths)
            }
        };

        if !ok {
            // 失败不登记任何抑制信息：剪贴板没被改，也就不会有自己造成的事件。
            crate::log_error!("写回剪贴板失败");
            return;
        }

        let seq_after = clipboard::sequence_number();
        self.self_write = (seq_after != seq_before).then(|| SelfWrite {
            lo: seq_before,
            hi: seq_after,
            // 读回来的是**归一化**后的内容，所以用 `req.payload` 的指纹比对：
            // 文本写回时的 CRLF 会在读取侧被归一成 LF，图片按 CF_PNG 优先读回，
            // 文件按换行拼路径，三者都能对上。
            hash: *blake3::hash(&req.payload).as_bytes(),
        });

        if !req.paste {
            return;
        }

        // 自动粘贴。hide 由 `paste_to_previous` 内部处理，而且必须在它
        // 把焦点还回去**之后**才隐藏——顺序反了会丢掉前台权限。
        std::thread::sleep(Duration::from_millis(30));
        if !crate::win::paste_to_previous() {
            crate::log_error!("自动粘贴失败，内容已在剪贴板，可手动 Ctrl+V");
        }
    }

    fn handle(&mut self, seq: u32) {
        // 序列号只是单调递增的不透明值，不是拷贝次数。
        // 相同就说明是同一次变更的重复通知。
        if seq == self.last_seq {
            return;
        }
        self.last_seq = seq;

        // 自己刚写回的那次，丢弃。
        if let Some(sw) = self.self_write {
            if seq_in_range(sw.lo, sw.hi, seq) {
                // 区间里可能有**多个**事件：实测一次图片写回会产生 5 个
                // （EmptyClipboard + PNG + DIB + 系统内部的两下）。
                // 所以不能像旧的布尔标志那样「用一次就清」，否则后面几个
                // 全被当成新复制记进库。
                if seq == sw.hi {
                    self.self_write = None;
                    // 只在收尾事件上验一次内容：用户可能在这几毫秒里又复制
                    // 了别的东西，那个 seq 同样落进区间，不能跟着一起吞掉。
                    if let Ok(Some(c)) = read_with_timeout()
                        && c.hash != sw.hash
                    {
                        self.stats.events += 1;
                        self.persist(seq, c, Instant::now());
                    }
                }
                // 读不出来（超时 / 被别的程序占着）也按自己写回的丢掉：
                // 多记一条自己写回的内容，比漏掉用户真实复制更能接受。
                return;
            }
            // 第一个落在区间外的事件说明自己这次写入已经走完，抑制信息作废。
            self.self_write = None;
        }

        self.stats.events += 1;
        let started = Instant::now();
        match read_with_timeout() {
            Ok(Some(captured)) => self.persist(seq, captured, started),
            Ok(None) => {
                self.stats.empty += 1;
                // 区分「剪贴板为空」和「有内容但没匹配到我们支持的格式」，
                // 排查捕获问题时这行是唯一的线索。
                crate::log_info!("seq {seq} 无可捕获内容（格式不支持或剪贴板被占用）");
            }
            Err(()) => {
                self.stats.timeouts += 1;
                crate::log_warn!("seq {seq} 读取超时，源程序可能卡在延迟渲染，已放弃");
            }
        }
    }

    fn persist(&mut self, seq: u32, captured: Captured, started: Instant) {
        let elapsed = started.elapsed();
        match self.store.insert(&captured) {
            Ok(InsertOutcome::New(id)) => {
                self.stats.stored += 1;
                // 只记元信息，不记内容：GUI 子系统没有控制台，而且剪贴板预览
                // 可能含密码等敏感信息，不该明文落到磁盘上。
                crate::log_info!(
                    "seq {seq} 新增 #{id} {} {}B {}us",
                    captured.kind.name(),
                    captured.len(),
                    elapsed.as_micros()
                );
            }
            Ok(InsertOutcome::Bumped(_)) => {
                // 去重路径是热路径，不写日志；计数在 worker 退出汇总里。
                self.stats.dup += 1;
            }
            Err(err) => crate::log_error!("入库失败: {err:#}"),
        }
    }
}

/// `Ok(Some(内容))` / `Ok(None)` 剪贴板为空 / `Err(())` 读超时。
fn read_with_timeout() -> Result<Option<Captured>, ()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("rpaste-read".into())
        .spawn(move || {
            let _ = tx.send(clipboard::read());
        })
        .map_err(|_| ())?;

    rx.recv_timeout(READ_TIMEOUT).map_err(|_| ())
}

/// `seq` 是否落在自己写回造成的区间 `(lo, hi]` 里。
///
/// 序列号是 u32，跑满会回绕，所以用 `wrapping_sub` 算相对偏移，
/// 不能直接比较大小。`lo == hi` 表示这次写入没产生新序列号，区间为空。
fn seq_in_range(lo: u32, hi: u32, seq: u32) -> bool {
    let span = hi.wrapping_sub(lo);
    let offset = seq.wrapping_sub(lo);
    span > 0 && offset > 0 && offset <= span
}

#[cfg(test)]
mod tests {
    use super::seq_in_range;

    #[test]
    fn self_write_range_covers_every_seq_it_produced() {
        // 图片写回会挂 PNG + DIB 两个格式，产生多个 seq，必须全部盖住。
        assert!(seq_in_range(100, 103, 101));
        assert!(seq_in_range(100, 103, 102));
        assert!(seq_in_range(100, 103, 103));
    }

    #[test]
    fn self_write_range_excludes_baseline_and_later_events() {
        // lo 是写之前的状态；hi 之后是别人的复制，都不能丢。
        assert!(!seq_in_range(100, 103, 100));
        assert!(!seq_in_range(100, 103, 104));
        assert!(!seq_in_range(100, 103, 200));
    }

    #[test]
    fn self_write_range_handles_u32_wraparound() {
        let lo = u32::MAX - 1;
        let hi = 2; // 事件序列: MAX-1 → MAX → 0 → 1 → 2
        assert!(seq_in_range(lo, hi, 0));
        assert!(seq_in_range(lo, hi, 2));
        assert!(!seq_in_range(lo, hi, 3));
    }

    #[test]
    fn empty_range_matches_nothing() {
        assert!(!seq_in_range(7, 7, 7));
        assert!(!seq_in_range(7, 7, 8));
    }
}
