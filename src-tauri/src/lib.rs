//! 文件职责：组装 CommandShelf 后端模块、Tauri 状态与前端可调用命令。
//! 主要内容：注册应用恢复、文档保存与 Git 同步接口，并启动单窗口桌面壳。
//! 重要约束：前端只能调用显式注册的窄接口，不能获得任意文件或 Shell 能力。

mod app_service;
mod backup_store;
mod command_store;
mod config_store;
mod error;
mod file_io;
mod git_repository;
mod model;

use app_service::AppService;
use config_store::default_config_directory;
use error::AppError;
use model::{AppSnapshot, CommandDocument};
use std::sync::{Mutex, MutexGuard, TryLockError};
use tauri::State;

/// Tauri 管理的运行期依赖；所有仓库写入和同步共享同一互斥门。
struct RuntimeState {
    /// 负责机器配置、仓库和文档编排的用例服务。
    app_service: AppService,
    /// 串行化仓库选择和文档写入，后端不能只依赖前端按钮禁用。
    operation_lock: Mutex<()>,
}

/// 阻塞获取启动恢复锁；若此前线程异常退出，继续运行而不是让应用永久不可用。
fn lock_operations(operation_lock: &Mutex<()>) -> MutexGuard<'_, ()> {
    operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 非阻塞获取用户操作锁，拒绝排队的重复保存或同步，避免前一次结束后意外再执行一次。
fn try_lock_operations(operation_lock: &Mutex<()>) -> Result<MutexGuard<'_, ()>, AppError> {
    match operation_lock.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => Err(AppError::new(
            "OPERATION_IN_PROGRESS",
            "另一项保存或同步操作仍在进行，本次请求未排队。",
            "等待当前操作结束后再重试；浏览与复制仍可继续使用。",
            true,
        )),
    }
}

/// 恢复上次有效仓库；首次运行返回未配置快照而不是错误。
#[tauri::command]
fn load_app(state: State<'_, RuntimeState>) -> AppSnapshot {
    let _guard = lock_operations(&state.operation_lock);
    state.app_service.load_app()
}

/// 连接用户输入的本地 Git 仓库，并在需要时初始化空数据文件。
///
/// 参数：`repository_path` 必须是已有 origin 和上游的 Git 根目录。
/// 副作用：成功后写入当前电脑配置；仓库缺少数据文件时创建空 `commands.json`。
#[tauri::command]
fn choose_repository(
    repository_path: String,
    state: State<'_, RuntimeState>,
) -> Result<AppSnapshot, AppError> {
    let _guard = try_lock_operations(&state.operation_lock)?;
    state.app_service.choose_repository(&repository_path)
}

/// 校验前端完整文档和磁盘基线，成功后备份并原子保存。
///
/// 参数：`expected_hash` 来自最近成功快照，用于检测应用外修改。
/// 副作用：替换仓库中的 `commands.json`，但不创建 Git 提交或访问网络。
#[tauri::command]
fn save_document(
    document: CommandDocument,
    expected_hash: String,
    state: State<'_, RuntimeState>,
) -> Result<AppSnapshot, AppError> {
    let _guard = try_lock_operations(&state.operation_lock)?;
    state.app_service.save_document(document, &expected_hash)
}

/// 显式执行安全拉取，并在成功后返回重新校验的完整文档快照。
///
/// 副作用：会访问当前仓库的 `origin`；本地有修改、候选无效或分叉时不改变工作区。
#[tauri::command]
fn pull_repository(state: State<'_, RuntimeState>) -> Result<AppSnapshot, AppError> {
    let _guard = try_lock_operations(&state.operation_lock)?;
    state.app_service.pull_repository()
}

/// 显式提交并普通推送当前命令数据，成功后返回重新计算的同步状态。
///
/// 副作用：可能创建本地提交并访问 `origin`；不会暂存其他文件或使用强制推送。
#[tauri::command]
fn push_repository(state: State<'_, RuntimeState>) -> Result<AppSnapshot, AppError> {
    let _guard = try_lock_operations(&state.operation_lock)?;
    state.app_service.push_repository()
}

/// 启动 Tauri 桌面应用并注册经过显式授权的最小命令集合。
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let runtime_state = RuntimeState {
        app_service: AppService::new(default_config_directory()),
        operation_lock: Mutex::new(()),
    };
    tauri::Builder::default()
        .manage(runtime_state)
        .invoke_handler(tauri::generate_handler![
            load_app,
            choose_repository,
            save_document,
            pull_repository,
            push_repository
        ])
        .run(tauri::generate_context!())
        .expect("CommandShelf 桌面应用启动失败");
}

#[cfg(test)]
mod tests {
    //! 测试职责：验证后端互斥门不会把用户重复操作排队到当前同步之后执行。

    use super::try_lock_operations;
    use std::sync::Mutex;

    /// 验证已有操作持锁时返回稳定错误，释放后立即恢复可用。
    #[test]
    fn rejects_overlapping_operations_without_queueing() {
        let operation_lock = Mutex::new(());
        let active_guard = operation_lock.lock().expect("首次操作应能获取互斥锁");

        let error = try_lock_operations(&operation_lock).expect_err("并发操作应被立即拒绝");
        assert_eq!(error.code, "OPERATION_IN_PROGRESS");
        assert!(error.retryable);

        drop(active_guard);
        assert!(try_lock_operations(&operation_lock).is_ok());
    }
}
