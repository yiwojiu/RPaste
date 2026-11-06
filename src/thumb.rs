//! 图片条目的缩略图：后台线程解码 + 纹理缓存。
//!
//! 两个约束决定了这个结构：
//!
//! 1. **不能在 UI 线程解码**。一张 1300×800 的 PNG 解出来要十几毫秒，
//!    滚动时每帧解一张就是肉眼可见的掉帧；一帧解完所有可见项更不可能。
//!    这里用一个常驻线程串行解，UI 只把结果上传成纹理。
//! 2. **不能缓存原图纹理**。历史里要是塞了几十张 2K 截图，按原尺寸上传
//!    就是几个 GB 的显存。列表统一缩到 128px 以内，一张 64 KB，上限 48 张；
//!    大图预览是单独一档（1024px），只留最近一张。
//!
//! 缓存键是 `(id, byte_len, side)` 而不是单纯的 id：SQLite 的 rowid 在删掉
//! 最新一条之后会被下一条复用，只认 id 会让新图片顶着旧缩略图显示；
//! `side` 则区分「列表缩略图」和「预览大图」这两个尺寸。

use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::store::ReadStore;

/// 缩略图最长边。列表里只显示到 44px，留 3 倍余量做高质量缩放。
const THUMB_SIDE: u32 = 128;
/// 大图预览的最长边。
///
/// 窗口本身只有 420×560，放大后也到不了这个尺寸；定 1024 是给「窗口拉大 +
/// 高 DPI」留余量。再往上涨一分辨率就多付四分显存和成倍解码时间，
/// 屏幕上却看不出区别。
const PREVIEW_SIDE: u32 = 1024;
/// 同时缓存的缩略图张数。128px RGBA 一张 64 KB，48 张约 3 MB。
const MAX_CACHE: usize = 48;
/// 在途解码任务上限。超过就不再排新任务，避免快速滚动时把队列堆满。
const MAX_INFLIGHT: usize = 4;
/// 超过这个像素数直接放弃解码。8K 截图约 3300 万，仍然在范围内；
/// 再大的基本是拼图或者异常文件，解出来光位图就几百 MB。
const MAX_PIXELS: u64 = 40_000_000;

#[derive(Clone)]
pub struct Thumb {
    pub texture: egui::TextureHandle,
    /// **原图**尺寸（不是纹理尺寸），用来算显示时的宽高比。
    pub source: egui::Vec2,
}

/// 大图预览的三态。
///
/// 必须是显式枚举而不是 `Option`：调用方要区分「还在解」和「解不出来」，
/// 前者该画占位、后者该给一句话说明，混成一个 `None` 就只能让用户干等。
pub enum PreviewState {
    Loading,
    Ready(Thumb),
    Failed,
}

/// 缓存键。`side` 是必须的一维：同一张图在列表里要 128px、在预览里要
/// 1024px，只按 (id, len) 认会造成「预览拿到的其实是缩略图纹理，放大后一片糊」。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    id: i64,
    len: i64,
    side: u32,
}

struct Slot {
    /// `None` 表示解码失败。失败也记一条，否则每帧都会重试同一个坏数据。
    texture: Option<egui::TextureHandle>,
    source: egui::Vec2,
    used: u64,
}

struct Job {
    key: Key,
    png: Vec<u8>,
}

struct Done {
    key: Key,
    /// 原图尺寸
    source: [usize; 2],
    /// 缩略图尺寸 + RGBA 像素；解码失败时为 None
    bitmap: Option<([usize; 2], Vec<u8>)>,
}

pub struct Thumbs {
    cache: HashMap<Key, Slot>,
    pending: HashSet<Key>,
    /// 大图预览的纹理。只留一张——用户不会同时看两张图，
    /// 而 1024px RGBA 一张就是 4 MB，多留几张纯属浪费。
    preview: PreviewSlot,
    tx: Sender<Job>,
    rx: Receiver<Done>,
    /// 单增的「使用序号」，用来做最近最少使用淘汰。
    tick: u64,
}

/// 预览槽的状态。用它自己记排队情况，而不是共用的 `pending` 集合：
/// 预览只发一次请求，必须能区分「已经排上了」和「还没排」。
enum PreviewSlot {
    Empty,
    Loading(Key),
    Ready(Key, egui::TextureHandle, egui::Vec2),
    /// 解码失败（损坏的 PNG / 超像素上限 / 读不出 payload）。记下来别重试。
    Failed(Key),
}

impl Thumbs {
    pub fn new(ctx: &egui::Context) -> Self {
        // 有界队列：UI 侧本来就限制了在途数量，这里再兜一层，防止极端情况下堆积。
        let (tx, job_rx) = crossbeam_channel::bounded::<Job>(8);
        let (done_tx, rx) = crossbeam_channel::unbounded();

        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("rpaste-thumbs".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let (source, bitmap) = match decode(&job.png, job.key.side) {
                        Some((w, h, tex_w, tex_h, rgba)) => ([w, h], Some(([tex_w, tex_h], rgba))),
                        None => ([0, 0], None),
                    };
                    if done_tx
                        .send(Done {
                            key: job.key,
                            source,
                            bitmap,
                        })
                        .is_err()
                    {
                        break;
                    }
                    // 必须叫醒 UI：窗口空闲时根本不重绘，不主动请求的话
                    // 缩略图要等到用户下一次操作才会出现。
                    ctx.request_repaint();
                }
            });
        if spawned.is_err() {
            crate::log_error!("无法启动缩略图解码线程");
        }

        Self {
            cache: HashMap::new(),
            pending: HashSet::new(),
            preview: PreviewSlot::Empty,
            tx,
            rx,
            tick: 0,
        }
    }

    /// 每帧调用一次，把后台解好的位图上传成纹理。
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.rx.try_recv() {
            self.pending.remove(&done.key);
            self.tick += 1;
            let used = self.tick;

            let (texture, source) = match done.bitmap {
                Some(([w, h], rgba)) => {
                    let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                    let name = format!("thumb/{}/{}/{}", done.key.side, done.key.id, used);
                    let texture = ctx.load_texture(name, image, egui::TextureOptions::LINEAR);
                    (
                        Some(texture),
                        egui::vec2(done.source[0] as f32, done.source[1] as f32),
                    )
                }
                None => (None, egui::Vec2::ZERO),
            };

            // 预览请求走单独一条路：只有「当前正等着这张」才收结果。
            // 用户按了 Space 又换图时，旧结果到达时槽里已经不是它了，
            // 直接丢弃，绝不能把新图顶掉。
            if done.key.side > THUMB_SIDE {
                if matches!(self.preview, PreviewSlot::Loading(k) if k == done.key) {
                    self.preview = match texture {
                        Some(t) => PreviewSlot::Ready(done.key, t, source),
                        None => PreviewSlot::Failed(done.key),
                    };
                }
                continue;
            }

            self.cache.insert(
                done.key,
                Slot {
                    texture,
                    source,
                    used,
                },
            );
            self.evict();
        }
    }

    /// 取缩略图。没解好就排进队列并返回 `None`——调用方先画占位块。
    pub fn get(
        &mut self,
        ctx: &egui::Context,
        read: &ReadStore,
        id: i64,
        byte_len: i64,
    ) -> Option<Thumb> {
        self.poll(ctx);
        self.tick += 1;
        let key = Key {
            id,
            len: byte_len,
            side: THUMB_SIDE,
        };

        if let Some(slot) = self.cache.get_mut(&key) {
            slot.used = self.tick;
            return slot.texture.clone().map(|texture| Thumb {
                texture,
                source: slot.source,
            });
        }

        if self.pending.len() < MAX_INFLIGHT && !self.pending.contains(&key) {
            self.pending.insert(key);
            let queued = match read.load_payload(id) {
                Ok(Some(png)) if !png.is_empty() => self.tx.try_send(Job { key, png }).is_ok(),
                _ => false,
            };
            if !queued {
                // 读不出来或者队列满了：记成失败，别每帧重试。
                self.pending.remove(&key);
                let used = self.tick;
                self.cache.insert(
                    key,
                    Slot {
                        texture: None,
                        source: egui::Vec2::ZERO,
                        used,
                    },
                );
                self.evict();
            }
        }

        None
    }

    /// 取大图预览纹理。
    ///
    /// 和 `get` 分开而不是加个参数：预览有独立的一格缓存和独立的三态，
    /// 混进列表缓存会把 4 MB 的纹理挤进 48 张 64 KB 的池子里，淘汰逻辑立刻失真。
    pub fn preview(
        &mut self,
        ctx: &egui::Context,
        read: &ReadStore,
        id: i64,
        byte_len: i64,
    ) -> PreviewState {
        self.poll(ctx);
        let key = Key {
            id,
            len: byte_len,
            side: PREVIEW_SIDE,
        };

        match &self.preview {
            PreviewSlot::Ready(k, texture, source) if *k == key => {
                return PreviewState::Ready(Thumb {
                    texture: texture.clone(),
                    source: *source,
                });
            }
            PreviewSlot::Loading(k) if *k == key => return PreviewState::Loading,
            PreviewSlot::Failed(k) if *k == key => return PreviewState::Failed,
            _ => {}
        }

        // 换了一张图：旧纹理直接丢掉，把新的排上。
        self.preview = PreviewSlot::Loading(key);
        let queued = match read.load_payload(id) {
            Ok(Some(png)) if !png.is_empty() => self.tx.try_send(Job { key, png }).is_ok(),
            _ => false,
        };
        if !queued {
            self.preview = PreviewSlot::Failed(key);
            return PreviewState::Failed;
        }
        PreviewState::Loading
    }

    /// 超出上限时丢掉最久没被用到的那些。
    fn evict(&mut self) {
        while self.cache.len() > MAX_CACHE {
            let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(_, slot)| slot.used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.cache.remove(&oldest);
        }
    }
}

/// PNG → 缩略图 RGBA，最长边缩到 `side` 以内。
///
/// 先只读文件头拿尺寸，超大图直接放弃：`image` 的解码器会先把整张位图
/// 铺到内存里，8K 图光这一步就是 130 MB，`panic = "abort"` 下分配失败
/// 会直接把进程干掉。
fn decode(png: &[u8], side: u32) -> Option<(usize, usize, usize, usize, Vec<u8>)> {
    let reader = image::ImageReader::new(Cursor::new(png))
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS {
        return None;
    }

    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;

    let scale = (side as f32 / w.max(h) as f32).min(1.0);
    let tw = ((w as f32 * scale).round() as u32).max(1);
    let th = ((h as f32 * scale).round() as u32).max(1);
    let small = if tw < w || th < h {
        // CatmullRom 比默认的 Triangle 锐一些，截图里的文字缩下去更清楚。
        img.resize_exact(tw, th, image::imageops::FilterType::CatmullRom)
    } else {
        img
    };

    let rgba = small.to_rgba8();
    let (tw, th) = (rgba.width() as usize, rgba.height() as usize);
    Some((w as usize, h as usize, tw, th, rgba.into_raw()))
}
