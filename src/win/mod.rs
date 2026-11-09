pub mod appinfo;
pub mod autostart;
pub mod clipboard;
/// Win+V 接管（低级键盘钩子）——暂时搁置，代码保留。见模块文档。
pub mod hook;
pub mod hotkey;
pub mod listener;
pub mod settings;
pub mod single;
pub mod tray;
pub mod window;

pub use window::{
    apply_backdrop, hide_main_window, is_main_window_visible, paste_to_previous, poke_repaint,
    show_at_cursor, show_main_window, toggle_main_window,
};
