//! 文件职责：为 CommandShelf 生成 Tauri 所需的 Windows 资源与构建元数据。
//! 主要内容：把静态配置交给 Tauri 官方构建器处理。
//! 重要约束：构建脚本不得读取用户数据仓库，也不得产生运行期配置。

/// 执行 Tauri 官方构建流程；失败由 Cargo 直接终止构建并返回原始诊断。
fn main() {
    tauri_build::build();
}
