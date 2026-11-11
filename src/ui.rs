//! egui 界面。只读查询走 `ReadStore`，写回走 worker 的 channel。
//!
//! 设计约定：
//! - 每个元素都要有紧凑的卡片样式，历史列表按时间倒序。
//! - 点选一条 → 发 `Command::CopyText` 给 worker，同时把窗口隐藏。
//! - 点关闭按钮不退出，只是隐藏到托盘；退出走托盘菜单。
//! - 显示/隐藏一律调 `win::window` 里的 Win32 函数，**不要用 egui 的
//!   `ViewportCommand::Visible`**：窗口隐藏后 winit 不再派发重绘，
//!   `update` 不执行，任何依赖 update 的逻辑都会失效。
//! - UI 每次收到 worker 的更新提示（`Notify`）就同步内存索引：增量拉新条目，
//!   条数和库里对不上就全量重建（见 `crate::search`）。

use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::capture::{Command, CopyRequest};
use crate::model::{Kind, denormalize_newlines};
use crate::search::{Counts, Index, KindFilter, match_ranges};
use crate::store::{EntryMeta, ReadStore};
use crate::theme;
use crate::thumb::{PreviewState, Thumb, Thumbs};
use crate::win;
use crate::win::hotkey::{self, Hotkey};

const RECENT_LIMIT: usize = 200;

/// 列表里每条预览最多占几行。
///
/// 长文本（配置、日志、一段 SQL）折行后能把整个窗口吃掉，列表就失去
/// 「一眼扫过去」的作用；只给一行又常常看不出这条讲什么。两行是平衡点，
/// 全文交给空格打开的浮层。
const PREVIEW_ROWS: usize = 2;

/// 列表预览最多参与排版的字符数。
///
/// 存储用的预览是 512 字，但两行无论如何显示不下这么多；不截的话每帧都要
/// 为 200 行各排版 512 字，纯属白烧。240 字已经超过两行的容量。
const PREVIEW_SHOW_CHARS: usize = 240;

/// 搜索时列表显示的命中上下文长度（字符）。
///
/// 命中点前后各留一段，让用户看出「为什么这条被选中」——长文本只显示开头
/// 时，命中的词经常在 5000 字之后，光看开头会以为搜索坏了。
const SNIPPET_CHARS: usize = 120;

/// 全文浮层最多渲染的字符数。
///
/// 单条内容上限是 64MB，直接丢给 `LayoutJob` 会把界面卡死几秒。32K 字
/// （约 20 屏）足够读完任何「刚复制的一段东西」，再多请回原程序里看。
const TEXT_VIEW_CHARS: usize = 32 * 1024;

/// 列表里图片缩略图的高度。
///
/// 和文本条目两行的高度基本一致（约 44px），这样图片行换上去不会把
/// 列表节奏打乱。
const THUMB_H: f32 = 44.0;
/// 缩略图最大宽度。
///
/// 不用「塞进正方形」的算法：591×258 这种宽图按高度缩放只剩 19px 高，
/// 白底截图基本什么都看不出来。改成高度固定、宽度封顶，宽图能多占些
/// 横向像素，行高依然是统一的 44。
const THUMB_MAX_W: f32 = 96.0;

/// `egui::Context` 是 `Send + Sync`，留给后台守护线程用。
///
/// **为什么需要它**：窗口被 `SW_HIDE` 收进托盘后 Windows 不再发 `WM_PAINT`。
/// eframe 0.33 的 `run.rs::check_redraw_requests()` 一遇到期的重绘请求就把
/// `ControlFlow` 设成 `Poll`，指望 `window.request_redraw()` 带来下一帧
/// `RedrawRequested` 再改回 `Wait`——隐藏窗口等不来这一帧，主循环就永久空转，
/// 实测跑满一个逻辑核（96.8%，任务管理器里约 12%）。此时 `App::update`
/// 根本不被调用（实测隐藏 8 秒只有帧 0），应用层没法自己救自己。
///
/// 守护线程在窗口隐藏期间每 500ms 手动补发一个 `WM_PAINT`（见
/// `win::poke_repaint`），替 winit 把缺失的 `RedrawRequested` 补上；同时塞一个
/// 远期重绘请求让 `check_redraw_requests` 走 `WaitUntil`。实测单核 CPU
/// 96.8% → 1.87%。窗口可见时什么都不发，正常渲染不受影响。
pub static EGUI_CTX: OnceLock<egui::Context> = OnceLock::new();

/// 启动主循环守护线程。窗口隐藏时每 500ms 补一帧，把 `ControlFlow` 按回 `Wait`。
pub fn spawn_repaint_guard() {
    let started = std::thread::Builder::new()
        .name("rpaste-repaint-guard".into())
        .spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if win::is_main_window_visible() {
                    continue;
                }

                // 关键是这一帧 WM_PAINT：它让 winit 派发 RedrawRequested，
                // eframe 才能走完 run_ui_and_paint 并返回 EventResult::Wait。
                win::poke_repaint();
                // 顺带塞一个远期重绘时间，让 check_redraw_requests 走 WaitUntil，
                // 避免在 Poll 上多空转一轮。
                if let Some(ctx) = EGUI_CTX.get() {
                    ctx.request_repaint_after(std::time::Duration::from_secs(3600));
                }
            }
        });
    if started.is_err() {
        crate::log_error!("无法启动 repaint guard 线程");
    }
}

/// 托盘菜单点了「设置」。
///
/// listener 线程碰不到 UI 的状态，而窗口隐藏时 `App::update` 根本不执行，
/// 事件递过去也没人接。所以这里只落一个标志位：listener 先把窗口唤出来，
/// UI 恢复渲染的第一帧再消费它。
static SETTINGS_REQUEST: AtomicBool = AtomicBool::new(false);

pub fn request_settings() {
    SETTINGS_REQUEST.store(true, Ordering::Release);
}

fn take_settings_request() -> bool {
    SETTINGS_REQUEST.swap(false, Ordering::Acquire)
}

/// 托盘菜单点了「关于」。与设置同理：listener 只落标志位，UI 首帧消费。
static ABOUT_REQUEST: AtomicBool = AtomicBool::new(false);

pub fn request_about() {
    ABOUT_REQUEST.store(true, Ordering::Release);
}

fn take_about_request() -> bool {
    ABOUT_REQUEST.swap(false, Ordering::Acquire)
}

/// 「清空历史」的确认有效期。清空不可撤销，所以点两次才执行；
/// 两次之间超过这个时间就当用户放弃了，按钮收回待确认态。
const CLEAR_CONFIRM_TTL: std::time::Duration = std::time::Duration::from_secs(3);

/// 默认窗口尺寸（逻辑像素）。
///
/// `main.rs` 首次启动、设置面板的「恢复默认尺寸」都用这一个数——两处各写
/// 一个值的话，「恢复默认」会跳到一个用户从没见过的大小。
pub const DEFAULT_WINDOW: (f32, f32) = (420.0, 560.0);

/// 设置面板里可选的保存上限。
///
/// 只给几档而不是让用户随手填数：条目上限直接决定删多少数据，填错一位
/// 就是十倍差距。这几档覆盖「只想留最近的」到「留够半年」，也都能一眼
/// 估出磁盘量级（纯文本约 1 万条十几 MB，图片视截图而定）。
const LIMIT_CHOICES: [i64; 4] = [1_000, 3_000, 10_000, 30_000];

/// `Alt+1..9` 对应的数字键，顺序即「列表第几条」。
const QUICK_PASTE_KEYS: [egui::Key; 9] = [
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
];

/// 粘贴时对文本做不做净化。
///
/// 用枚举而不是 `bool`：`send_copy(entry, true, false)` 读起来完全不知道那个
/// `false` 是什么，而这个参数会实实在在改动用户粘出去的内容。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PasteMode {
    /// 原样粘贴。
    Raw,
    /// 裁掉尾部空白再粘贴。
    ///
    /// 场景很具体：从终端、日志、浏览器地址栏复制的东西，尾部常带着一两个换行。
    /// 粘回终端会直接执行上一行命令，粘进聊天框会多出空行。只裁尾部不裁开头——
    /// 缩进是代码和 YAML 的内容，动了就是错的。
    Trimmed,
}

pub struct App {
    read: ReadStore,
    tx: crossbeam_channel::Sender<Command>,
    notify: crossbeam_channel::Receiver<()>,
    /// 内存检索索引：全量条目都在这里，列表和搜索都从它取。
    ///
    /// 和数据库的唯一关系是「下次刷新时对齐条数」——对不上就全量重建，
    /// 所以增量同步写错也不会留下永久不一致。
    index: Index,
    entries: Vec<EntryMeta>,
    search: String,
    /// 当前类型筛选。列表和搜索都受它约束。
    filter: KindFilter,
    /// 各类型条数，刷新时算一次，供顶部 chip 显示。
    counts: Counts,
    /// 需要重查数据库的信号（worker 提示或首次启动）。
    dirty: bool,
    /// 最近一次操作的状态提示，短暂显示在底部。
    toast: Option<(String, std::time::Instant)>,
    /// 上一次查询用的（筛选, 搜索词）和结果。egui 是立即模式，`ui_list`
    /// 每帧都会调用，没有这层缓存就会 60fps 无脑全量扫描。
    cached_key: Option<(KindFilter, String)>,
    cached_results: Vec<EntryMeta>,
    /// 搜索结果里「长文本命中上下文」：id → 切出来的片段。只在搜索时非空，
    /// 随 `cached_results` 一起重算。列表渲染只读它，不再单独查库。
    snippets: HashMap<i64, String>,
    /// 当前选中的条目。单击只改这个，双击/Enter 才真正动作。
    selected: Option<i64>,
    /// 上一次搜索词。用来检测「搜索词是否变了」，变了就清掉选中项。
    search_snapshot: String,
    /// 下一帧要把选中项滚进可视区。只在键盘移动选中时置位——
    /// 否则用户用滚轮翻看时会被一直拽回选中项。
    scroll_to_selected: bool,
    /// 上一帧窗口是否有焦点。用来检测「刚刚失焦」这一刻。
    was_focused: bool,
    /// 「清空历史」的一次确认。点第一次记时间，3 秒内再点才真的清。
    /// 清空是不可撤销的，不能一次点击就执行。
    confirm_clear: Option<std::time::Instant>,
    /// 文件类型条目的路径检查结果：id → 已经不存在的路径条数，0 表示全在。
    ///
    /// 只在新条目出现时增量检查，避免每帧对磁盘做 stat；双击粘贴时会
    /// 实时复查并覆盖这里的值，所以文件被恢复后也能立刻纠正。
    missing_paths: HashMap<i64, usize>,
    /// 图片条目的缩略图缓存。解码在独立线程，这里只管取和显示。
    thumbs: Thumbs,
    /// 大图预览浮层里正在显示的条目 id，`None` 表示没开。
    ///
    /// 存 id 而不是 bool：浮层要能独立于选中项存在（选中项一删就变），
    /// 也方便判断「当前这张解码到哪一步了」。
    preview_id: Option<i64>,
    /// 全文浮层的内容缓存。浮层每帧都要画这段文字，不能每帧去库里读一遍
    /// payload（大条目还可能是一次 blob 文件读）。
    text_view: Option<TextView>,
    /// 上一帧鼠标所在的行。
    ///
    /// 底色的绘制顺序决定了它必须提前知道 hover：egui 是即时模式，`Response`
    /// 要等内容画完才存在，而底色得画在内容**下面**。所以读上一帧的状态，
    /// 用一帧（16ms）的延迟换正确的绘制顺序。
    hover_row: Option<i64>,
    /// 浮层打开的時刻（`egui` 的 input time），用于淡入。
    ///
    /// 不用 `animate_bool_with_time`：它在一个 id 首次出现时直接返回终值，
    /// 于是「第一次打开」恰恰没有动画——而第一次正是最需要动画的那次。
    overlay_opened_at: Option<f64>,
    /// 设置面板是否打开。
    settings_open: bool,
    /// 关于面板是否打开。和设置面板互斥——两个都盖在同一层，同时开会叠成一坨。
    about_open: bool,
    /// 设置面板里是否正在「录」新热键（等用户按键）。
    recording: bool,
    /// 设置面板里的一次性反馈：`(文案, 是否成功)`。成功用强调色，失败用红色。
    setting_note: Option<(String, bool)>,
    /// 当前生效的呼出热键。启动时读一次注册表，改键成功后同步更新。
    hotkey: Hotkey,
    /// 当前生效的历史条目上限。改完立刻写注册表并通知 worker 淘汰。
    max_entries: i64,
    /// 上一次落盘的窗口尺寸，用来判断「用户还在拖」还是「已经停手」。
    size_saved: (u32, u32),
    /// 正在等待落盘的尺寸：`(尺寸, 这个尺寸第一次出现的时刻)`。
    ///
    /// 拖动窗口时每帧尺寸都在变，逐帧写注册表既浪费又会把中间态写进去。
    /// 只有「同一个尺寸稳定了半秒」才认为用户松手了。
    size_pending: Option<((u32, u32), f64)>,
}

/// 全文浮层要显示的那一段。
///
/// 长文本不可能整段丢给 `LayoutJob`（单条内容上限 64MB），所以这里只留窗口
/// 内的一段；有搜索词时窗口从命中处往前退一点开始，打开就能看见命中的词。
struct TextView {
    id: i64,
    /// 生成这一段时用的搜索词。搜索词变了窗口要重算，否则「跳到命中处」会失效。
    query: String,
    text: String,
    /// 第一个命中在 `text` 里的字节偏移。
    hit: Option<usize>,
    /// 前面还有内容（已经切成窗口了）。
    skipped: bool,
    /// 后面还有内容（超出窗口上限）。
    truncated: bool,
    /// 下一帧要不要把命中处滚进视野。只在刚打开/搜索词刚变时为真——
    /// 之后每帧都滚的话，用户自己翻页会被一直拽回去。
    scroll_to_hit: bool,
}

impl App {
    pub fn new(
        ctx: &egui::Context,
        read: ReadStore,
        tx: crossbeam_channel::Sender<Command>,
        notify: crossbeam_channel::Receiver<()>,
    ) -> Self {
        let mut app = Self {
            read,
            tx,
            notify,
            index: Index::new(),
            entries: Vec::new(),
            search: String::new(),
            filter: KindFilter::All,
            counts: Counts::default(),
            dirty: true,
            toast: None,
            cached_key: None,
            cached_results: Vec::new(),
            snippets: HashMap::new(),
            selected: None,
            search_snapshot: String::new(),
            scroll_to_selected: false,
            was_focused: false,
            confirm_clear: None,
            missing_paths: HashMap::new(),
            thumbs: Thumbs::new(ctx),
            preview_id: None,
            hover_row: None,
            overlay_opened_at: None,
            text_view: None,
            settings_open: false,
            about_open: false,
            recording: false,
            setting_note: None,
            hotkey: Hotkey::load(),
            max_entries: win::settings::max_entries(),
            size_saved: (0, 0),
            size_pending: None,
        };
        app.refresh();
        app
    }

    /// 装入中文字体，否则中文全是豆腐块。
    ///
    /// 用的是 `tools/subset_font.py` 裁剪出来的子集（GB2312 全集 + ASCII + 常用标点，
    /// 2MB 而不是系统字体的 9.7MB），并直接 `include_bytes!` 编进二进制：
    /// - 单文件分发，不依赖目标机器装了什么字体
    /// - `from_static` 引用只读段，不像 `from_owned` 那样在堆上再复制一份
    ///
    /// egui 自带字体不含 CJK，必须追加到字体族末尾做回落。
    pub fn install_cjk_font(ctx: &egui::Context) {
        // 调试用：设 RPASTE_NO_FONT=1 可跳过字体加载，用来量化字体占了多少常驻内存。
        if std::env::var_os("RPASTE_NO_FONT").is_some() {
            return;
        }

        const BUNDLED_FONT: &[u8] = include_bytes!("../assets/font.ttf");

        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            egui::FontData::from_static(BUNDLED_FONT).into(),
        );

        // 追加到两个族族的末尾：英文仍用 egui 自带字体（更紧凑），
        // 缺字时才回落到中文字体。
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_owned());
        }

        ctx.set_fonts(fonts);
    }

    fn refresh(&mut self) {
        let Ok(total) = self.read.total() else {
            return;
        };

        // 增量同步：新条目（id 更大）+ 最近窗口的字段刷新。
        // 去重命中（bump）会改 created_at，靠 recent() 那 200 条覆盖过来——
        // 被 bump 的条目一定是最新的，必然落在这个窗口里。
        // 两批合成一次合并：`merge` 内部按 id 去重，排序只做一次。
        let since = self.index.max_id();
        let mut fresh = self.read.meta_since(since).unwrap_or_default();
        fresh.extend(self.read.recent(RECENT_LIMIT).unwrap_or_default());
        // 正文尾部只跟着**新条目**走：`recent()` 那批老条目的尾部早在索引里，
        // 每次刷新重发一遍就是白读几十 KB。
        let tails = self.read.tails_since(since).unwrap_or_default();
        self.index.merge(&fresh, &tails);

        // 「删一条 + 加一条」时条数刚好对得上，增量同步看不出来；
        // 条数一旦不符就全量重建——这是索引和库之间唯一的正确性兜底。
        if self.index.len() as i64 != total
            && let Ok(all) = self.read.all_meta()
        {
            let tails = self.read.all_tails().unwrap_or_default();
            self.index.rebuild(all, &tails);
        }

        self.entries = self.index.list(RECENT_LIMIT);
        self.counts = self.index.counts();
        self.dirty = false;
        // 库里的数据变了，搜索结果也得重算，否则搜着搜着新复制的内容不出现。
        self.cached_key = None;
        self.refresh_missing_paths();
    }

    /// 增量刷新「源文件已不存在」标记。
    ///
    /// 只查本次新出现的条目，已经算过的沿用旧结果；条目被删掉时把缓存一并清掉。
    /// `refresh()` 只在数据变化时调用，不会每帧扫磁盘。
    fn refresh_missing_paths(&mut self) {
        let live: HashSet<i64> = self.entries.iter().map(|e| e.id).collect();
        self.missing_paths.retain(|id, _| live.contains(id));

        for entry in &self.entries {
            if entry.kind != Kind::Files || self.missing_paths.contains_key(&entry.id) {
                continue;
            }
            let Ok(Some(payload)) = self.read.load_payload(entry.id) else {
                continue;
            };
            let missing = count_missing_paths(&payload);
            self.missing_paths.insert(entry.id, missing);
        }
    }

    /// 每帧先消费 worker 的更新提示，有就重查。
    fn poll_notify(&mut self) {
        while self.notify.try_recv().is_ok() {
            self.dirty = true;
        }
        if self.dirty {
            self.refresh();
        }
    }

    /// 把一条历史发回 worker。`paste` 为真时写完后自动 Ctrl+V 到原窗口。
    fn send_copy(&mut self, entry: &EntryMeta, paste: bool, mode: PasteMode) -> bool {
        let Ok(Some(payload)) = self.read.load_payload(entry.id) else {
            return false;
        };

        // 文件类型在写回前实时复查一次：源文件可能已经被删掉或移走，
        // 再写回去就是个悬空引用，资源管理器只会静默失败、什么都不提示。
        // 这里拦下本次粘贴并弹提示；窗口不隐藏，用户才看得见为什么没反应。
        if entry.kind == Kind::Files {
            let missing = count_missing_paths(&payload);
            self.missing_paths.insert(entry.id, missing);
            if missing > 0 {
                self.show_toast(format!(
                    "源文件已不存在（{missing} 个），无法粘贴：{}",
                    entry.preview
                ));
                return false;
            }
        }

        // 文本写回剪贴板时换成 CRLF，记事本 / Office 更认。
        // 图片和文件的 payload 是二进制，原样传。
        let payload = paste_payload(entry.kind, payload, mode);

        let req = CopyRequest {
            kind: entry.kind,
            payload,
            paste,
        };

        if self.tx.send(Command::Copy(req)).is_ok() {
            let verb = match (paste, mode) {
                (true, PasteMode::Trimmed) => "已粘贴（去掉尾部空行）",
                (true, PasteMode::Raw) => "已粘贴",
                (false, PasteMode::Trimmed) => "已复制（去掉尾部空行）",
                (false, PasteMode::Raw) => "已复制",
            };
            self.show_toast(format!("{verb} #{}", entry.id));
            true
        } else {
            false
        }
    }

    /// 单击：只选中，不动剪贴板也不收窗口。用户还需要看得见自己选了哪条。
    fn on_single_click(&mut self, entry: &EntryMeta) {
        self.selected = Some(entry.id);
    }

    /// 双击（或选中后按 Enter）：复制并直接粘贴到原来的窗口。
    /// **不在这里隐藏窗口**——hide 由 worker 在把焦点还回去之后执行，
    /// 顺序见 `win::paste_to_previous`。
    fn on_activate(&mut self, entry: &EntryMeta) {
        self.selected = Some(entry.id);
        self.send_copy(entry, true, PasteMode::Raw);
    }

    /// `Ctrl+Enter`：粘之前把尾部空白裁掉。
    ///
    /// 这是「粘贴为纯文本」这个常见需求在本项目里的实际形态：剪贴板写回时本来
    /// 就只挂 `CF_UNICODETEXT`，格式已经没了，剩下会咬人的只有尾部换行。
    fn on_activate_trimmed(&mut self, entry: &EntryMeta) {
        self.selected = Some(entry.id);
        self.send_copy(entry, true, PasteMode::Trimmed);
    }

    /// 删除选中项。
    ///
    /// 命令发给 worker 走写连接，本地同时把这条摘掉——两边的数据源不同
    /// （UI 持只读连接），不先摘的话要等 worker 回来才看得到变化。
    fn delete_selected(&mut self) {
        let Some(id) = self.selected else {
            return;
        };

        // 位置要在**当前可见列表**上算，不是全量 `entries`：筛了类型或搜了词时
        // 两者顺序完全不同，用全量列表定位会把选中项跳到看不见的地方。
        let list = self.filtered();
        let Some(pos) = list.iter().position(|e| e.id == id) else {
            return;
        };
        let preview: String = list[pos].preview.chars().take(24).collect();

        if self.tx.send(Command::Delete(id)).is_err() {
            return;
        }

        self.index.remove(id);
        self.entries.retain(|e| e.id != id);
        self.cached_results.retain(|e| e.id != id);
        self.snippets.remove(&id);
        self.counts = self.index.counts();

        // 选中原位置的下一条；删的正好是最后一条就退到新的末尾。
        let remaining: Vec<i64> = list.iter().filter(|e| e.id != id).map(|e| e.id).collect();
        self.selected = remaining.get(pos).or_else(|| remaining.last()).copied();
        self.scroll_to_selected = true;

        // 把删掉的是什么写出来——删除不可撤销，至少让用户确认删对了。
        self.show_toast(format!("已删除：{preview}"));
    }

    /// 切换收藏。收藏项不会被 1 万条的容量规则淘汰。
    fn toggle_pin_selected(&mut self) {
        let Some(id) = self.selected else {
            return;
        };
        let Some(pinned) = self.entries.iter().find(|e| e.id == id).map(|e| !e.pinned) else {
            return;
        };

        if self.tx.send(Command::SetPinned(id, pinned)).is_err() {
            return;
        }

        self.index.set_pinned(id, pinned);
        // 收藏会改变排序（置顶），列表必须从索引重新取一份。只改 `pinned`
        // 字段的话条目还蹲在原位置，和「收藏 = 钉在顶部」的直觉对不上。
        self.entries = self.index.list(RECENT_LIMIT);
        self.cached_key = None;
        self.selected = Some(id);
        self.scroll_to_selected = true;

        self.show_toast(if pinned {
            "已收藏（置顶，且不会被自动清理）".to_owned()
        } else {
            "已取消收藏".to_owned()
        });
    }

    /// 窗口失焦就收起；重新获得焦点时把键盘焦点交给搜索框。
    ///
    /// 只在「从有焦点变成没焦点」的那一刻动作：窗口刚唤出时焦点可能还没到，
    /// 那种 `focused == false` 不能当成用户点了外面。
    ///
    /// 焦点一进来就 request_focus 到搜索框，这样唤出后直接打字就是搜索，
    /// 不用先用鼠标点一下输入框。
    fn tick_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));

        if self.was_focused && !focused {
            win::hide_main_window();
            // 窗口收起来了，设置面板没有理由留到下次唤出——用户再按热键
            // 是想看历史，不是想接着改设置。
            self.close_settings();
            self.close_about();
        } else if !self.was_focused && focused && !self.settings_open && !self.about_open {
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("search")));
        }

        self.was_focused = focused;
    }

    /// 收起设置面板，顺带清掉录制态和一次性提示。
    fn close_settings(&mut self) {
        self.settings_open = false;
        self.recording = false;
        self.setting_note = None;
    }

    /// 打开设置面板。每次打开都重新读一遍注册表：万一用户手改了注册表，
    /// 面板显示的应该是「现在真正生效的」，而不是上次读到的。
    fn open_settings(&mut self) {
        self.settings_open = true;
        self.about_open = false;
        self.recording = false;
        self.setting_note = None;
        self.hotkey = Hotkey::load();
        self.max_entries = win::settings::max_entries();
    }

    fn close_about(&mut self) {
        self.about_open = false;
    }

    fn open_about(&mut self) {
        // 两块卡片用同一个居中位置和同一种压暗，同时开着只会互相盖住。
        self.close_settings();
        self.about_open = true;
    }

    /// 搜索词一变就丢掉选中项。
    ///
    /// 两个理由：
    /// 1. 之前选中的那行可能已经不在新结果里了，`selected` 指向一条看不见的记录。
    /// 2. `Delete` 的语义是「删选中的那条」。用户正在改搜索词时按 Delete
    ///    显然是想删字符，不该有一把「看不见的刀」挂在选中项上。
    fn sync_search_state(&mut self) {
        if self.search != self.search_snapshot {
            self.search_snapshot = self.search.clone();
            self.selected = None;
        }
    }

    /// 键盘操作。
    ///
    /// 这些键必须用 `consume_key` 从事件队列里**摘掉**：搜索框有焦点时，
    /// egui 的 TextEdit 会先消费 Enter（表现为失焦），方向键也会被挪光标。
    /// 只在 `update` 开头拦，才轮得到我们处理。
    ///
    /// 方向键要**循环消费**：一帧里可能攒了好几个按键事件（按住不放的自动重复，
    /// 或者处理慢时的补帧），而 `consume_key` 一次只摘掉一个。
    /// 不循环的话，快按几下只会动一格，手感很粘。
    fn handle_keys(&mut self, ctx: &egui::Context) {
        // Ctrl+K 开关设置面板。
        //
        // 放在最前面：面板开着时也要能按键关掉，否则只能去点「完成」或者点空白。
        // 这也是唯一一条不依赖鼠标的入口——鼠标点击要求窗口已是前台，
        // 而这个窗口一失焦就自己收起。
        //
        // 不用更符合直觉的 `Ctrl+,`：逗号经 winit 进来是 `Key::Character(",")`，
        // 实测 egui 侧拿不到 `Key::Comma`（`Ctrl+comma` 在真键盘上同样不触发），
        // 而字母键这条路径是通的。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::K)) {
            if self.settings_open {
                self.close_settings();
            } else {
                self.open_settings();
                ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("search")));
            }
            return;
        }

        // 设置面板打开时，列表的快捷键全部让路：这时候按 Del 想删的是
        // 刚录进去的那个组合，不是某条历史；按 Alt+1 更不该直接粘出去。
        if self.settings_open {
            self.handle_settings_keys(ctx);
            return;
        }

        // 关于面板同理：它只是块牌子，但底下压着列表，按键必须拦住，
        // 否则盖着牌子按 Del 会把一条看不见的历史删掉。
        if self.about_open {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                self.close_about();
            }
            return;
        }

        // Esc 是分层退出：浮层开着先关浮层，再按一次才收窗口。
        // 直接收窗口的话，用户想退掉大图就整个界面没了，得重新按一次热键。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if self.preview_id.is_some() {
                self.preview_id = None;
                self.text_view = None;
            } else {
                win::hide_main_window();
            }
            return;
        }

        // Alt+1..9：直接粘贴当前列表的第 N 条。
        //
        // 比「↓ 定位再 Enter」少一半按键，翻前几条时这是最快的路径。
        // 顺序按**当前可见列表**算，所以搜索和筛选之后按的仍然是眼前第 N 条。
        for (i, key) in QUICK_PASTE_KEYS.iter().enumerate() {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, *key)) {
                // Alt+数字在 Windows 上会同时产生一个 `Event::Text("2")`，
                // 而 `consume_key` 只摘 `Event::Key`——不删掉文本事件的话，
                // 键是"吞"了，数字照样落到搜索框里，把列表筛成只剩含 "2" 的条目。
                drop_text(ctx, &char::from(b'1' + i as u8).to_string());
                match self.filtered().get(i).cloned() {
                    Some(entry) => self.on_activate(&entry),
                    // 按了超出列表长度的数字：给一句话，否则看起来像没反应。
                    None => self.show_toast(format!("当前列表没有第 {} 条", i + 1)),
                }
                return;
            }
        }

        // Delete 删选中项。
        //
        // **只在有选中项时才碰这个键**：没有选中项说明用户在编辑搜索词，
        // Delete 该归输入框，这时候吃掉事件会让「删字符」失灵。
        //
        // 有选中项时则一定删——用户按 ↓ 或点了某一行，预期就是「删这一条」。
        // 搜索词一变会清空选中项（见 `sync_search_state`），所以「正在改搜索词
        // 却把历史删掉」的路径被堵住了。
        if self.selected.is_some()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Delete))
        {
            self.delete_selected();
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::P)) {
            self.toggle_pin_selected();
        }

        // 类型筛选：Ctrl+1/2/3 只看文本 / 图片 / 文件，Ctrl+0 回到全部。
        for (key, filter) in [
            (egui::Key::Num1, KindFilter::Text),
            (egui::Key::Num2, KindFilter::Image),
            (egui::Key::Num3, KindFilter::Files),
            (egui::Key::Num0, KindFilter::All),
        ] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, key)) {
                self.set_filter(filter);
            }
        }

        // 空格开关预览浮层（图片看大图，文本看全文）。
        //
        // 这个键和输入框有冲突，所以**先判断该不该抢，再消费事件**：
        // `consume_key` 一旦调用，事件就从队列里没了，条件不满足时再后悔
        // 已经晚了——用户按空格想打「空格」会变成什么也没发生。
        //
        // 抢的两种情形：浮层开着（空格 = 关掉）；搜索词为空且有选中项
        // （空搜索框里打空格毫无意义，正好让给预览）。搜索词非空时一律让给
        // 输入框，否则多词搜索连分隔符都打不出来。
        let want_space =
            self.preview_id.is_some() || (self.search.trim().is_empty() && self.selected.is_some());
        if want_space && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space)) {
            // 同上：输入框吃的是 `Event::Text`，不把那个空格一起滤掉的话，
            // 搜索词会被改动，进而触发 `sync_search_state` 清空选中项——
            // 表现成"按空格预览失败，顺带把选中项也弄丢了"。
            drop_text(ctx, " ");
            self.preview_id = if self.preview_id.is_some() {
                None
            } else {
                self.selected
            };
            return;
        }

        // Alt+Enter：打开/关闭预览浮层，**搜索词非空时也能用**。
        //
        // 空格是更顺手的那个，但它必须让给「在搜索词里打空格」，所以再留一个
        // 不受搜索状态影响的入口。搜出一条长文本、想读全文的时候正是需要它
        // 的时候——而那一刻搜索框里通常正有词。
        if self.selected.is_some()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::Enter))
        {
            self.preview_id = if self.preview_id.is_some() {
                None
            } else {
                self.selected
            };
            return;
        }

        // Ctrl+Enter：粘贴并裁掉尾部空白。
        //
        // 必须在普通的 Enter 之前消费：`consume_key` 按修饰键精确匹配，两者
        // 不会互相抢；但先问 Ctrl 版本能保证「按住 Ctrl 按回车」不会被当成
        // 一次普通粘贴——那种情况下尾部换行正是要消掉的东西。
        let ctrl_enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter));
        let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));

        let ups = drain_keys(ctx, egui::Key::ArrowUp);
        let downs = drain_keys(ctx, egui::Key::ArrowDown);

        if ups == 0 && downs == 0 && !enter && !ctrl_enter {
            return;
        }

        let list = self.filtered();
        if list.is_empty() {
            return;
        }

        for _ in 0..ups {
            self.step_selection(&list, -1);
        }
        for _ in 0..downs {
            self.step_selection(&list, 1);
        }

        if enter || ctrl_enter {
            // 没选中就直接拿第一条——`Alt+V` 后一路 Enter 是最顺手的路径。
            let target = match self.selected {
                Some(id) if list.iter().any(|e| e.id == id) => {
                    list.iter().find(|e| e.id == id).cloned()
                }
                _ => list.first().cloned(),
            };
            if let Some(entry) = target {
                if ctrl_enter {
                    self.on_activate_trimmed(&entry);
                } else {
                    self.on_activate(&entry);
                }
            }
        }
    }

    /// 设置面板里的按键。
    ///
    /// 只有两件事：Esc 分级退出（录制态先取消录制），录制态下把下一次按键
    /// 收成新热键。其余按键一律不响应——面板打开时列表的快捷键已经全部让路。
    fn handle_settings_keys(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if self.recording {
                self.recording = false;
            } else {
                self.close_settings();
            }
            return;
        }
        if !self.recording {
            return;
        }

        // 取第一个「带修饰键的按下事件」。修饰键本身不产生 `Event::Key`，
        // 所以这里拿到的组合已经带着当次按下的 Ctrl/Alt/Shift 状态。
        let picked = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Hotkey::from_key(*key, *modifiers),
                _ => None,
            })
        });
        let Some(hk) = picked else {
            // 用户按了个裸键：不认，继续等。这里不能退出录制态，
            // 否则「先按了 Shift 再按 V」会被当成失败。
            return;
        };

        self.recording = false;
        // 录成了当前正在用的组合：探测必然失败（占着它的就是我们自己），
        // 直接给一句人话，别报「被其他程序占用」让人去找冲突。
        if hk == self.hotkey {
            self.setting_note = Some((format!("已经是 {}", hk.label()), true));
            return;
        }
        match hotkey::try_apply(hk) {
            Ok(()) => {
                self.hotkey = hk;
                self.setting_note = Some((format!("已切换为 {}", hk.label()), true));
            }
            Err(err) => self.setting_note = Some((err, false)),
        }
    }

    /// 设置面板：呼出热键 + 窗口尺寸。
    ///
    /// 用独立 `Area` 压暗 + `Window` 承载卡片：卡片高度由内容决定，
    /// 而 `Window` 的 `anchor` 每帧按实际尺寸重新居中，不用先估高度。
    fn ui_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let screen = ctx.content_rect();

        // 压暗。点空白处收起——这层在卡片之前创建，所以永远画在卡片下面。
        let mut clicked_outside = false;
        egui::Area::new(egui::Id::new("settings-dim"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(150));
                clicked_outside = resp.clicked();
            });

        let visuals = ctx.style().visuals.clone();
        let mut close = false;
        let mut start_recording = false;
        let mut reset_hotkey = false;
        let mut reset_size = false;
        let mut open_about = false;
        let mut new_limit: Option<i64> = None;

        egui::Window::new("settings-card")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .movable(false)
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .frame(
                egui::Frame::new()
                    .fill(visuals.window_fill)
                    .stroke(visuals.window_stroke)
                    .corner_radius(theme::RADIUS_LG)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                ui.set_width(290.0);
                ui.label(egui::RichText::new("设置").size(theme::FONT_BODY).strong());
                ui.add_space(theme::GAP_MD);
                ui.separator();
                ui.add_space(theme::GAP_MD);

                // ---- 呼出快捷键
                section_label(ui, "呼出快捷键");
                ui.add_space(theme::GAP_SM);
                ui.horizontal(|ui| {
                    if self.recording {
                        ui.label(
                            egui::RichText::new("请按下组合键…")
                                .size(theme::FONT_BODY)
                                .color(theme::ACCENT),
                        );
                    } else {
                        keycap_sized(ui, &self.hotkey.label(), theme::FONT_BODY, 24.0);
                        ui.add_space(theme::GAP_MD);
                        if ghost_button(
                            ui,
                            egui::Id::new("hotkey-edit"),
                            "修改",
                            theme::ACCENT,
                            false,
                        )
                        .clicked()
                        {
                            start_recording = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.hotkey != Hotkey::DEFAULT
                                && ghost_button(
                                    ui,
                                    egui::Id::new("hotkey-reset"),
                                    "恢复默认",
                                    theme::ACCENT,
                                    false,
                                )
                                .clicked()
                            {
                                reset_hotkey = true;
                            }
                        });
                    }
                });
                ui.add_space(theme::GAP_XS);
                hint_label(
                    ui,
                    if self.recording {
                        "按 Esc 取消。组合里必须有 Ctrl / Alt / Shift 之一。"
                    } else {
                        "点「修改」后按下新的组合键，立即生效。"
                    },
                );

                // ---- 窗口大小
                ui.add_space(theme::GAP_LG);
                ui.separator();
                ui.add_space(theme::GAP_MD);
                section_label(ui, "窗口大小");
                ui.add_space(theme::GAP_SM);
                ui.horizontal(|ui| {
                    let size = ctx.content_rect().size();
                    ui.label(
                        egui::RichText::new(format!(
                            "{} × {}",
                            size.x.round() as i32,
                            size.y.round() as i32
                        ))
                        .size(theme::FONT_BODY)
                        .color(visuals.text_color()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ghost_button(
                            ui,
                            egui::Id::new("size-reset"),
                            "恢复默认尺寸",
                            theme::ACCENT,
                            false,
                        )
                        .clicked()
                        {
                            reset_size = true;
                        }
                    });
                });
                ui.add_space(theme::GAP_XS);
                hint_label(ui, "拖动窗口边缘即可调整，下次打开按这个尺寸恢复。");

                // ---- 保存上限
                ui.add_space(theme::GAP_LG);
                ui.separator();
                ui.add_space(theme::GAP_MD);
                section_label(ui, "保存上限");
                ui.add_space(theme::GAP_SM);
                ui.horizontal(|ui| {
                    for value in LIMIT_CHOICES {
                        // 选中的那颗做实心，其余描边——一排按钮里要能一眼看出
                        // 现在用的是哪个，否则只能去读下面那行小字。
                        let selected = self.max_entries == value;
                        if ghost_button(
                            ui,
                            egui::Id::new(("limit", value)),
                            &thousands(value),
                            theme::ACCENT,
                            selected,
                        )
                        .clicked()
                            && !selected
                        {
                            new_limit = Some(value);
                        }
                    }
                });
                ui.add_space(theme::GAP_XS);
                hint_label(
                    ui,
                    &format!(
                        "最多保存 {} 条，超出按最旧优先淘汰，收藏项保留。",
                        thousands(self.max_entries)
                    ),
                );

                // ---- 一次性反馈（改键成功 / 被占用）
                if let Some((note, ok)) = &self.setting_note {
                    ui.add_space(theme::GAP_MD);
                    ui.label(
                        egui::RichText::new(note)
                            .size(theme::FONT_SM)
                            .color(if *ok { theme::ACCENT } else { theme::DANGER }),
                    );
                }

                ui.add_space(theme::GAP_LG);
                // 收尾按钮必须包在 `ui.horizontal` 里：裸 `with_layout` 会拿到
                // 整块剩余高度，把它撑成一条从内容底部到窗口底部的空白。
                ui.horizontal(|ui| {
                    // 左边「关于」、右边「完成」：关于不是主操作，但放在设置里
                    // 是唯一不离开键盘就能查到版本的路径（Ctrl+K 之后）。
                    if ghost_button(ui, egui::Id::new("settings-about"), "关于", theme::ACCENT, false)
                        .clicked()
                    {
                        open_about = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ghost_button(
                            ui,
                            egui::Id::new("settings-done"),
                            "完成",
                            theme::ACCENT,
                            false,
                        )
                        .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });

        if start_recording {
            self.recording = true;
            self.setting_note = None;
        }
        if reset_hotkey {
            match hotkey::try_apply(Hotkey::DEFAULT) {
                Ok(()) => {
                    self.hotkey = Hotkey::DEFAULT;
                    self.setting_note =
                        Some((format!("已恢复为 {}", Hotkey::DEFAULT.label()), true));
                }
                Err(err) => self.setting_note = Some((err, false)),
            }
        }
        if reset_size {
            // 只改窗口，不动注册表里的记忆：用户松手后 `persist_window_size`
            // 会把新尺寸写回去，记忆自然跟上。
            let (w, h) = DEFAULT_WINDOW;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
        }
        if let Some(limit) = new_limit {
            // 先落注册表再通知 worker：worker 才是真正删数据的一方，
            // 注册表写失败就当场报错、不改内存里的值，避免「面板说 1000、
            // 实际还是 30000」这种自相矛盾。
            match win::settings::set_max_entries(limit) {
                Ok(()) => {
                    self.max_entries = limit;
                    if self.tx.send(Command::SetLimit(limit)).is_err() {
                        crate::log_warn!("调整保存上限：worker 已退出，本次只在下次启动生效");
                    }
                    self.setting_note = Some((format!("已改为最多 {} 条", thousands(limit)), true));
                }
                Err(err) => self.setting_note = Some((err, false)),
            }
        }
        if close || clicked_outside {
            self.close_settings();
        }
        if open_about {
            self.open_about();
        }
    }

    /// 关于面板：版本号 + 两个「找得到人」的入口。
    ///
    /// 刻意不做成「设置里的一个小节」：想查版本的人第一反应是去托盘找关于，
    /// 而不是翻设置。两个入口（托盘菜单、面板里的关于按钮）都走这里。
    fn ui_about(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let screen = ctx.content_rect();

        let mut clicked_outside = false;
        egui::Area::new(egui::Id::new("about-dim"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(150));
                clicked_outside = resp.clicked();
            });

        let visuals = ctx.style().visuals.clone();
        let data_dir = crate::data_dir();
        let data_dir_text = data_dir.display().to_string();
        let mut close = false;
        let mut open_dir = false;
        let mut open_repo = false;

        egui::Window::new("about-card")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .movable(false)
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .frame(
                egui::Frame::new()
                    .fill(visuals.window_fill)
                    .stroke(visuals.window_stroke)
                    .corner_radius(theme::RADIUS_LG)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                ui.set_width(290.0);

                // 名字 + 版本放在同一行：版本是这块面板存在的理由，不该被
                // 挤到下面小字里。`v` 前缀用弱色，数字用正常色，扫一眼就能读出。
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("R-Paste")
                            .size(theme::FONT_TITLE + 3.0)
                            .strong(),
                    );
                    ui.add_space(theme::GAP_SM);
                    ui.label(
                        egui::RichText::new(format!("v{}", win::tray::VERSION))
                            .size(theme::FONT_BODY)
                            .color(theme::ACCENT)
                            .monospace(),
                    );
                });
                ui.add_space(theme::GAP_XS);
                hint_label(ui, "Windows 剪贴板历史工具");

                ui.add_space(theme::GAP_LG);
                ui.separator();
                ui.add_space(theme::GAP_MD);

                // ---- 数据目录
                section_label(ui, "数据目录");
                ui.add_space(theme::GAP_SM);
                if link_label(ui, &data_dir_text, theme::ACCENT).clicked() {
                    open_dir = true;
                }
                ui.add_space(theme::GAP_XS);
                hint_label(ui, "历史库与日志都在这里，点一下打开文件夹。");

                // ---- 项目主页
                ui.add_space(theme::GAP_MD);
                section_label(ui, "项目主页");
                ui.add_space(theme::GAP_SM);
                let repo = env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://");
                if link_label(ui, repo, theme::ACCENT).clicked() {
                    open_repo = true;
                }
                ui.add_space(theme::GAP_XS);
                hint_label(ui, "遇到问题或有想法，欢迎提 issue。");

                ui.add_space(theme::GAP_LG);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ghost_button(
                            ui,
                            egui::Id::new("about-done"),
                            "关闭",
                            theme::ACCENT,
                            false,
                        )
                        .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });

        if open_dir {
            win::shell::open(&data_dir_text);
        }
        if open_repo {
            win::shell::open(env!("CARGO_PKG_REPOSITORY"));
        }
        if close || clicked_outside {
            self.close_about();
        }
    }

    /// 把当前窗口尺寸记到注册表。
    ///
    /// 拖动过程中每帧尺寸都在变，逐帧写注册表既浪费又会存下中间态。
    /// 所以只在「同一个尺寸稳定了半秒」时落一次盘——那个时刻基本等于用户松手。
    fn persist_window_size(&mut self, ctx: &egui::Context) {
        let size = ctx.content_rect().size();
        let now = (size.x.round() as u32, size.y.round() as u32);
        if now == self.size_saved {
            self.size_pending = None;
            return;
        }

        let time = ctx.input(|i| i.time);
        match self.size_pending {
            Some((pending, since)) if pending == now => {
                if time - since >= 0.5 {
                    if let Err(err) = win::settings::set_window_size(now.0, now.1) {
                        crate::log_warn!("窗口尺寸保存失败: {err}");
                    } else {
                        self.size_saved = now;
                    }
                    self.size_pending = None;
                } else {
                    // 稳定期内还得有一帧来检查：窗口静止时 egui 不会自己重绘。
                    ctx.request_repaint_after(std::time::Duration::from_millis(520));
                }
            }
            _ => {
                self.size_pending = Some((now, time));
                ctx.request_repaint_after(std::time::Duration::from_millis(520));
            }
        }
    }

    /// 上下键移动选中项。到头就停住，不循环——循环会让「按到底」失去确定感。
    fn step_selection(&mut self, list: &[EntryMeta], delta: isize) {
        let cur = self
            .selected
            .and_then(|id| list.iter().position(|e| e.id == id));

        let next = match cur {
            Some(i) => (i as isize + delta).clamp(0, list.len() as isize - 1) as usize,
            // 还没选过：向下选第一条，向上选最后一条
            None => {
                if delta > 0 {
                    0
                } else {
                    list.len() - 1
                }
            }
        };

        self.selected = Some(list[next].id);
        // 让列表跟着滚。不置位的话，选中项移出可视区后按方向键
        // 会出现「选中了但看不见选中了什么」。
        self.scroll_to_selected = true;
    }

    fn show_toast(&mut self, msg: String) {
        self.toast = Some((msg, std::time::Instant::now()));
    }

    /// 取当前要显示的列表。筛选条件和搜索词都没变就复用上次结果，
    /// 避免每帧重新扫一遍索引。
    fn filtered(&mut self) -> Vec<EntryMeta> {
        let key = (self.filter, self.search.trim().to_owned());
        if self.cached_key.as_ref() == Some(&key) {
            return self.cached_results.clone();
        }

        let results = self.index.query(&key.1, key.0, RECENT_LIMIT);

        // 命中上下文和结果一起算：两者都是「查询变了才要重算」的东西，
        // 分开维护只会多一处忘记同步的地方。
        let ids: HashSet<i64> = results.iter().map(|e| e.id).collect();
        let terms: Vec<&str> = key.1.split_whitespace().collect();
        let snippets = self.index.snippets(&ids, &terms, SNIPPET_CHARS);

        self.cached_key = Some(key);
        self.cached_results = results.clone();
        self.snippets = snippets;
        results
    }

    /// 切类型筛选。再点一次当前档位等于取消筛选（回到「全部」）。
    /// 浮层淡入进度：`0.0` 刚打开，`1.0` 完全展开。
    ///
    /// 自己按时间算，不走 `animate_bool_with_time`：后者在 id 首次出现时
    /// 直接返回终值，于是「第一次打开」恰好没有动画，而第一次正是最该有的那次。
    fn overlay_fade(&mut self, ctx: &egui::Context) -> f32 {
        let Some(t0) = self.overlay_opened_at else {
            return 1.0;
        };
        let now = ctx.input(|i| i.time);
        let raw = ((now - t0) / theme::ANIM_BASE as f64).clamp(0.0, 1.0) as f32;
        if raw < 1.0 {
            // 进度是自己算的，egui 不知道动画还没走完——得显式要下一帧，
            // 否则淡入会停在第 1 帧（这个窗口空闲时本来就不重绘）。
            ctx.request_repaint();
        }
        // ease-out：开头快、结尾缓，比线性更像「弹出来」而不是「淡出来」。
        1.0 - (1.0 - raw) * (1.0 - raw)
    }

    fn set_filter(&mut self, filter: KindFilter) {
        self.filter = if self.filter == filter {
            KindFilter::All
        } else {
            filter
        };
        self.cached_key = None;
        // 之前选中的那条可能已经不在新列表里，留着会变成一把「看不见的刀」
        // ——`Delete` 会删掉一条用户根本看不见的记录。
        self.selected = None;
        self.scroll_to_selected = false;
    }

    fn ui_header(&mut self, ui: &mut egui::Ui) {
        // 搜索框自己就是标题：这个窗口只有一个用途，挂个 "R-Paste" 只是白占
        // 一行高度（顶栏每多一行，列表就少一行）。
        let show_clear = !self.search.is_empty();

        // 清除键画在输入框**里面**，而不是旁边再摆一个按钮。
        // 好处是顶栏只有一行，且宽度不用手算：`TextEdit` 的右内边距就是给
        // 它挖的坑，输入框照旧 `INFINITY` 填满整行。
        const CLEAR_W: i8 = 20;
        let edit = ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .id(egui::Id::new("search"))
                .hint_text("搜索历史…（空格分词，wj = 文件）")
                .margin(egui::Margin {
                    left: 4,
                    right: if show_clear { CLEAR_W + 2 } else { 4 },
                    top: 2,
                    bottom: 2,
                })
                .desired_width(f32::INFINITY),
        );

        // 搜索框为空时没有东西可清，按钮和它占的位置一起省掉。
        if show_clear {
            // 用 painter 自己画，而不是 `ui.put(Button)`：后者要走一遍 allocate，
            // 在这一行里再挤出一个控件位置，把输入框填满整行的效果破坏掉。
            let side = CLEAR_W as f32;
            let rect = egui::Rect::from_center_size(
                egui::pos2(edit.rect.right() - 2.0 - side / 2.0, edit.rect.center().y),
                egui::vec2(side, side),
            );
            let resp = ui
                .interact(rect, egui::Id::new("search-clear"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);

            let visuals = ui.visuals();
            // hover 底色淡入而不是凭空出现：这个小叉子面积不小，
            // 瞬间亮起一块底色在静止的界面里很扎眼。
            let hover_t = ui.ctx().animate_bool_with_time(
                egui::Id::new("search-clear-hover"),
                resp.hovered(),
                theme::ANIM_FAST,
            );
            if hover_t > 0.01 {
                ui.painter().rect_filled(
                    rect,
                    theme::RADIUS_SM,
                    theme::wash(visuals.widgets.hovered.bg_fill, (255.0 * hover_t) as u8),
                );
            }
            // 叉号自己画两条线，不用字符：egui 默认字体里没有 ✕ (U+2715)，
            // 直接写会渲染成豆腐块。画线还顺手解决了「不同字号下粗细不一」。
            let color = blend(
                visuals.weak_text_color(),
                visuals.strong_text_color(),
                hover_t,
            );
            let c = rect.center();
            let d = 4.0;
            let stroke = egui::Stroke::new(1.4, color);
            ui.painter()
                .line_segment([c + egui::vec2(-d, -d), c + egui::vec2(d, d)], stroke);
            ui.painter()
                .line_segment([c + egui::vec2(d, -d), c + egui::vec2(-d, d)], stroke);

            if resp.clicked() {
                self.search.clear();
                // 清空之后最常见的动作是接着打字，焦点还给输入框。
                edit.request_focus();
            }
        }

        // 类型筛选 chip。计数直接标在标签上——「图片 3」比只看「图片」信息量大，
        // 也让用户一眼知道切过去是不是空的。
        ui.add_space(theme::GAP_SM);
        let counts = self.counts;
        let mut picked = None;
        let mut open_settings = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = theme::GAP_SM;
            if let Some(f) = filter_chips(ui, self.filter, counts) {
                picked = Some(f);
            }
            // 设置放在这一行的右端，而不是底栏：底栏左半边要放四组快捷键提示，
            // 再挤第二个按钮就会像刚才那样叠在「Ctrl+P 收藏」上面。
            // 这一行右侧本来就空着，而且「设置」是低频动作，放次级行更合适。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 计数多到把 chip 排到行尾时，宁可先不显示按钮，也不要压住 chip。
                if ui.available_width() > 60.0
                    && ghost_button(
                        ui,
                        egui::Id::new("header-settings"),
                        "设置",
                        theme::ACCENT,
                        false,
                    )
                    .clicked()
                {
                    open_settings = true;
                }
            });
        });
        if open_settings {
            self.open_settings();
            // 设置面板打开后不该再往搜索框里打字，先把焦点交出去。
            ui.ctx()
                .memory_mut(|m| m.surrender_focus(egui::Id::new("search")));
        }
        if let Some(f) = picked {
            self.set_filter(f);
        }
        ui.separator();
    }

    fn ui_list(&mut self, ui: &mut egui::Ui) {
        let entries = self.filtered();
        let searching = !self.search.trim().is_empty();
        // 预览浮层盖住列表时不响应点击——否则点在图上会把下面那行也一起选中，
        // 甚至触发粘贴。列表照常绘制，只是不再接收交互。
        let locked = self.preview_id.is_some() || self.settings_open;
        // 搜索词拆一次给整轮渲染用；来源是 `self.search`，得先脱开借用，
        // 循环里还要 &mut self 处理点击和缩略图。
        let terms: Vec<String> = if searching {
            self.search.split_whitespace().map(str::to_owned).collect()
        } else {
            Vec::new()
        };
        let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        // 先取快照：渲染过程中不能一边读一边清，否则后面的条目判断就乱了。
        let scroll_to_selected = self.scroll_to_selected;
        // 本帧鼠标落在哪一行，循环结束后写回 `hover_row`（供下一帧绘制底色）。
        let mut next_hover = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if entries.is_empty() {
                    ui.centered_and_justified(|ui| {
                        let hint = if searching {
                            "没有匹配的记录"
                        } else if self.filter == KindFilter::All {
                            "还没有剪贴板历史，复制点东西试试。"
                        } else {
                            "这个类型还没有记录"
                        };
                        ui.label(hint);
                    });
                    return;
                }

                for entry in &entries {
                    let created = fmt_time(entry.created_at);
                    let is_selected = self.selected == Some(entry.id);

                    // hover 底色：目标值是上一帧的 hover 状态，插值由 egui 推进
                    // （它会在动画期间自动 request_repaint）。
                    let hover_prev = !locked && self.hover_row == Some(entry.id);
                    let hover_t = ui.ctx().animate_bool_with_time(
                        egui::Id::new(("row-hover", entry.id)),
                        hover_prev,
                        theme::ANIM_FAST,
                    );
                    let missing = self.missing_paths.get(&entry.id).copied().unwrap_or(0);

                    // 这一行是否落在可视区。缩略图排队靠它：解码很贵，
                    // 200 条历史一次性灌满解码队列会让翻页全在等它。
                    let probe = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), THUMB_H),
                    );
                    let visible = ui.is_rect_visible(probe);

                    // 搜索时优先显示命中上下文：长文本只画开头的话，
                    // 命中的词常常在 5000 字之后，看到的是一条完全不含
                    // 搜索词的内容——直觉就是「搜索没生效」。
                    let shown_src = self
                        .snippets
                        .get(&entry.id)
                        .map(String::as_str)
                        .unwrap_or(entry.preview.as_str());
                    let shown = truncate_chars(shown_src, PREVIEW_SHOW_CHARS);
                    let ranges = if term_refs.is_empty() {
                        Vec::new()
                    } else {
                        match_ranges(&shown, &term_refs)
                    };

                    // 图片条目：缩略图还没解好就先画占位块。
                    //
                    // 只有真正落在可视区里的行才排队解码——否则一打开列表，
                    // 200 条历史里的图片会一次性灌满解码队列，翻页全在等它。
                    let thumb = if entry.kind == Kind::Image && visible {
                        self.thumbs
                            .get(ui.ctx(), &self.read, entry.id, entry.byte_len)
                    } else {
                        None
                    };

                    // 半透明高亮：深浅主题都透出底色，不会把文字盖住。
                    let sel = ui.visuals().selection.bg_fill;
                    let bg = if is_selected {
                        Some(theme::wash(sel, 58))
                    } else if hover_t > 0.01 {
                        // hover 底比选中更淡，而且用文字色而非选中色：
                        // 选中是「这是当前项」，hover 只是「鼠标在这」，
                        // 两者可能同时出现，必须一眼分得清主次。
                        Some(theme::wash(
                            ui.visuals().text_color(),
                            (14.0 * hover_t).round() as u8,
                        ))
                    } else {
                        None
                    };

                    let mut frame = egui::Frame::new()
                        .inner_margin(egui::Margin {
                            left: theme::CARD_PAD_X,
                            right: theme::CARD_PAD_X,
                            top: theme::CARD_PAD_Y,
                            bottom: theme::CARD_PAD_Y,
                        })
                        .corner_radius(theme::RADIUS_MD);
                    if let Some(bg) = bg {
                        frame = frame.fill(bg);
                    }

                    // 卡片正文：元数据行 + 预览行。图片条目多一个左侧缩略图，
                    // 正文逻辑完全共用，不复制一份。
                    let body = |ui: &mut egui::Ui| {
                        ui.horizontal(|ui| {
                            if entry.pinned {
                                ui.label(egui::RichText::new("★").color(theme::PIN).size(12.0));
                            }
                            kind_chip(ui, entry.kind);
                            if missing > 0 {
                                ui.label(
                                    egui::RichText::new(format!("源文件已失效 ({missing})"))
                                        .color(theme::DANGER)
                                        .small(),
                                );
                            }
                            // 文本类显示字节数没有信息量，只在图片 / 文件类给体积。
                            if !matches!(entry.kind, Kind::Text | Kind::RichText) {
                                ui.label(
                                    egui::RichText::new(human_size(entry.byte_len))
                                        .weak()
                                        .small(),
                                );
                            }
                            if entry.hit_count > 0 {
                                ui.label(
                                    egui::RichText::new(format!("×{}", entry.hit_count))
                                        .weak()
                                        .small(),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    // 来源和时间拼成一个 label，而不是两个。
                                    // 右对齐布局里每多一个元素就多一份 item_spacing，
                                    // 420px 宽的卡片上这 5px 得省。
                                    let meta = if entry.app.is_empty() {
                                        // 老库里的条目没有来源，直接不显示，
                                        // 不拿「未知」占位——那只是噪音。
                                        created
                                    } else {
                                        format!("{} · {}", truncate_chars(&entry.app, 12), created)
                                    };
                                    ui.label(egui::RichText::new(meta).weak().small());
                                },
                            );
                        });
                        ui.label(preview_job(ui, &shown, &ranges, missing > 0));
                    };

                    let resp = frame
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if entry.kind == Kind::Image {
                                ui.horizontal(|ui| {
                                    match &thumb {
                                        Some(t) => thumb_image(ui, t),
                                        None => thumb_placeholder(ui),
                                    }
                                    ui.add_space(theme::GAP_MD);
                                    let width = ui.available_width();
                                    ui.vertical(|ui| {
                                        ui.set_width(width);
                                        body(ui);
                                    });
                                });
                            } else {
                                body(ui);
                            }
                        })
                        .response
                        .interact(egui::Sense::click());

                    if !locked {
                        if resp.double_clicked() {
                            self.on_activate(entry);
                        } else if resp.clicked() {
                            self.on_single_click(entry);
                        }
                    }

                    // 键盘移动选中项时把它带到视野中间。
                    //
                    // 必须显式传 `ScrollAnimation::none()`：默认的平滑滚动要靠
                    // 连续重绘推进动画，而这个窗口空闲时根本不重绘（省 CPU），
                    // 动画会卡在第一帧，表现得像「scroll_to 完全没生效」。
                    if scroll_to_selected && is_selected {
                        ui.scroll_to_rect_animation(
                            resp.rect,
                            Some(egui::Align::Center),
                            egui::style::ScrollAnimation::none(),
                        );
                    }

                    if resp.hovered() && !locked {
                        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                        next_hover = Some(entry.id);
                    }

                    // 选中项左侧一条强调竖条：浅色主题下光靠底色几乎看不出来。
                    if is_selected {
                        let bar = egui::Rect::from_min_size(
                            resp.rect.left_top() + egui::vec2(2.0, 4.0),
                            egui::vec2(3.0, (resp.rect.height() - 8.0).max(0.0)),
                        );
                        ui.painter().rect_filled(bar, 1.5, sel);
                    } else {
                        // 卡片之间一条极淡的分隔线，替代原来散乱的空白。
                        let y = resp.rect.bottom() + 2.0;
                        ui.painter().line_segment(
                            [
                                egui::pos2(resp.rect.left() + 8.0, y),
                                egui::pos2(resp.rect.right() - 8.0, y),
                            ],
                            ui.visuals().widgets.noninteractive.bg_stroke,
                        );
                    }
                    ui.add_space(theme::GAP_SM);
                }
            });

        self.hover_row = next_hover;

        // 本帧已经处理过滚动请求，清掉，避免之后每帧都把视图拽回选中项
        if scroll_to_selected {
            self.scroll_to_selected = false;
        }
    }

    /// 预览浮层：图片看大图，文本看全文。
    ///
    /// 盖在同一个窗口里而不是新开一个窗口：主窗口是置顶无边框小窗，
    /// 再开一个窗口就要处理「关掉一个另一个留在哪一层」的焦点问题，
    /// 而预览本来就是看一眼就走的东西，不值得引入第二套窗口生命周期。
    ///
    /// 配合 `ui_list` 里的 `locked`，预览期间下面的行既不响应点击也不显示手形光标。
    fn ui_preview(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.preview_id else {
            return;
        };
        // 条目可能在浮层开着的时候被删掉（Delete 走的是别处），取不到就收起。
        let Some(entry) = self.index.get(id).cloned() else {
            self.preview_id = None;
            return;
        };

        // 文本类走全文浮层。列表里那条预览只存了前 512 字（长文本连两行都
        // 显示不满），想看全只能在这里——不然「复制过的东西看不到全貌」。
        if matches!(entry.kind, Kind::Text | Kind::RichText) {
            let fade = self.overlay_fade(ui.ctx());
            if self.ui_text_view(ui, &entry, fade) {
                self.preview_id = None;
                self.text_view = None;
            }
            return;
        }

        let fade = self.overlay_fade(ui.ctx());

        // 解码请求每帧都要问一次，否则纹理解出来了也没人来取。
        let state = self
            .thumbs
            .preview(ui.ctx(), &self.read, entry.id, entry.byte_len);

        // 用 Area 而不是面板的 painter：面板画布被裁在 CentralPanel 里，
        // 盖不住顶部的搜索框和底部的状态栏，露出来的浅色边会一直分散注意力。
        // 独立 Area 在 Foreground 层，能铺满整个窗口，也天然接管点击。
        let screen = ui.ctx().content_rect();
        let avail = screen.shrink(12.0);
        let mut close = false;

        egui::Area::new(egui::Id::new("preview"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ui.ctx(), |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
                if resp.clicked() {
                    close = true;
                }

                // 压到接近全黑：列表还亮着的话，看图时文字和分隔线会一直往眼里钻。
                // 打开时按 `fade` 从透明压下来，避免整块黑屏突然盖住。
                let painter = ui.painter();
                painter.rect_filled(
                    rect,
                    0.0,
                    egui::Color32::from_black_alpha((244.0 * fade) as u8),
                );

                match &state {
                    PreviewState::Ready(thumb) => {
                        let size = fit_preview(thumb.source, avail.size());
                        let img = egui::Rect::from_center_size(avail.center(), size);
                        painter.image(
                            thumb.texture.id(),
                            img,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                        // 尺寸信息放左下角：看图时最常问的就是「这图多大」。
                        painter.text(
                            screen.left_bottom() + egui::vec2(10.0, -8.0),
                            egui::Align2::LEFT_BOTTOM,
                            format!(
                                "{}×{} · 空格 / Esc / 点击关闭",
                                thumb.source.x as i32, thumb.source.y as i32
                            ),
                            egui::FontId::proportional(11.0),
                            egui::Color32::from_gray(150),
                        );
                    }
                    PreviewState::Loading => {
                        painter.text(
                            screen.center(),
                            egui::Align2::CENTER_CENTER,
                            "解码中…",
                            egui::FontId::proportional(13.0),
                            egui::Color32::from_gray(180),
                        );
                    }
                    PreviewState::Failed => {
                        painter.text(
                            screen.center(),
                            egui::Align2::CENTER_CENTER,
                            "这张图无法预览（数据损坏或尺寸过大）",
                            egui::FontId::proportional(13.0),
                            egui::Color32::from_gray(180),
                        );
                    }
                }
            });

        if close {
            self.preview_id = None;
        }
    }

    /// 文本条目的全文浮层。返回 `true` 表示用户要求关闭。
    ///
    /// 为什么不把全文直接铺在列表里：512 字的预览折行后已经能吃掉大半个窗口，
    /// 八千字的正文会把列表冲得找不着北。列表负责「扫」，浮层负责「读」。
    fn ui_text_view(&mut self, ui: &mut egui::Ui, entry: &EntryMeta, fade: f32) -> bool {
        // 浮层里的高亮沿用列表那套：同一个搜索词，在任何地方都应该被标出来。
        let query = self.search.trim().to_owned();
        let terms: Vec<&str> = query.split_whitespace().collect();

        // 内容只从库里读一次。浮层开着时每帧重读一遍 payload 是纯粹的 IO 浪费，
        // 大条目还可能是一次 blob 文件读。
        let stale = self
            .text_view
            .as_ref()
            .is_none_or(|v| v.id != entry.id || v.query != query);
        if stale {
            let raw = self
                .read
                .load_payload(entry.id)
                .ok()
                .flatten()
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&raw);
            let (shown, hit, skipped, truncated) = text_view_window(&text, &terms, TEXT_VIEW_CHARS);
            self.text_view = Some(TextView {
                id: entry.id,
                query: query.clone(),
                text: shown,
                hit,
                skipped,
                truncated,
                scroll_to_hit: true,
            });
        }
        let view = self.text_view.as_ref().expect("上面刚填过");

        let ranges = match_ranges(&view.text, &terms);
        let scroll_to_hit = view.scroll_to_hit && view.hit.is_some();

        let screen = ui.ctx().content_rect();
        let mut close = false;

        egui::Area::new(egui::Id::new("text-view"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ui.ctx(), |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
                // 点空白处收起。点击落在下面的面板/滚动区上时由它们接管，
                // 所以不会「刚一拖动滚动条就关掉」。
                if resp.clicked() {
                    close = true;
                }
                ui.painter().rect_filled(
                    rect,
                    0.0,
                    egui::Color32::from_black_alpha((210.0 * fade) as u8),
                );

                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        // 卡片从下方 8px 处滑到位：位移比纯淡入更有「弹出」感，
                        // 8px 足够被感知又不会真的晃眼。
                        .max_rect(
                            screen
                                .shrink(10.0)
                                .translate(egui::vec2(0.0, (1.0 - fade) * 8.0)),
                        )
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );

                egui::Frame::new()
                    .fill(ui.visuals().window_fill)
                    .stroke(ui.visuals().window_stroke)
                    .corner_radius(theme::RADIUS_LG)
                    .inner_margin(10.0)
                    .show(&mut child, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            kind_chip(ui, entry.kind);
                            if view.skipped {
                                ui.label(egui::RichText::new("已从命中处开始").weak().small());
                            }
                            if view.truncated {
                                ui.label(egui::RichText::new("只显示前 32K 字").weak().small());
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} · 空格 / Esc 关闭",
                                            human_size(entry.byte_len)
                                        ))
                                        .weak()
                                        .small(),
                                    );
                                },
                            );
                        });
                        ui.separator();

                        // 剩余高度全给正文，滚动条只在这一块里生效。
                        // `auto_shrink` 纵向为真：短内容把卡片收到它自己的高度，
                        // 一条 85 字节的文本不该占满整屏。
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                // 自己排版再交给 painter：`scroll_to_rect` 要的是
                                // 「命中那一行在哪」，而这个位置只有 galley 知道。
                                // 走 `Fonts::layout_job` 和 Label 用的是同一份
                                // galley 缓存，不会重复排版。
                                let job = full_text_job(ui, &view.text, &ranges);
                                let galley = ui.fonts_mut(|f| f.layout_job(job));
                                let (rect, _) =
                                    ui.allocate_exact_size(galley.size(), egui::Sense::hover());
                                ui.painter().galley(
                                    rect.min,
                                    galley.clone(),
                                    ui.visuals().text_color(),
                                );

                                if scroll_to_hit && let Some(pos) = view.hit {
                                    let chars = view.text[..pos].chars().count();
                                    let hit =
                                        galley.pos_from_cursor(egui::text::CCursor::new(chars));
                                    let target = egui::Rect::from_min_size(
                                        rect.min + hit.min.to_vec2(),
                                        egui::vec2(rect.width().max(1.0), hit.height().max(1.0)),
                                    );
                                    ui.scroll_to_rect(target, Some(egui::Align::Center));
                                }
                            });
                    });
            });

        if scroll_to_hit && let Some(view) = self.text_view.as_mut() {
            view.scroll_to_hit = false;
        }
        close
    }

    fn ui_footer(&mut self, ui: &mut egui::Ui) {
        // 「确认清空」只在 3 秒内有效，过期自动收回，避免按钮一直挂在待确认态。
        let confirming = self
            .confirm_clear
            .map(|t| t.elapsed() < CLEAR_CONFIRM_TTL)
            .unwrap_or(false);

        // 这一栏和列表用同一个面板底色。原先在顶边画过一条发丝线，实际看是
        // 一道横贯整窗的硬边，比"分不清"更碍眼——去掉，靠上下 6px 留白分隔就够了。
        ui.horizontal(|ui| {
            // 左右两侧各自成组，中间的间距全由显式 `add_space` 控制：
            // 键帽和说明之间要 4px、组与组之间要 10px，默认 item_spacing 只能给一个值。
            ui.spacing_mut().item_spacing.x = 0.0;

            // 左侧：操作反馈，没有就显示快捷键提示
            let mut expired = false;
            match &self.toast {
                Some((msg, t)) if t.elapsed().as_secs() < 3 => {
                    // 前面点一颗强调色圆点：纯灰字在一屏灰字里没有"刚发生了什么"的语义，
                    // 而这一行本来就窄，加个图标也塞不下。
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 3.0, theme::ACCENT);
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(msg)
                            .size(theme::FONT_FOOT)
                            .color(ui.visuals().strong_text_color()),
                    );
                    // 到期时叫醒一帧，否则空闲不重绘、提示会一直挂着不走。
                    let left = std::time::Duration::from_secs(3) - t.elapsed();
                    ui.ctx().request_repaint_after(left);
                }
                Some(_) => expired = true,
                None => {
                    // 选中项有预览可看时换一套提示：空格是这一刻最有用、
                    // 也最不为人知的键，其余时候它根本不生效，一直挂着只会占地方。
                    let kind = self
                        .selected
                        .and_then(|id| self.index.get(id))
                        .map(|e| e.kind);
                    let hint: &[(&str, &str)] = match kind {
                        Some(Kind::Image) => {
                            &[("空格", "看大图"), ("Enter", "粘贴"), ("Alt+1~9", "直达")]
                        }
                        Some(Kind::Text | Kind::RichText) => {
                            // 文本类把 `Alt+1~9` 换成 `Ctrl+Enter`：数字直达是
                            // 熟练后才用的，而"粘出去多一个空行"每天都会发生。
                            // 一栏只放得下三组，得挑当下的信息量。
                            &[
                                ("空格", "看全文"),
                                ("Enter", "粘贴"),
                                ("Ctrl+Enter", "去尾空行"),
                            ]
                        }
                        _ => &[
                            ("↑↓", "选择"),
                            ("Alt+1~9", "直达"),
                            ("Del", "删除"),
                            ("Ctrl+P", "收藏"),
                        ],
                    };
                    for (i, (key, desc)) in hint.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(theme::GAP_MD + 2.0);
                        }
                        keycap(ui, key);
                        ui.add_space(4.0);
                        // 说明文字比 `weak_text_color` 再深一点：它是这一栏里
                        // 唯一的连续阅读内容，纯弱化色在 12px 下会掉出可读范围。
                        ui.label(
                            egui::RichText::new(*desc)
                                .size(theme::FONT_FOOT)
                                .color(blend(
                                    ui.visuals().weak_text_color(),
                                    ui.visuals().text_color(),
                                    0.45,
                                )),
                        );
                    }
                }
            }
            if expired {
                self.toast = None;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if confirming {
                    "确认清空？"
                } else {
                    "清空历史"
                };
                if footer_button(ui, egui::Id::new("footer-clear"), label, confirming).clicked() {
                    if confirming {
                        self.confirm_clear = None;
                        if self.tx.send(Command::Clear).is_ok() {
                            // 收藏项保留，所以本地只摘掉未收藏的。
                            self.entries.retain(|e| e.pinned);
                            self.cached_results.retain(|e| e.pinned);
                            self.index.retain_pinned();
                            self.counts = self.index.counts();
                            self.cached_key = None;
                            self.selected = None;
                            self.show_toast("已清空历史（收藏项保留）".to_owned());
                        }
                    } else {
                        self.confirm_clear = Some(std::time::Instant::now());
                        // 到期要把标签恢复成「清空历史」，得先安排一次重绘。
                        ui.ctx().request_repaint_after(CLEAR_CONFIRM_TTL);
                    }
                }
            });
        });
    }
}

impl eframe::App for App {
    /// 全透明：让窗口背后的系统材质透上来。
    ///
    /// 面板自己画半透明底色，所以文字对比度仍然受我们控制——这里只是把
    /// 「最底层」交给 DWM，而不是自己铺一块纯色。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 隐藏窗口正常不会有 RedrawRequested，但万一有也不该重建整个列表。
        if !win::is_main_window_visible() {
            return;
        }

        // 托盘「设置」：listener 已经把窗口唤出来了，这里补上面板状态。
        // 必须早于 `tick_focus`——否则这一帧的「刚获得焦点 → 聚焦搜索框」
        // 会先把焦点塞给搜索框，面板刚开就要再抢一次。
        if take_settings_request() {
            self.open_settings();
            ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("search")));
        }
        if take_about_request() {
            self.open_about();
            ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("search")));
        }

        self.tick_focus(ctx);
        self.poll_notify();
        self.sync_search_state();
        // 录制热键时，系统级热键会被系统先吞掉（WM_HOTKEY 直接进 listener），
        // 那时候再去 toggle 窗口就会「录到一半窗口消失」。所以状态同步给 listener。
        hotkey::set_capturing(self.recording);
        self.handle_keys(ctx);

        // 浮层显示的是「某一条」。选中项换人或被删掉之后继续盖着，
        // 会让人以为浮层里的图就是眼前选中的那条——直接收起。
        if self.preview_id.is_some_and(|id| self.selected != Some(id)) {
            self.preview_id = None;
        }

        // 点窗口关闭按钮不退出，收进托盘。先 CancelClose 阻止 eframe 销毁窗口，
        // 再用 Win32 把它藏起来。退出走托盘菜单（由 listener 直接处理）。
        // 浮层打开的時刻只记一次，用于淡入进度。收起来时清掉，下次重新计时。
        if self.preview_id.is_some() {
            self.overlay_opened_at
                .get_or_insert_with(|| ctx.input(|i| i.time));
        } else {
            self.overlay_opened_at = None;
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            win::hide_main_window();
        }

        // `show_separator_line(false)`：egui 的面板默认会在自己的边缘画一条
        // `bg_stroke` 分隔线，和有没有 `frame` 无关。底栏那条横贯整窗、贴在
        // 快捷键上方 6px 处，像条多余的横线——而顶栏本来就有显式的 `ui.separator()`，
        // 两条叠着反而更粗。两边都关掉，线只由内容自己画。
        egui::TopBottomPanel::top("header")
            .show_separator_line(false)
            .show(ctx, |ui| self.ui_header(ui));
        // 底部栏高度写死：键帽和按钮都是 `KEY_H` 高，居中需要上下各留 6px；
        // 靠内容撑高的话每种状态高度都不一样，窗口会跟着抖。
        egui::TopBottomPanel::bottom("footer")
            .exact_height(theme::FOOTER_H)
            .show_separator_line(false)
            .frame(
                egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin {
                    left: theme::FOOTER_PAD_X,
                    right: theme::FOOTER_PAD_X,
                    top: 6,
                    bottom: 6,
                }),
            )
            .show(ctx, |ui| self.ui_footer(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            self.ui_list(ui);
            self.ui_preview(ui);
        });

        // 设置面板盖在最上层（Foreground），要最后画，否则会被面板内容盖住。
        self.ui_settings(ctx);
        // 关于面板同理，且它和设置互斥，谁开着就画谁。
        self.ui_about(ctx);
        // 尺寸落盘放在最后：这一帧的 `content_rect` 包含了本轮所有视口命令的结果。
        self.persist_window_size(ctx);
    }
}

/// 两个颜色按 `t` 线性混合（含 alpha）。
///
/// 用来做颜色过渡。egui 只提供给数值插值的接口，颜色插值得自己做——
/// 但它比直接切颜色舒服得多，尤其「淡底 + 淡字」需要一起变的时候。
fn blend(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    egui::Color32::from_rgba_unmultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        mix(a.a(), b.a()),
    )
}

/// 把某个字符从本帧的文本事件里摘掉。
///
/// `consume_key` 只删 `Event::Key`，而输入框吃的是 `Event::Text`。组合键里的
/// 可打印键（空格、Alt+数字）同时产生两者，不删干净的话键是"吞"了，
/// 字符照样落进搜索框——顺手把列表筛成一个莫名其妙的结果。
fn drop_text(ctx: &egui::Context, ch: &str) {
    ctx.input_mut(|i| {
        i.events
            .retain(|e| !matches!(e, egui::Event::Text(t) if t == ch));
    });
}

/// 把某个键在这一帧积压的所有按下事件都消费掉，返回次数。
///
/// 上限 20 次纯粹是防御——正常一帧不可能攒这么多。
fn drain_keys(ctx: &egui::Context, key: egui::Key) -> usize {
    let mut n = 0;
    while n < 20 && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key)) {
        n += 1;
    }
    n
}

/// 顶部那一排类型筛选 chip。返回用户点中的档位（没有点击则 `None`）。
fn filter_chips(ui: &mut egui::Ui, current: KindFilter, counts: Counts) -> Option<KindFilter> {
    let items = [
        ("全部", KindFilter::All, counts.all, theme::KIND_ALL),
        ("文本", KindFilter::Text, counts.text, theme::KIND_TEXT),
        ("图片", KindFilter::Image, counts.image, theme::KIND_IMAGE),
        ("文件", KindFilter::Files, counts.files, theme::KIND_FILES),
    ];

    let mut picked = None;
    for (label, filter, n, tint) in items {
        if filter_chip(ui, label, n, current == filter, tint) {
            picked = Some(filter);
        }
    }
    picked
}

/// 单个筛选 chip。选中时用实色底 + 白字，一眼能看出当前在哪一档；
/// 未选中只给一层极淡同色底，不抢列表的注意力。
fn filter_chip(
    ui: &mut egui::Ui,
    label: &str,
    n: usize,
    active: bool,
    tint: egui::Color32,
) -> bool {
    // 选中态切换做一次插值：chip 是最高频点击的控件，瞬间换色显得生硬。
    // `animate_bool_with_time` 对一直存在的 id 才有意义——chip 恰好符合。
    let anim = ui.ctx().animate_bool_with_time(
        ui.make_persistent_id(("chip", label)),
        active,
        theme::ANIM_BASE,
    );

    let (idle_fill, idle_color) = if n == 0 {
        // 没内容的档位弱化，但仍可点——点了能看到「这个类型还没有记录」。
        (egui::Color32::TRANSPARENT, ui.visuals().weak_text_color())
    } else {
        (theme::wash(tint, 24), tint)
    };
    let fill = blend(idle_fill, theme::ACCENT, anim);
    let color = blend(idle_color, egui::Color32::WHITE, anim);
    // 描边只在未选中时存在，跟着一起淡出，不然选中后会留一圈深色边。
    let stroke = if n > 0 && anim < 0.99 {
        egui::Stroke::new(
            1.0,
            blend(theme::wash(tint, 60), egui::Color32::TRANSPARENT, anim),
        )
    } else {
        egui::Stroke::NONE
    };

    let resp = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(theme::RADIUS_PILL)
        .inner_margin(egui::Margin {
            left: theme::CHIP_PAD_X,
            right: theme::CHIP_PAD_X,
            top: theme::CHIP_PAD_Y,
            bottom: theme::CHIP_PAD_Y,
        })
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("{label} {n}"))
                    .small()
                    .strong()
                    .color(color),
            );
        })
        .response
        .interact(egui::Sense::click());

    if resp.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// 一个「键帽」：淡底 + 一圈发丝边 + 12px 字。
///
/// 底部快捷键原先是整行灰字，11px 弱化色贴在窗口最下沿，几乎看不清哪个键
/// 对应哪句话。拆成键帽之后每个按键有自己的边界，扫一眼就能对上是哪个键；
/// 而且它和筛选 chip 共用「淡底 + 描边」这套语言，不像另一种风格硬拼进来的。
///
/// 高度和字号都与 `footer_button` 对齐（`KEY_H` + `FONT_FOOT`），两者才能
/// 落在同一条视觉基线上。
///
/// 颜色从 `text_color()` 推：浅色主题下文字近黑，淡黑底正好是浅灰卡片；
/// 深色主题下文字近白，同一段代码给出的是浅色浮起块。不写死 RGB。
fn keycap(ui: &mut egui::Ui, key: &str) -> egui::Response {
    keycap_sized(ui, key, theme::FONT_FOOT, theme::KEY_H)
}

/// 同 `keycap`，但字号和高度由调用方给。
///
/// 底部状态栏要「和按钮一样高」（`KEY_H`），设置面板里的热键展示则要更醒目
/// 一档——同一个视觉元素，两种尺寸需求，所以把尺寸抽成参数。
fn keycap_sized(ui: &mut egui::Ui, key: &str, font_size: f32, height: f32) -> egui::Response {
    let color = ui.visuals().text_color();
    let font = egui::FontId::new(font_size, egui::FontFamily::Proportional);
    let galley = ui.painter().layout_no_wrap(key.to_owned(), font, color);
    let size = egui::vec2(galley.size().x.ceil() + theme::KEY_PAD_X * 2.0, height);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());

    let painter = ui.painter();
    painter.rect_filled(rect, theme::RADIUS_SM, theme::wash(color, 30));
    painter.rect_stroke(
        rect,
        theme::RADIUS_SM,
        egui::Stroke::new(1.0, theme::wash(color, 52)),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.center() - galley.size() * 0.5, galley, color);
    resp
}

/// 状态栏和设置面板里的按钮：平时只有描边（ghost），hover 才浮起。
///
/// 不用 `ui.button`：默认按钮是实心填充，在 32px 的窄带里像一块补丁。
/// `solid` 表示「已经在问你了」的状态（清空历史的二次确认），实心 + 白字——
/// 破坏性操作在按下之前就该换一副面孔，而不是等按完再说。
///
/// `accent` 决定强调色：破坏性操作给 `DANGER`，普通操作给 `ACCENT`。
fn ghost_button(
    ui: &mut egui::Ui,
    id: egui::Id,
    label: &str,
    accent: egui::Color32,
    solid: bool,
) -> egui::Response {
    // 先量字、再占位、最后按 hover 上色：按钮尺寸由文字决定，颜色却要等
    // 交互结果才知道。用 `PLACEHOLDER` 排版 + 绘制时覆盖颜色绕开这个顺序。
    let font = egui::FontId::new(theme::FONT_FOOT, egui::FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, egui::Color32::PLACEHOLDER);
    let size = egui::vec2(galley.size().x.ceil() + 20.0, theme::KEY_H);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());

    let hover = ui.ctx().animate_bool_with_time(
        id.with("hover"),
        resp.hovered() && !solid,
        theme::ANIM_FAST,
    );
    let text = ui.visuals().text_color();
    let (fill, stroke, color) = if solid {
        (accent, egui::Stroke::NONE, egui::Color32::WHITE)
    } else {
        (
            theme::wash(accent, (20.0 * hover) as u8),
            egui::Stroke::new(
                1.0,
                blend(theme::wash(text, 46), theme::wash(accent, 110), hover),
            ),
            blend(ui.visuals().weak_text_color(), accent, hover),
        )
    };

    let painter = ui.painter();
    painter.rect_filled(rect, theme::RADIUS_SM, fill);
    if stroke != egui::Stroke::NONE {
        painter.rect_stroke(rect, theme::RADIUS_SM, stroke, egui::StrokeKind::Inside);
    }
    painter.galley_with_override_text_color(rect.center() - galley.size() * 0.5, galley, color);

    if resp.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    resp
}

/// 底部状态栏的「清空历史」：红系的 `ghost_button`。
fn footer_button(ui: &mut egui::Ui, id: egui::Id, label: &str, confirming: bool) -> egui::Response {
    ghost_button(ui, id, label, theme::DANGER, confirming)
}

/// 设置面板里的小标题。
fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(theme::FONT_FOOT)
            .color(blend(
                ui.visuals().weak_text_color(),
                ui.visuals().text_color(),
                0.5,
            )),
    );
}

/// 设置面板里的说明文字：弱化、小一号，但不到看不清。
fn hint_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(theme::FONT_SM).weak());
}

/// 可点击的一行文字（关于面板里的路径、网址）。
///
/// 不画下划线也不做成按钮：这里是「顺手能点」的辅助信息，画成按钮会跟
/// 面板底部的「关闭」抢注意力。hover 变手型 + 颜色加深，足够说明它能点。
fn link_label(ui: &mut egui::Ui, text: &str, accent: egui::Color32) -> egui::Response {
    let resp = ui.add(
        egui::Label::new(egui::RichText::new(text).size(theme::FONT_SM).color(accent))
            .sense(egui::Sense::click())
            .truncate(),
    );
    if resp.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    resp
}

/// 列表里的预览行：等宽字体、最多 `PREVIEW_ROWS` 行、超出用省略号收尾。
///
/// 用 `LayoutJob` 而不是给 `RichText` 加样式，有两个原因：
/// 1. `RichText` 只能整条一个颜色，而这里要的是「同一条里只有几个词变色」；
/// 2. `max_rows` 这个旋钮只存在于 `LayoutJob` 的 `TextWrapping` 上。
///
/// 代价是每帧都要算一次 job 的哈希再查 galley 缓存（命中缓存就不重排），
/// 所以调用方会先把文本截到 `PREVIEW_SHOW_CHARS` 以内。
fn preview_job(
    ui: &egui::Ui,
    text: &str,
    ranges: &[(usize, usize)],
    dim: bool,
) -> egui::text::LayoutJob {
    let mut job = text_job(ui, text, ranges, dim);
    job.wrap.max_rows = PREVIEW_ROWS;
    job.wrap.overflow_character = Some('…');
    job
}

/// 全文浮层用：不限制行数，其余排版规则和列表一致。
fn full_text_job(ui: &egui::Ui, text: &str, ranges: &[(usize, usize)]) -> egui::text::LayoutJob {
    text_job(ui, text, ranges, false)
}

fn text_job(
    ui: &egui::Ui,
    text: &str,
    ranges: &[(usize, usize)],
    dim: bool,
) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};

    let visuals = ui.visuals();
    let mut job = LayoutJob::default();
    job.wrap.max_width = ui.available_width();

    let color = if dim {
        visuals.weak_text_color()
    } else {
        visuals.text_color()
    };
    let base = TextFormat {
        font_id: egui::FontId::monospace(13.0),
        color,
        ..Default::default()
    };

    // 命中部分只加一层半透明底色，不换字色：底色由主题决定，深浅主题都不会
    // 出现「亮底浅字」这种对比度塌掉的情况。
    let sel = visuals.selection.bg_fill;
    let mut hl = base.clone();
    hl.background = egui::Color32::from_rgba_unmultiplied(sel.r(), sel.g(), sel.b(), 120);

    let mut cursor = 0;
    for &(s, e) in ranges {
        let s = s.max(cursor);
        if s >= e || e > text.len() {
            continue;
        }
        if s > cursor {
            job.append(&text[cursor..s], 0.0, base.clone());
        }
        job.append(&text[s..e], 0.0, hl.clone());
        cursor = e;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, base);
    }
    job
}

/// 全文浮层要渲染的那一段：有搜索词就从命中处往前退一点开始。
///
/// 返回 `(片段, 前面是否还有, 后面是否还有)`。为什么要切片而不是整段渲染：
/// 单条内容上限 64MB，整段丢进 `LayoutJob` 会把界面卡住；而且命中点可能在
/// 几十屏之后，从头铺开等于让用户自己再找一遍。
fn text_view_window(
    text: &str,
    terms: &[&str],
    max_chars: usize,
) -> (String, Option<usize>, bool, bool) {
    /// 命中点前面留多少字做上下文。
    const LEAD_CHARS: usize = 240;

    let total = text.chars().count();
    let hit = crate::search::find_hit(text, terms);
    let mut start_byte = 0;
    if total > max_chars
        && let Some(pos) = hit
    {
        let before = text[..pos].chars().count();
        let back = before.saturating_sub(LEAD_CHARS);
        start_byte = char_offset(text, back);
        // 尽量从行首开始：把一行代码或一句话切成两半很难读。
        if let Some(nl) = text[..start_byte].rfind('\n') {
            start_byte = nl + 1;
        }
    }

    let shown: String = text[start_byte..].chars().take(max_chars).collect();
    let hit_in_view = hit.and_then(|pos| pos.checked_sub(start_byte));
    let skipped = start_byte > 0;
    let head_chars = text[..start_byte].chars().count();
    let truncated = total > head_chars + shown.chars().count();
    (shown, hit_in_view, skipped, truncated)
}

/// 第 `n` 个字符所在的字节偏移；越界时返回 `s.len()`。
fn char_offset(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

/// 按字符截断，超出的部分用省略号补上——「还有下文」必须让用户看得见。
fn truncate_chars(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    match s.char_indices().nth(max) {
        None => std::borrow::Cow::Borrowed(s),
        Some((idx, _)) => {
            let mut out = String::with_capacity(idx + 3);
            out.push_str(&s[..idx]);
            out.push('…');
            std::borrow::Cow::Owned(out)
        }
    }
}

/// 类型徽标：淡色胶囊 + 同色文字，比一坨彩色小字更容易扫读。
///
/// 底色用同色的低 alpha，而不是实色：这样深浅主题都能自动适配，
/// 文字始终用实色，对比度不会被背景吃掉。
fn kind_chip(ui: &mut egui::Ui, kind: Kind) {
    let (label, tint) = match kind {
        Kind::Text => ("文本", theme::KIND_TEXT),
        Kind::RichText => ("富文本", theme::KIND_RICH),
        Kind::Image => ("图片", theme::KIND_IMAGE),
        Kind::Files => ("文件", theme::KIND_FILES),
    };
    let fill = theme::wash(tint, 26);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(theme::RADIUS_SM)
        .inner_margin(egui::Margin {
            left: 6,
            right: 6,
            top: 1,
            bottom: 1,
        })
        .show(ui, |ui| {
            ui.label(egui::RichText::new(label).color(tint).strong().small());
        });
}

/// 人可读的体积。文本类不显示，只有图片 / 文件才用到。
/// 算缩略图在列表里的显示尺寸：高度固定，宽度按比例算、超过上限就截。
///
/// 小图会放大到填满高度——列表里缩略图高度一致才好横向比较，
/// 一列大小不一的方块反而更乱。
fn fit_thumb(source: egui::Vec2) -> egui::Vec2 {
    if source.x <= 0.0 || source.y <= 0.0 {
        return egui::vec2(THUMB_MAX_W, THUMB_H);
    }
    let scale = (THUMB_H / source.y).min(THUMB_MAX_W / source.x);
    egui::vec2(
        (source.x * scale).clamp(1.0, THUMB_MAX_W),
        (source.y * scale).clamp(1.0, THUMB_H),
    )
}

/// 算大图预览的显示尺寸：等比撑满可用区域。
///
/// 和 `fit_thumb` 不同，这里**不限制放大**。缩略图要求高度整齐才好横向比较，
/// 而预览的目的就是把小图放大到能看清——用户按空格就是想看大的。
fn fit_preview(source: egui::Vec2, avail: egui::Vec2) -> egui::Vec2 {
    if source.x <= 0.0 || source.y <= 0.0 {
        return avail;
    }
    let scale = (avail.x / source.x).min(avail.y / source.y);
    egui::vec2(source.x * scale, source.y * scale)
}

/// 画一张缩略图，并描一圈极淡的边——浅色截图贴在浅色背景上会糊成一片。
fn thumb_image(ui: &mut egui::Ui, thumb: &Thumb) {
    let size = fit_thumb(thumb.source);
    let resp = ui.add(
        egui::Image::new(&thumb.texture)
            .fit_to_exact_size(size)
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_XS as u8)),
    );
    let stroke = egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    ui.painter().rect_stroke(
        resp.rect,
        theme::RADIUS_XS,
        stroke,
        egui::StrokeKind::Inside,
    );
}

/// 缩略图还没解出来时的占位块。尺寸按最宽的形态撑开，
/// 解码完成时最多只是变窄一点，不会把整行推着跳。
fn thumb_placeholder(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(THUMB_MAX_W, THUMB_H), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, theme::RADIUS_XS, ui.visuals().faint_bg_color);
}

fn human_size(n: i64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// `12345` → `12 345`。上限动辄五位数，不加分隔用户得数位数才能分清
/// 1 000 和 10 000。用空格而不是逗号：中文界面里逗号容易被读成句读。
fn thousands(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// 数一下文件条目的路径列表里有多少个已经不存在了。
///
/// 网络路径（`\\server\share\...`）**不查**：目标离线时底层 `GetFileAttributesW`
/// 会等 SMB 超时几十秒，而这里跑在 UI 线程上，会把整个窗口卡死。
/// 这类条目保守地当作「还在」，宁可不提醒也不能卡界面。
fn count_missing_paths(payload: &[u8]) -> usize {
    let text = String::from_utf8_lossy(payload);
    text.lines()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| !p.starts_with("\\\\") && !p.starts_with("//"))
        .filter(|p| !std::path::Path::new(p).exists())
        .count()
}

/// 把 unix 毫秒格式化成**本地时间**。
///
/// 不能直接对时间戳取模算时分：那是 UTC，界面上会显示成差 8 小时的时间，
/// 让人以为记录错位了。这里借 Win32 的 FILETIME 转换，夏令时之类的也一并交给系统。
///
/// 当天的只显示时分，更早的补上月日——列表里光看 `14:03` 分不清是哪天记的。
fn fmt_time(ms: i64) -> String {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;

    if ms <= 0 {
        return "--:--".to_owned();
    }

    /// 1601-01-01 到 1970-01-01 之间的 100ns 数
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;

    let ticks = EPOCH_DIFF + (ms as u64) * 10_000;
    let utc = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };

    unsafe {
        let mut local: FILETIME = std::mem::zeroed();
        if FileTimeToLocalFileTime(&utc, &mut local) == 0 {
            return "--:--".to_owned();
        }

        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&local, &mut st) == 0 {
            return "--:--".to_owned();
        }

        let mut now: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut now);

        let same_day = st.wYear == now.wYear && st.wMonth == now.wMonth && st.wDay == now.wDay;
        if same_day {
            format!("{:02}:{:02}", st.wHour, st.wMinute)
        } else {
            format!(
                "{:02}-{:02} {:02}:{:02}",
                st.wMonth, st.wDay, st.wHour, st.wMinute
            )
        }
    }
}

/// 把库里的 payload 转成「写回剪贴板用」的字节。
///
/// 单独拎成自由函数是为了能直接测：这是全流程里**唯一会改动用户内容**的地方，
/// 裁错一个字就是把用户复制的东西弄坏了，必须有测试兜着。
///
/// 文本往返一次 CRLF（存的时候归一成 LF，写回时换回来），因此这里对原始
/// payload 做的是「归一化状态下的裁剪 + denormalize」，两步顺序不能换。
fn paste_payload(kind: Kind, payload: Vec<u8>, mode: PasteMode) -> Vec<u8> {
    match kind {
        Kind::Text | Kind::RichText => {
            let text = String::from_utf8_lossy(&payload);
            let text = match mode {
                // 只裁尾部：开头的缩进是内容的一部分（代码、YAML、引用的邮件），
                // 动了就废了。尾部空白则几乎总是「复制时顺带扫进来的」。
                PasteMode::Trimmed => text.trim_end().to_owned(),
                PasteMode::Raw => text.into_owned(),
            };
            denormalize_newlines(&text).into_bytes()
        }
        _ => payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn fmt_time_today_is_hh_mm() {
        // 当天记录只显示时分：5 个字符
        let s = fmt_time(now_ms());
        assert_eq!(s.len(), 5, "当天应显示 HH:MM，实际是 {s:?}");
        assert_eq!(&s[2..3], ":");
    }

    #[test]
    fn fmt_time_older_shows_date() {
        // 3 天前带月日：MM-DD HH:MM，11 个字符
        let old = now_ms() - 3 * 86_400_000;
        let s = fmt_time(old);
        assert_eq!(s.len(), 11, "跨天应显示 MM-DD HH:MM，实际是 {s:?}");
        assert_eq!(&s[2..3], "-");
    }

    #[test]
    fn fmt_time_is_local_not_utc() {
        // 关键回归：以前直接对时间戳取模，显示的是 UTC，比本地慢 8 小时。
        // 拿当前时间去比，本地时和 UTC 时至少有一个字段应该不同（除非恰好在整点边界）。
        let ms = now_ms();
        let shown = fmt_time(ms);
        let hour: u32 = shown[..2].parse().expect("时");

        let mut now: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut now) };

        // 允许跨分钟边界造成的误差，只比小时
        let diff = (hour as i32 - now.wHour as i32).abs();
        assert!(
            diff <= 1,
            "显示的小时 {hour} 和本地小时 {} 差太多，说明还在用 UTC",
            now.wHour
        );
    }

    #[test]
    fn fmt_time_handles_bad_input() {
        assert_eq!(fmt_time(0), "--:--");
        assert_eq!(fmt_time(-1), "--:--");
    }

    #[test]
    fn count_missing_paths_detects_deleted_file() {
        let dir =
            std::env::temp_dir().join(format!("rpaste-paths-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let existing = dir.join("here.txt");
        let gone = dir.join("gone.txt");
        std::fs::write(&existing, b"x").expect("写临时文件");
        let _ = std::fs::remove_file(&gone);

        let payload = format!("{}\n{}\n", existing.display(), gone.display());
        assert_eq!(count_missing_paths(payload.as_bytes()), 1);

        std::fs::remove_file(&existing).expect("删临时文件");
        assert_eq!(count_missing_paths(payload.as_bytes()), 2);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn count_missing_paths_skips_unc() {
        // UNC 不能真的 stat：离线时会等 SMB 超时，把 UI 卡死。
        assert_eq!(
            count_missing_paths(b"\\\\offline-server\\share\\a.txt\n"),
            0
        );
    }

    #[test]
    fn thumb_fit_keeps_aspect_and_caps_width() {
        // 竖图：高度撑满，宽度按比例
        let tall = fit_thumb(egui::vec2(600.0, 1200.0));
        assert!((tall.y - THUMB_H).abs() < 0.01, "高度应撑满: {tall:?}");
        assert!((tall.x - 22.0).abs() < 0.1, "宽度应按比例: {tall:?}");

        // 超宽图：宽度封顶，高度按比例，不能被压成一条缝
        let wide = fit_thumb(egui::vec2(4000.0, 1000.0));
        assert!((wide.x - THUMB_MAX_W).abs() < 0.01, "宽度应封顶: {wide:?}");
        assert!((wide.y - 24.0).abs() < 0.1, "高度应按比例: {wide:?}");

        // 退化输入不能出现 0 或 NaN 尺寸
        let bad = fit_thumb(egui::vec2(0.0, 0.0));
        assert!(bad.x > 0.0 && bad.y > 0.0, "{bad:?}");
    }

    #[test]
    fn preview_fit_fills_available_and_keeps_aspect() {
        let avail = egui::vec2(396.0, 536.0);

        // 宽图：宽度撑满，高度按比例
        let wide = fit_preview(egui::vec2(1009.0, 713.0), avail);
        assert!((wide.x - avail.x).abs() < 0.1, "宽图应撑满宽度: {wide:?}");
        assert!(
            (wide.x / wide.y - 1009.0 / 713.0).abs() < 1e-3,
            "宽高比要保持"
        );

        // 高图：高度受限
        let tall = fit_preview(egui::vec2(400.0, 2000.0), avail);
        assert!((tall.y - avail.y).abs() < 0.1, "高图应撑满高度: {tall:?}");
        assert!(tall.x < avail.x, "高图不该撑满宽度: {tall:?}");

        // 小图要放大，不保留原始像素尺寸——预览的意义就是看大
        let small = fit_preview(egui::vec2(80.0, 60.0), avail);
        assert!(small.x > 80.0, "小图应被放大: {small:?}");

        // 退化输入退回可用区域，不能给出 0 尺寸
        let bad = fit_preview(egui::Vec2::ZERO, avail);
        assert_eq!(bad, avail);
    }
    #[test]
    fn text_view_window_starts_at_the_hit() {
        // 命中距离开头很远：窗口要从命中处往前退一点开始，而不是从头发。
        let text = format!(
            "{}{}{}",
            "铺垫".repeat(2000),
            "命中目标",
            "收尾".repeat(2000)
        );
        let (shown, hit, skipped, truncated) = text_view_window(&text, &["命中目标"], 1000);
        assert!(skipped, "前面还有几千字，得让人知道");
        assert!(truncated, "后面也还有");
        assert!(shown.contains("命中目标"));
        assert!(shown.chars().count() <= 1000);
        // 命中偏移要落在切出来的这段里，浮层才有东西可滚。
        let hit = hit.expect("应该有命中位置");
        assert!(shown.is_char_boundary(hit) && shown[hit..].starts_with("命中目标"));
    }

    #[test]
    fn text_view_window_keeps_short_text_whole() {
        let (shown, hit, skipped, truncated) = text_view_window("一段短文本", &["短"], 1000);
        assert_eq!(shown, "一段短文本");
        assert!(!skipped && !truncated);
        assert_eq!(hit, Some("一段".len()), "字节偏移，不是字符序号");
    }

    #[test]
    fn trimmed_paste_drops_trailing_blank_lines() {
        // 从终端复制命令的典型形态：一条命令 + 两个换行。
        // 不裁的话粘回终端会立刻执行，粘进聊天框会多出空行。
        let raw = b"git status --short\n\n".to_vec();
        let out = paste_payload(Kind::Text, raw, PasteMode::Trimmed);
        assert_eq!(String::from_utf8(out).unwrap(), "git status --short");
    }

    #[test]
    fn trimmed_paste_keeps_leading_indent() {
        // 缩进是内容本身：YAML、Python、引用的邮件全依赖它。
        let raw = b"  key: value\n  other: value\n\n".to_vec();
        let out = paste_payload(Kind::Text, raw, PasteMode::Trimmed);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "  key: value\r\n  other: value",
            "只裁尾部，且换行换成 CRLF"
        );
    }

    #[test]
    fn raw_paste_keeps_everything() {
        let raw = b"line\n\n".to_vec();
        let out = paste_payload(Kind::Text, raw, PasteMode::Raw);
        assert_eq!(String::from_utf8(out).unwrap(), "line\r\n\r\n");
    }

    #[test]
    fn trimmed_paste_leaves_binary_untouched() {
        // 图片和文件不该被当文本处理，否则 payload 直接被改写坏掉。
        let png = vec![0x89, b'P', b'N', b'G', 0x00, 0x0a, 0x1a, 0x0a];
        let out = paste_payload(Kind::Image, png.clone(), PasteMode::Trimmed);
        assert_eq!(out, png);
    }
}
