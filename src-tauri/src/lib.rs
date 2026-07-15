//! 文件职责：组装 CommandShelf 后端模块、Tauri 状态与前端可调用命令。
//! 主要内容：注册应用恢复、命令与临时收集文档读取、保存及 Git 同步接口。
//! 重要约束：前端只能调用显式注册的窄接口，不能获得任意文件或 Shell 能力。

mod app_service;
mod backup_store;
mod codex_cli;
mod command_store;
mod config_store;
mod error;
mod file_io;
mod git_repository;
mod inbox_store;
/// 结构化同步冲突的纯内存三方合并接口；由 Git 编排层和对应测试共同复用。
pub mod merge_engine;
mod model;
mod process_runner;

use app_service::AppService;
use codex_cli::{detect_codex_cli, generate_command_draft_with_retry, CodexCliStatus};
use config_store::default_config_directory;
use error::AppError;
use model::{
    AppSnapshot, CommandDocument, CommandDraftGenerationResult, InboxDocument, InboxSnapshot,
};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use tauri::State;

/// Tauri 管理的运行期依赖；所有仓库写入和同步共享同一互斥门。
struct RuntimeState {
    /// 负责机器配置、仓库和文档编排的用例服务。
    app_service: Arc<AppService>,
    /// 串行化仓库选择和文档写入，后端不能只依赖前端按钮禁用。
    operation_lock: Arc<Mutex<()>>,
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
            "等待当前操作结束后再重试；浏览和复制参考输出仍可继续使用。",
            true,
        )),
    }
}

/// 在 Tauri 阻塞线程池中执行一次应用 I/O 操作，避免文件或 Git 等待占用桌面事件线程。
///
/// 参数：`operation` 只能调用已经受控的应用服务方法，不得绕过数据仓库边界启动任意进程。
/// 返回值：保留原业务结果；后台任务异常退出时转换为稳定的结构化错误。
/// 副作用：闭包持有互斥门直到保存、拉取或推送完成，重叠请求仍会被立即拒绝而不会排队。
async fn run_blocking_app_operation<T, F>(
    app_service: Arc<AppService>,
    operation_lock: Arc<Mutex<()>>,
    operation: F,
) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(&AppService) -> Result<T, AppError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = try_lock_operations(&operation_lock)?;
        operation(&app_service)
    })
    .await
    .map_err(|error| {
        AppError::new(
            "BACKGROUND_OPERATION_FAILED",
            format!("保存或同步后台任务异常结束：{error}"),
            "确认应用仍在运行后重试；本地命令数据不会因此被删除。",
            true,
        )
    })?
}

/// 在阻塞线程池中读取或首次初始化当前仓库的临时收集文档。
///
/// 返回值：独立的临时收集快照，不改变现有应用启动快照和分类页面状态。
/// 副作用：仓库缺少 `inbox.json` 时创建空文档；不保存已有记录，也不访问网络。
#[tauri::command]
async fn load_inbox_document(state: State<'_, RuntimeState>) -> Result<InboxSnapshot, AppError> {
    run_blocking_app_operation(
        Arc::clone(&state.app_service),
        Arc::clone(&state.operation_lock),
        |app_service| app_service.load_inbox_document(),
    )
    .await
}

/// 校验临时收集文档和磁盘基线，成功后在阻塞线程池中备份并原子保存。
///
/// 参数：`expected_hash` 来自最近一次成功读取或保存，用于检测应用外修改。
/// 副作用：替换仓库中的 `inbox.json`，但不提交 Git、不访问网络。
#[tauri::command]
async fn save_inbox_document(
    document: InboxDocument,
    expected_hash: String,
    state: State<'_, RuntimeState>,
) -> Result<InboxSnapshot, AppError> {
    run_blocking_app_operation(
        Arc::clone(&state.app_service),
        Arc::clone(&state.operation_lock),
        move |app_service| app_service.save_inbox_document(document, &expected_hash),
    )
    .await
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
async fn save_document(
    document: CommandDocument,
    expected_hash: String,
    state: State<'_, RuntimeState>,
) -> Result<AppSnapshot, AppError> {
    run_blocking_app_operation(
        Arc::clone(&state.app_service),
        Arc::clone(&state.operation_lock),
        move |app_service| app_service.save_document(document, &expected_hash),
    )
    .await
}

/// 显式执行安全拉取，并在成功后返回重新校验的完整文档快照。
///
/// 副作用：可能提交受管本地修改并访问 `origin`；候选无效时不接入远端，冲突时自动中止 rebase。
#[tauri::command]
async fn pull_repository(state: State<'_, RuntimeState>) -> Result<AppSnapshot, AppError> {
    run_blocking_app_operation(
        Arc::clone(&state.app_service),
        Arc::clone(&state.operation_lock),
        |app_service| app_service.pull_repository(),
    )
    .await
}

/// 显式提交、接入远端更新并普通推送当前命令数据，成功后返回重新计算的同步状态。
///
/// 副作用：可能创建或重放本地提交并访问 `origin`；不会暂存其他文件、解决冲突或使用强制推送。
#[tauri::command]
async fn push_repository(state: State<'_, RuntimeState>) -> Result<AppSnapshot, AppError> {
    run_blocking_app_operation(
        Arc::clone(&state.app_service),
        Arc::clone(&state.operation_lock),
        |app_service| app_service.push_repository(),
    )
    .await
}

/// 查询当前电脑上的 Codex CLI 是否可用，并返回版本或可执行的安装检查提示。
///
/// 副作用：仅受控执行固定的 `codex --version`，不调用模型、不访问网络或命令数据。
#[tauri::command]
fn get_codex_cli_status() -> CodexCliStatus {
    detect_codex_cli()
}

/// 使用本机 Codex CLI 生成临时命令草稿，不保存到当前分类或数据文件。
///
/// 参数：`question` 只作为标准输入传给固定的只读、临时 Codex 会话。
/// 返回值：包含合法草稿，或连续两次无效后供前端人工填写的第二次原文。
/// 副作用：响应无效时会新建一次会话重试；绝不执行生成命令或修改 `commands.json`。
#[tauri::command]
fn generate_command_draft(question: String) -> Result<CommandDraftGenerationResult, AppError> {
    generate_command_draft_with_retry(&question)
}

/// 启动 Tauri 桌面应用并注册经过显式授权的最小命令集合。
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let runtime_state = RuntimeState {
        app_service: Arc::new(AppService::new(default_config_directory())),
        operation_lock: Arc::new(Mutex::new(())),
    };
    tauri::Builder::default()
        .manage(runtime_state)
        .invoke_handler(tauri::generate_handler![
            load_app,
            load_inbox_document,
            save_inbox_document,
            choose_repository,
            save_document,
            pull_repository,
            push_repository,
            get_codex_cli_status,
            generate_command_draft
        ])
        .run(tauri::generate_context!())
        .expect("CommandShelf 桌面应用启动失败");
}

#[cfg(test)]
mod tests {
    //! 测试职责：验证后端互斥门不会排队重复操作，本地保存与仓库同步也不会占用调用线程。

    use super::{run_blocking_app_operation, try_lock_operations};
    use crate::app_service::AppService;
    use crate::model::AppSnapshot;
    use std::sync::{Arc, Mutex};
    use std::thread;

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

    /// 验证应用 I/O 闭包在阻塞线程池执行，供本地保存和仓库同步共同复用。
    #[test]
    fn runs_blocking_app_operation_off_the_calling_thread() {
        let directory = tempfile::tempdir().expect("应能创建后台操作测试目录");
        let app_service = Arc::new(AppService::new(directory.path().join("config")));
        let operation_lock = Arc::new(Mutex::new(()));
        let calling_thread = thread::current().id();
        let worker_thread = Arc::new(Mutex::new(None));
        let captured_worker_thread = Arc::clone(&worker_thread);

        let snapshot = tauri::async_runtime::block_on(run_blocking_app_operation(
            app_service,
            operation_lock,
            move |_| {
                *captured_worker_thread.lock().expect("应能记录后台线程") =
                    Some(thread::current().id());
                Ok(AppSnapshot::unconfigured())
            },
        ))
        .expect("后台仓库操作应成功返回");

        assert!(snapshot.repository_path.is_none());
        assert_ne!(
            worker_thread
                .lock()
                .expect("应能读取后台线程")
                .expect("后台闭包必须记录线程"),
            calling_thread,
            "仓库操作不得在调用线程内同步执行"
        );
    }
}
