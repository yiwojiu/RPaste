//! 视觉 token：圆角、间距、字号、语义色、动画时长。
//!
//! 这些数字原先散落在 `ui.rs` 各处：圆角出现过 3 / 4 / 5 / 8 / 9 五种，
//! 颜色十几处硬编码 RGB。后果不是「不好看」，而是**同一屏里风格不一致**——
//! 搜索框圆一点、卡片方一点、浮层又圆一点，眼睛说不出哪不对但就是觉得糙。
//!
//! 收到这里之后，`ui.rs` 只表达意图（"这是一个卡片"），数值由这里定义。
//! 改整体风格只需要动这一个文件。
//!
//! 为什么不用 `egui::Style` 里现成的旋钮：那套是按「控件类型」组织的
//! （widget/interact/window/noninteractive），而这个应用的视觉单元和控件类型
//! 不是一一对应的——列表卡片是 `Frame`、chip 是 `Frame` + `interact`、
//! 浮层也是 `Frame`，但它们该用三种不同的圆角。按语义命名比按控件命名好用。

use eframe::egui::{self, Color32};

// ---------------------------------------------------------------- 圆角

/// 缩略图占位、内嵌小方块。
pub const RADIUS_XS: f32 = 3.0;
/// 图标按钮的 hover 底色（搜索框里的 ✕）。
pub const RADIUS_SM: f32 = 4.0;
/// 列表卡片。
pub const RADIUS_MD: f32 = 6.0;
/// 浮层卡片、预览窗。
pub const RADIUS_LG: f32 = 10.0;
/// chip 与搜索框：胶囊形。给一个大于高度一半的值，`CornerRadius` 内部会夹到
/// 高度的一半，写 999 比自己算 `height/2` 稳——高度会随字号变。
pub const RADIUS_PILL: f32 = 999.0;
// ---------------------------------------------------------------- 间距

pub const GAP_XS: f32 = 3.0;
pub const GAP_SM: f32 = 5.0;
pub const GAP_MD: f32 = 8.0;
pub const GAP_LG: f32 = 12.0;

/// 卡片内边距（左、右、上、下）。
pub const CARD_PAD_X: i8 = 8;
pub const CARD_PAD_Y: i8 = 5;
/// chip 内边距。
pub const CHIP_PAD_X: i8 = 8;
pub const CHIP_PAD_Y: i8 = 3;

/// 底部状态栏总高。
///
/// 原来是「一行 11px 灰字 + 默认面板边距」约 22px，贴着窗口下沿，看起来像
/// 被切掉了一半。32px 里放 20px 控件 + 上下各 6px 呼吸，代价不到一行列表
/// （一行约 50px），换来的是这一栏终于"站得住"。
pub const FOOTER_H: f32 = 32.0;
/// 状态栏左右内边距。
pub const FOOTER_PAD_X: i8 = 12;

/// 状态栏里所有控件的统一高度：键帽和右侧按钮必须一样高。
///
/// 曾经键帽 18、按钮 23，而这一行是顶对齐的（不是居中），结果按钮里的字
/// 比键帽里的字低 3px —— 12px 字号下 3px 的错位一眼就能看出来。高度一致
/// 之后，无论这行按顶对齐还是居中对齐，两者都不会再错开。
pub const KEY_H: f32 = 20.0;
/// 键帽左右内边距。
pub const KEY_PAD_X: f32 = 5.0;

// ---------------------------------------------------------------- 字号

/// 次要信息：时间、字节数、命中次数。
pub const FONT_SM: f32 = 11.0;
/// 正文。
pub const FONT_BODY: f32 = 13.0;
/// 底部状态栏：键帽用 `FONT_SM`，说明文字用这一档。
/// 比 Small 大一档——它在窗口最下沿，离眼睛最远，11px 撑不住。
pub const FONT_FOOT: f32 = 12.0;
/// 预览区（等宽）。
pub const FONT_MONO: f32 = 13.0;

// ---------------------------------------------------------------- 语义色

/// 主强调色：选中态、焦点环、主按钮。
pub const ACCENT: Color32 = Color32::from_rgb(0x2C, 0x5E, 0x8A);
/// 收藏星标。
pub const PIN: Color32 = Color32::from_rgb(228, 172, 45);
/// 错误、源文件失效。
pub const DANGER: Color32 = Color32::from_rgb(0xE2, 0x4B, 0x4A);

/// 四个筛选档位的色相。选中时统一填 `ACCENT`，未选中时用各自的色相做淡底，
/// 这样「切到哪一档」和「这一档是什么」是两套视觉信号，不互相干扰。
pub const KIND_ALL: Color32 = Color32::from_rgb(0x5B, 0x6B, 0x7A);
pub const KIND_TEXT: Color32 = Color32::from_rgb(0x18, 0x5F, 0xA5);
pub const KIND_RICH: Color32 = Color32::from_rgb(0x53, 0x4A, 0xB7);
pub const KIND_IMAGE: Color32 = Color32::from_rgb(0x0F, 0x6E, 0x56);
pub const KIND_FILES: Color32 = Color32::from_rgb(0x99, 0x3C, 0x1D);

/// 淡淡一层：「未选中的 chip 底色」「行 hover 底色」都靠它。
/// 返回 alpha 已经压到很低，直接 `rect_filled` 也不会盖住文字。
pub fn wash(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

// ---------------------------------------------------------------- 动画

/// 瞬时反馈：hover 底色、清除键。
pub const ANIM_FAST: f32 = 0.10;
/// 状态切换：chip 选中、浮层淡入。
pub const ANIM_BASE: f32 = 0.16;

// ---------------------------------------------------------------- 全局样式

/// 在创建窗口前调一次，把上面这些值灌进 egui 的全局样式。
///
/// 只管「默认控件」——`TextEdit`、滚动条、面板边距这些没法逐处传参的东西。
/// 自己 `Frame` 画的卡片仍然直接读上面的常量，不经过这里。
pub fn install(ctx: &egui::Context) {
    use egui::{FontFamily, FontId, TextStyle};

    ctx.style_mut(|style| {
        // 默认间距偏大，列表一行能占掉 30px。收到 5，一屏多显示两行。
        style.spacing.item_spacing = egui::vec2(GAP_SM, GAP_SM);
        style.spacing.button_padding = egui::vec2(GAP_MD, GAP_XS);
        style.spacing.menu_margin = egui::Margin::same(GAP_XS as i8);
        // 滚动条窄一点，不抢视线。
        style.spacing.scroll.bar_width = 6.0;
        style.spacing.scroll.bar_inner_margin = 2.0;

        style.visuals.selection.stroke.width = 1.0;

        // 面板底色半透明：底下是 DWM 合成的亚克力，画成全不透明就把材质盖死了。
        //
        // 这个 alpha 是量出来的，不是拍的：210 时实测「时间」「字节数」这些
        // 弱化文字在浅色亚克力上几乎糊成一片——系统亚克力本身已经叠了一层
        // 白色蒙版，再压一层半透明白就白了。238 保留隐约的通透感，同时把
        // 灰字对比度拉回到可读范围。
        style.visuals.panel_fill = if style.visuals.dark_mode {
            Color32::from_rgba_unmultiplied(26, 26, 30, 226)
        } else {
            Color32::from_rgba_unmultiplied(250, 250, 252, 238)
        };

        // 字号映射：`RichText::small()` 用的是 Small 这一档，
        // 默认 10 在中文下偏糊，提到 11。
        style.text_styles = [
            (
                TextStyle::Small,
                FontId::new(FONT_SM, FontFamily::Proportional),
            ),
            (
                TextStyle::Body,
                FontId::new(FONT_BODY, FontFamily::Proportional),
            ),
            (
                TextStyle::Button,
                FontId::new(FONT_BODY, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(FONT_MONO, FontFamily::Monospace),
            ),
        ]
        .into();

        // 控件自身的圆角也统一：输入框、按钮、滚动条滑块。
        let r = egui::CornerRadius::same(RADIUS_SM as u8);
        for w in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            w.corner_radius = r;
        }
    });
}
