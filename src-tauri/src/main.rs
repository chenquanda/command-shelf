#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! 文件职责：提供 Windows 桌面可执行程序的最小入口。
//! 主要内容：把生命周期与业务初始化委托给库入口，避免入口文件承载业务逻辑。
//! 重要约束：Release 构建隐藏控制台窗口；所有可观察错误由应用层转换后展示。

/// 启动 CommandShelf 桌面应用。
fn main() {
    command_shelf_lib::run();
}
