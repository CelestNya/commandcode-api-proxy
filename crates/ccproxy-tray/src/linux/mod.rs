//! Linux 平台实现：托盘（ksni / zbus StatusNotifierItem）、配置、日志、
//! 平台动作（xdg-open / XDG autostart / 代理启停）。
//!
//! 与 Windows 侧（`win.rs`/`process.rs`/`settings.rs` 等）平行，二者共享
//! `tray_api` 契约。本模块只在 `cfg(unix)` 下编译。

pub mod actions;
pub mod log;
pub mod settings;
pub mod tray;
