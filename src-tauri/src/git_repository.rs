//! 文件职责：验证专用数据仓库、读取受管数据状态并执行安全拉取与普通推送。
//! 主要内容：固定调用受控 Git 子命令，提供非交互、无窗口、限时和有限输出的进程边界。
//! 重要约束：程序名与参数由后端决定；禁止 Shell 字符串、merge commit、破坏性 reset 和 force；
//! 仅允许把本地数据提交接到已校验远端提交之后的受控 rebase，失败时必须自动中止并保留本地提交。

use crate::command_store::{parse_document_bytes, serialize_document, validate_document};
use crate::error::AppError;
use crate::file_io::atomic_write;
use crate::inbox_store::{
    parse_inbox_document_bytes, serialize_inbox_document, validate_inbox_document,
};
use crate::model::{CommandDocument, InboxDocument};
use crate::process_runner::{run_process, ProcessFailure, ProcessOutput};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 单个 Git 子进程允许运行的最长时间。
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// 标准输出最多保留 12 MB，足以容纳第一版 10 MB 的候选数据及少量余量。
const STDOUT_CAPTURE_LIMIT: usize = 12 * 1024 * 1024;
/// 标准错误最多保留 128 KB，避免异常 Git 进程无限占用内存。
const STDERR_CAPTURE_LIMIT: usize = 128 * 1024;

/// 已通过根目录、origin 与上游检查的数据仓库信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInfo {
    /// 经过文件系统规范化的仓库根路径。
    pub root: PathBuf,
    /// 供侧栏展示的仓库目录名称。
    pub name: String,
}

/// 一次拉取产生的远端接入与本地待推送状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullOutcome {
    /// `true` 表示本地 `HEAD` 已快进或重放到新的上游提交。
    pub updated: bool,
    /// `true` 表示拉取结束后仍有本地提交等待用户主动推送。
    pub has_local_changes: bool,
}

/// 一次推送是否创建了提交并实际访问远端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushOutcome {
    /// 本次操作是否为未提交的受管数据文件创建了新提交。
    pub committed: bool,
    /// 本次操作是否执行并成功完成了普通 `git push`。
    pub pushed: bool,
}

/// 固定在一次 Git 冲突时刻的三份受管数据快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryConflictSnapshot {
    /// 双方分叉前的共同祖先提交 OID。
    pub base_oid: String,
    /// 自动提交本机受管数据后的本地提交 OID。
    pub local_oid: String,
    /// 本次 fetch 后固定并校验过的远端提交 OID。
    pub upstream_oid: String,
    /// 共同祖先中的命令文档。
    pub base_commands: CommandDocument,
    /// 本机提交中的命令文档。
    pub local_commands: CommandDocument,
    /// 固定远端提交中的命令文档。
    pub remote_commands: CommandDocument,
    /// 共同祖先中的临时收集文档；旧提交缺失时视为空文档。
    pub base_inbox: InboxDocument,
    /// 本机提交中的临时收集文档；旧提交缺失时视为空文档。
    pub local_inbox: InboxDocument,
    /// 固定远端提交中的临时收集文档；旧提交缺失时视为空文档。
    pub remote_inbox: InboxDocument,
}

/// 受控 rebase 的两种健康结果：已经完成，或已退出并返回可供界面处理的冲突快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebasePreparationOutcome {
    /// Git 已经无冲突完成 rebase。
    Rebased,
    /// Git 确认受管文件存在冲突，随后成功 `rebase --abort` 并恢复原本机提交。
    Conflict(Box<RepositoryConflictSnapshot>),
}

/// 拉取准备阶段的结果；冲突分支携带语义合并所需快照而不把它压缩成错误文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullPreparationOutcome {
    /// 远端没有需要处理的真实冲突，拉取已按原协议完成。
    Completed(PullOutcome),
    /// 受管文件存在冲突，仓库已恢复干净，本地提交继续等待用户处理。
    Conflict {
        /// 固定共同基线、本机提交和远端提交中的两份文档。
        snapshot: Box<RepositoryConflictSnapshot>,
        /// 拉取开始前或自动提交后存在本地待推送提交。
        has_local_changes: bool,
    },
}

/// Git 业务层沿用受控进程的有限捕获结果。
type GitOutput = ProcessOutput;

/// 把进程启动失败转换为稳定 Git 错误；`NotFound` 必须与一般系统错误区分。
fn spawn_failure(error: std::io::Error) -> AppError {
    let (code, message, action) = if error.kind() == std::io::ErrorKind::NotFound {
        (
            "GIT_NOT_FOUND",
            "系统中没有找到 git.exe。".to_string(),
            "安装 Git for Windows，或确认 git.exe 已加入 PATH 后重试。",
        )
    } else {
        (
            "GIT_FAILED",
            format!("无法启动系统 Git：{error}"),
            "检查 Git 安装、PATH 和当前用户的程序执行权限后重试。",
        )
    };
    AppError::new(code, message, action, true)
}

/// 构造 Git 超时错误；测试可使用更短时限验证真实终止分支。
fn timeout_failure(timeout: Duration) -> AppError {
    let seconds = timeout.as_secs().max(1);
    AppError::new(
        "GIT_TIMEOUT",
        format!("系统 Git 运行超过 {seconds} 秒，操作已终止。"),
        "检查网络和凭据后重试；本地命令数据未被覆盖。",
        true,
    )
}

/// 把通用进程生命周期错误映射为稳定的 Git 业务错误。
fn process_failure(error: ProcessFailure) -> AppError {
    match error {
        ProcessFailure::Spawn(error) => spawn_failure(error),
        ProcessFailure::Timeout(timeout) => timeout_failure(timeout),
        ProcessFailure::Boundary { operation, source } => AppError::new(
            "GIT_FAILED",
            format!("{operation}：{source}"),
            "重新启动应用后重试；若持续失败，请在系统终端执行 Git 操作。",
            true,
        ),
        ProcessFailure::Wait(error) => AppError::new(
            "GIT_FAILED",
            format!("无法等待系统 Git：{error}"),
            "重新启动应用后重试。",
            true,
        ),
        ProcessFailure::ReaderPanicked { stream } => AppError::new(
            "GIT_FAILED",
            format!("读取 Git {stream}的线程异常退出。"),
            "重新启动应用后重试。",
            true,
        ),
        ProcessFailure::Read { stream, source } => AppError::new(
            "GIT_FAILED",
            format!("无法读取 Git {stream}：{source}"),
            "检查系统 Git 后重试。",
            true,
        ),
        ProcessFailure::Write { stream, source } => AppError::new(
            "GIT_FAILED",
            format!("无法写入 Git {stream}：{source}"),
            "重新启动应用后重试。",
            true,
        ),
    }
}

/// 以可注入程序和时限执行 Git 进程；生产路径固定为系统 Git，测试路径用于验证生命周期错误。
fn run_git_process(
    repository: &Path,
    executable: &str,
    arguments: &[&str],
    timeout: Duration,
) -> Result<GitOutput, AppError> {
    const GIT_ENVIRONMENT: &[(&str, &str)] = &[
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GCM_INTERACTIVE", "Never"),
        // rebase 不得因为提交信息编辑器占住后台线程；应用只重放既有提交，不改写提交说明。
        ("GIT_EDITOR", "true"),
        ("GIT_SEQUENCE_EDITOR", "true"),
        // Git for Windows 尊重该 locale；固定英文诊断后结构化分类不依赖用户系统语言。
        ("LC_ALL", "C"),
        ("LANG", "C"),
    ];
    run_process(
        repository,
        executable,
        arguments,
        GIT_ENVIRONMENT,
        timeout,
        STDOUT_CAPTURE_LIMIT,
        STDERR_CAPTURE_LIMIT,
    )
    .map_err(process_failure)
}

/// 以固定系统 Git 和正式超时时限执行仓库命令。
fn run_git(repository: &Path, arguments: &[&str]) -> Result<GitOutput, AppError> {
    run_git_process(repository, "git", arguments, GIT_TIMEOUT)
}

/// 把 Git 标准输出转换为去除结尾换行的 UTF-8 文本。
fn stdout_text(output: &GitOutput) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// 把 Git 标准错误转换为适合诊断的有限文本，并标记是否曾被截断。
fn stderr_text(output: &GitOutput) -> String {
    let mut text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.stderr_truncated {
        text.push_str("…（输出已截断）");
    }
    text
}

/// 根据有限 Git 错误文本映射身份、认证、网络、拒绝和通用错误。
///
/// 参数：`detail` 必须已经过输出上限约束；`status` 仅用于没有诊断文本的兜底提示。
fn classify_git_failure(detail: &str, operation: &str, status: &str) -> AppError {
    let normalized = detail.to_lowercase();
    if normalized.contains("author identity unknown")
        || normalized.contains("unable to auto-detect email address")
        || normalized.contains("please tell me who you are")
        || normalized.contains("empty ident name")
        || normalized.contains("no email was given")
    {
        return AppError::new(
            "GIT_IDENTITY_REQUIRED",
            "系统 Git 尚未配置提交姓名或邮箱。",
            "先在系统终端配置 Git user.name 与 user.email，再重试推送。",
            true,
        );
    }
    if normalized.contains("non-fast-forward")
        || normalized.contains("[rejected]")
        || normalized.contains("remote rejected")
        || normalized.contains("pre-receive hook declined")
    {
        return AppError::new(
            "GIT_PUSH_REJECTED",
            "远端拒绝了普通推送，应用不会强制覆盖。",
            "先处理远端新增提交或分叉，再重试推送。",
            false,
        );
    }
    if [
        "authentication failed",
        "could not read username",
        "terminal prompts disabled",
        "credential",
        "access denied",
        "repository not found",
        "permission denied (publickey)",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return AppError::new(
            "GIT_AUTH_REQUIRED",
            format!("{operation}需要有效的系统 Git 凭据。"),
            "先在系统终端对该仓库执行一次 Git 登录或同步，再回到应用重试。",
            true,
        );
    }
    if [
        "could not resolve host",
        "failed to connect",
        "couldn't connect",
        "connection timed out",
        "operation timed out",
        "connection reset",
        "connection was reset",
        "connection refused",
        "network is unreachable",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return AppError::new(
            "GIT_NETWORK_FAILED",
            format!("{operation}时无法连接远端仓库。"),
            "恢复网络后重试；本地命令数据未被覆盖。",
            true,
        );
    }
    AppError::new(
        "GIT_FAILED",
        if detail.is_empty() {
            format!("{operation}失败，Git 退出状态为 {status}。")
        } else {
            format!("{operation}失败：{detail}")
        },
        "在系统终端检查仓库状态后重试。",
        true,
    )
}

/// 根据 Git 进程结果调用纯文本分类器，便于错误矩阵独立回归。
fn command_failure(output: &GitOutput, operation: &str) -> AppError {
    classify_git_failure(&stderr_text(output), operation, &output.status.to_string())
}

/// 要求 Git 命令成功，否则使用统一错误映射。
fn require_success(output: GitOutput, operation: &str) -> Result<GitOutput, AppError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failure(&output, operation))
    }
}

/// 验证用户选择的是 Git 根目录，并且已有 `origin` 与当前分支上游。
pub fn validate_repository(path: &Path) -> Result<RepositoryInfo, AppError> {
    if !path.exists() || !path.is_dir() {
        return Err(AppError::new(
            "PATH_NOT_FOUND",
            "选择的本地仓库目录不存在。",
            "确认路径后重新选择已经克隆的仓库。",
            true,
        ));
    }
    let selected_root = fs::canonicalize(path).map_err(|error| {
        AppError::new(
            "PATH_INVALID",
            format!("无法规范化仓库路径：{error}"),
            "确认目录权限后重新选择。",
            true,
        )
    })?;

    let root_output = run_git(&selected_root, &["rev-parse", "--show-toplevel"])?;
    if !root_output.status.success() {
        return Err(AppError::new(
            "NOT_GIT_REPOSITORY",
            "选择的目录不是有效 Git 仓库。",
            "先使用系统 Git 克隆个人数据仓库，再选择其根目录。",
            false,
        ));
    }
    let discovered_root = fs::canonicalize(stdout_text(&root_output)).map_err(|error| {
        AppError::new(
            "NOT_GIT_REPOSITORY",
            format!("无法解析 Git 仓库根目录：{error}"),
            "重新克隆仓库后再连接。",
            true,
        )
    })?;
    if discovered_root != selected_root {
        return Err(AppError::new(
            "REPOSITORY_ROOT_REQUIRED",
            "请选择 Git 仓库根目录，而不是其中的子目录。",
            "返回上一级直到仓库根目录后重试。",
            false,
        ));
    }

    let origin_output = run_git(&selected_root, &["remote", "get-url", "origin"])?;
    if !origin_output.status.success() || stdout_text(&origin_output).is_empty() {
        return Err(AppError::new(
            "ORIGIN_MISSING",
            "数据仓库没有可用的 origin 远端。",
            "在系统终端配置 origin 后重新连接。",
            false,
        ));
    }

    let upstream_output = run_git(
        &selected_root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )?;
    if !upstream_output.status.success() || stdout_text(&upstream_output).is_empty() {
        let detail = stderr_text(&upstream_output);
        return Err(AppError::new(
            "UPSTREAM_MISSING",
            if detail.is_empty() {
                "当前分支没有配置上游。".to_string()
            } else {
                format!("当前分支没有可用上游：{detail}")
            },
            "在系统终端为当前分支设置上游后重新连接。",
            false,
        ));
    }

    let name = selected_root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("command-data")
        .to_string();
    Ok(RepositoryInfo {
        root: selected_root,
        name,
    })
}

/// 读取本地分支相对上游领先的提交数。
fn local_ahead_count(repository: &Path) -> Result<u64, AppError> {
    let output = require_success(
        run_git(repository, &["rev-list", "--count", "@{u}..HEAD"])?,
        "检查本地提交",
    )?;
    stdout_text(&output).parse::<u64>().map_err(|_| {
        AppError::new(
            "GIT_FAILED",
            "系统 Git 返回了无法识别的本地提交数量。",
            "在系统终端运行 git status 检查仓库后重试。",
            true,
        )
    })
}

/// 判断任一受管数据文件未提交，或当前分支存在尚未推送的提交。
pub fn repository_has_local_changes(repository: &Path) -> Result<bool, AppError> {
    let status_output = require_success(
        run_git(
            repository,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--",
                "commands.json",
                "inbox.json",
            ],
        )?,
        "检查受管数据文件状态",
    )?;
    if !stdout_text(&status_output).is_empty() {
        return Ok(true);
    }
    Ok(local_ahead_count(repository)? > 0)
}

/// 判断第一个提交是否为第二个提交的祖先；退出码 1 表示关系不成立而不是执行失败。
fn is_ancestor(repository: &Path, ancestor: &str, descendant: &str) -> Result<bool, AppError> {
    let output = run_git(
        repository,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(command_failure(&output, "判断分支关系")),
    }
}

/// 在 fetch 后把当前上游解析为一次性的完整提交 OID，后续流程不得再次解引用可变名称。
fn resolve_upstream_commit_oid(repository: &Path) -> Result<String, AppError> {
    let output = require_success(
        run_git(repository, &["rev-parse", "--verify", "@{u}^{commit}"])?,
        "解析当前上游提交",
    )?;
    let oid = stdout_text(&output);
    let supported_length = oid.len() == 40 || oid.len() == 64;
    if !supported_length || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::new(
            "GIT_FAILED",
            "系统 Git 返回了无法识别的上游提交 OID。",
            "在系统终端检查当前分支上游后重试。",
            true,
        ));
    }
    Ok(oid)
}

/// 把任意受控提交表达式解析为完整 OID，并拒绝异常输出。
fn resolve_commit_oid(
    repository: &Path,
    revision: &str,
    operation: &str,
) -> Result<String, AppError> {
    let object = format!("{revision}^{{commit}}");
    let output = require_success(
        run_git(repository, &["rev-parse", "--verify", object.as_str()])?,
        operation,
    )?;
    let oid = stdout_text(&output);
    let supported_length = oid.len() == 40 || oid.len() == 64;
    if supported_length && oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(oid);
    }
    Err(AppError::new(
        "GIT_FAILED",
        format!("{operation}返回了无法识别的提交 OID。"),
        "在系统终端检查当前分支历史后重试。",
        true,
    ))
}

/// 解析本机提交与固定远端提交的共同祖先，作为三方语义合并的共同基线。
fn resolve_merge_base_oid(
    repository: &Path,
    local_oid: &str,
    upstream_oid: &str,
) -> Result<String, AppError> {
    let output = require_success(
        run_git(repository, &["merge-base", local_oid, upstream_oid])?,
        "解析同步共同基线",
    )?;
    let oid = stdout_text(&output);
    let supported_length = oid.len() == 40 || oid.len() == 64;
    if supported_length && oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(oid);
    }
    Err(AppError::new(
        "GIT_DIVERGED",
        "本机与远端没有可识别的共同同步基线。",
        "确认两台电脑使用同一个数据仓库和分支后重试。",
        false,
    ))
}

/// 从固定提交读取并解析命令文档；调用方可用来源名称生成明确诊断。
fn read_commit_command_document(
    repository: &Path,
    commit_oid: &str,
    source: &str,
) -> Result<CommandDocument, AppError> {
    let object_spec = format!("{commit_oid}:commands.json");
    let candidate = run_git(repository, &["show", object_spec.as_str()])?;
    if !candidate.status.success() || candidate.stdout_truncated {
        return Err(AppError::new(
            "MERGE_DATA_INVALID",
            format!("{source}中缺少可读取的 commands.json。"),
            "重新连接仓库并生成新的冲突预览。",
            false,
        ));
    }
    parse_document_bytes(&candidate.stdout)
        .map(|(document, _hash)| document)
        .map_err(|error| {
            AppError::new(
                "MERGE_DATA_INVALID",
                format!("{source}的 commands.json 无效：{}", error.message),
                "修复对应数据后重新同步。",
                false,
            )
        })
}

/// 从固定提交读取临时收集文档；兼容旧提交尚未创建 `inbox.json` 的情况。
fn read_commit_inbox_document(
    repository: &Path,
    commit_oid: &str,
    source: &str,
) -> Result<InboxDocument, AppError> {
    let listing = require_success(
        run_git(
            repository,
            &["ls-tree", "--name-only", commit_oid, "--", "inbox.json"],
        )?,
        "检查冲突快照中的临时收集文件",
    )?;
    if stdout_text(&listing).is_empty() {
        return Ok(InboxDocument::empty());
    }
    let object_spec = format!("{commit_oid}:inbox.json");
    let candidate = run_git(repository, &["show", object_spec.as_str()])?;
    if !candidate.status.success() || candidate.stdout_truncated {
        return Err(AppError::new(
            "MERGE_DATA_INVALID",
            format!("{source}中的 inbox.json 无法读取。"),
            "重新连接仓库并生成新的冲突预览。",
            false,
        ));
    }
    parse_inbox_document_bytes(&candidate.stdout)
        .map(|(document, _hash)| document)
        .map_err(|error| {
            AppError::new(
                "MERGE_DATA_INVALID",
                format!("{source}的 inbox.json 无效：{}", error.message),
                "修复对应数据后重新同步。",
                false,
            )
        })
}

/// 从共同基线、本机提交和固定远端提交读取完整受管数据，形成不可变冲突会话。
fn capture_repository_conflict(
    repository: &Path,
    base_oid: String,
    local_oid: String,
    upstream_oid: String,
) -> Result<RepositoryConflictSnapshot, AppError> {
    Ok(RepositoryConflictSnapshot {
        base_commands: read_commit_command_document(repository, &base_oid, "共同基线")?,
        local_commands: read_commit_command_document(repository, &local_oid, "本机提交")?,
        remote_commands: read_commit_command_document(repository, &upstream_oid, "远端提交")?,
        base_inbox: read_commit_inbox_document(repository, &base_oid, "共同基线")?,
        local_inbox: read_commit_inbox_document(repository, &local_oid, "本机提交")?,
        remote_inbox: read_commit_inbox_document(repository, &upstream_oid, "远端提交")?,
        base_oid,
        local_oid,
        upstream_oid,
    })
}

/// 从固定提交 OID 读取并校验必需的候选 `commands.json`，不改变当前工作区。
fn validate_commit_command_document(repository: &Path, commit_oid: &str) -> Result<(), AppError> {
    let object_spec = format!("{commit_oid}:commands.json");
    let candidate = run_git(repository, &["show", object_spec.as_str()])?;
    if !candidate.status.success() {
        return Err(AppError::new(
            "REMOTE_DATA_INVALID",
            "远端最新提交中缺少可读取的 commands.json。",
            "在另一台电脑修复并推送数据文件后重试；本地文件未改变。",
            false,
        ));
    }
    if candidate.stdout_truncated {
        return Err(AppError::new(
            "REMOTE_DATA_INVALID",
            "远端 commands.json 超过允许的读取上限。",
            "精简远端数据文件后重试；本地文件未改变。",
            false,
        ));
    }
    parse_document_bytes(&candidate.stdout)
        .map(|_| ())
        .map_err(|error| {
            AppError::new(
                "REMOTE_DATA_INVALID",
                format!("远端 commands.json 未通过校验：{}", error.message),
                "在另一台电脑修复并推送有效数据后重试；本地文件未改变。",
                false,
            )
        })
}

/// 从固定提交 OID 校验可选的 `inbox.json`；旧仓库缺失该文件时允许后续兼容初始化。
fn validate_commit_inbox_document(repository: &Path, commit_oid: &str) -> Result<(), AppError> {
    let listing = require_success(
        run_git(
            repository,
            &["ls-tree", "--name-only", commit_oid, "--", "inbox.json"],
        )?,
        "检查远端临时收集文件",
    )?;
    if stdout_text(&listing).is_empty() {
        return Ok(());
    }

    let object_spec = format!("{commit_oid}:inbox.json");
    let candidate = require_success(
        run_git(repository, &["show", object_spec.as_str()])?,
        "读取远端临时收集文件",
    )?;
    if candidate.stdout_truncated {
        return Err(AppError::new(
            "REMOTE_INBOX_INVALID",
            "远端 inbox.json 超过允许的读取上限。",
            "精简远端临时收集内容后重试；本地文件未改变。",
            false,
        ));
    }
    parse_inbox_document_bytes(&candidate.stdout)
        .map(|_| ())
        .map_err(|error| {
            AppError::new(
                "REMOTE_INBOX_INVALID",
                format!("远端 inbox.json 未通过校验：{}", error.message),
                "在另一台电脑修复并推送有效临时收集数据后重试；本地文件未改变。",
                false,
            )
        })
}

/// 校验固定远端提交中的全部受管数据；命令必需，临时收集文件兼容旧仓库缺失。
fn validate_commit_documents(repository: &Path, commit_oid: &str) -> Result<(), AppError> {
    validate_commit_command_document(repository, commit_oid)?;
    validate_commit_inbox_document(repository, commit_oid)
}

/// 执行显式安全拉取：先提交受管数据，再校验并接入固定远端提交，保留待推送的本地提交。
pub fn pull_repository(repository: &Path) -> Result<PullOutcome, AppError> {
    pull_repository_with_after_validation(repository, || Ok(()))
}

/// 执行拉取准备阶段；无冲突时直接完成，受管文件冲突时返回已安全退出的固定快照。
pub fn prepare_pull_repository(repository: &Path) -> Result<PullPreparationOutcome, AppError> {
    prepare_pull_repository_with_after_validation(repository, || Ok(()))
}

/// 执行拉取核心流程；校验后回调用于确定性测试可变远端跟踪引用竞态。
fn pull_repository_with_after_validation<F>(
    repository: &Path,
    after_validation: F,
) -> Result<PullOutcome, AppError>
where
    F: FnOnce() -> Result<(), AppError>,
{
    match prepare_pull_repository_with_after_validation(repository, after_validation)? {
        PullPreparationOutcome::Completed(outcome) => Ok(outcome),
        PullPreparationOutcome::Conflict { .. } => Err(AppError::new(
            "GIT_DIVERGED",
            "本地修改与远端更新存在冲突，自动同步已停止。",
            "本地提交和数据均已保留；请在应用内确认合并结果后继续。",
            false,
        )),
    }
}

/// 拉取核心流程；回调用于固定候选后推进远端引用的竞态回归。
fn prepare_pull_repository_with_after_validation<F>(
    repository: &Path,
    after_validation: F,
) -> Result<PullPreparationOutcome, AppError>
where
    F: FnOnce() -> Result<(), AppError>,
{
    ensure_empty_index(repository)?;
    ensure_no_unrelated_changes(repository)?;
    let ahead_before_commit = local_ahead_count(repository)?;
    let committed = commit_managed_documents(repository)?;
    let has_local_changes = committed || ahead_before_commit > 0;
    require_success(
        run_git(repository, &["fetch", "--prune", "origin"])?,
        "拉取远端引用",
    )?;

    let upstream_oid = resolve_upstream_commit_oid(repository)?;
    let local_is_ancestor = is_ancestor(repository, "HEAD", &upstream_oid)?;
    let upstream_is_ancestor = is_ancestor(repository, &upstream_oid, "HEAD")?;
    match (local_is_ancestor, upstream_is_ancestor) {
        (true, true) | (false, true) => Ok(PullPreparationOutcome::Completed(PullOutcome {
            updated: false,
            has_local_changes,
        })),
        (true, false) => {
            validate_commit_documents(repository, &upstream_oid)?;
            after_validation()?;
            require_success(
                run_git(repository, &["merge", "--ff-only", &upstream_oid])?,
                "快进本地分支",
            )?;
            Ok(PullPreparationOutcome::Completed(PullOutcome {
                updated: true,
                has_local_changes,
            }))
        }
        (false, false) => {
            validate_commit_documents(repository, &upstream_oid)?;
            after_validation()?;
            match rebase_or_capture_conflict(repository, &upstream_oid)? {
                RebasePreparationOutcome::Rebased => {
                    Ok(PullPreparationOutcome::Completed(PullOutcome {
                        updated: true,
                        has_local_changes,
                    }))
                }
                RebasePreparationOutcome::Conflict(snapshot) => {
                    Ok(PullPreparationOutcome::Conflict {
                        snapshot,
                        has_local_changes,
                    })
                }
            }
        }
    }
}

/// 确认暂存区为空，防止应用把用户预先暂存的内容带入自动提交。
fn ensure_empty_index(repository: &Path) -> Result<(), AppError> {
    let output = run_git(repository, &["diff", "--cached", "--quiet"])?;
    match output.status.code() {
        Some(0) => Ok(()),
        Some(1) => Err(AppError::new(
            "WORKTREE_DIRTY",
            "仓库暂存区已有内容，推送已停止。",
            "在系统终端提交或取消暂存现有内容后重试。",
            false,
        )),
        _ => Err(command_failure(&output, "检查仓库暂存区")),
    }
}

/// 拒绝两个受管数据文件之外的工作区变化，确保专用仓库边界可预测。
fn ensure_no_unrelated_changes(repository: &Path) -> Result<(), AppError> {
    let output = require_success(
        run_git(
            repository,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--",
                ".",
                ":(exclude)commands.json",
                ":(exclude)inbox.json",
            ],
        )?,
        "检查仓库其他文件",
    )?;
    if !stdout_text(&output).is_empty() {
        return Err(AppError::new(
            "WORKTREE_DIRTY",
            "仓库中存在受管数据文件之外的未提交变化。",
            "在系统终端处理其他文件后重试；应用只会提交 commands.json 与 inbox.json。",
            false,
        ));
    }
    Ok(())
}

/// 返回当前需要暂存的受管路径；`inbox.json` 尚未创建时保持旧仓库兼容。
///
/// 删除已跟踪的 `inbox.json` 仍必须纳入列表，确保 Git 可以记录删除而不是遗漏变化。
fn managed_data_files(repository: &Path) -> Result<Vec<&'static str>, AppError> {
    let mut files = vec!["commands.json"];
    if repository.join("inbox.json").exists() {
        files.push("inbox.json");
        return Ok(files);
    }

    let tracked = require_success(
        run_git(repository, &["ls-files", "--", "inbox.json"])?,
        "检查临时收集文件跟踪状态",
    )?;
    if !stdout_text(&tracked).is_empty() {
        files.push("inbox.json");
    }
    Ok(files)
}

/// 判断受管数据文件暂存后是否确有差异；退出码 1 表示存在差异。
fn staged_documents_changed(repository: &Path) -> Result<bool, AppError> {
    let output = run_git(
        repository,
        &[
            "diff",
            "--cached",
            "--quiet",
            "--",
            "commands.json",
            "inbox.json",
        ],
    )?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(command_failure(&output, "检查待提交数据")),
    }
}

/// 撤销本次自动流程对受管数据文件的暂存，不改变工作区文件内容。
///
/// 该清理只在自动提交失败时执行，使用户修复 Git 身份或钩子后可以直接重试。
fn unstage_documents(repository: &Path) -> Result<(), AppError> {
    require_success(
        run_git(
            repository,
            &["reset", "--quiet", "--", "commands.json", "inbox.json"],
        )?,
        "撤销应用暂存",
    )?;
    Ok(())
}

/// 在保留原始提交错误的同时清理应用创建的暂存；清理失败时给出显式人工恢复提示。
fn rollback_staging_after_commit_failure(repository: &Path, original: AppError) -> AppError {
    match unstage_documents(repository) {
        Ok(()) => original,
        Err(cleanup) => AppError::new(
            "GIT_FAILED",
            format!(
                "{}；同时无法撤销应用创建的暂存：{}",
                original.message, cleanup.message
            ),
            "本地文件仍然保留；请在系统终端取消暂存 commands.json 与 inbox.json 后重试。",
            true,
        ),
    }
}

/// 只暂存并提交两个受管数据文件；没有文件差异时不创建空提交。
///
/// 返回值表示本次调用是否创建了提交。提交失败时会撤销应用创建的暂存，但绝不回退工作区数据。
fn commit_managed_documents(repository: &Path) -> Result<bool, AppError> {
    let managed_files = managed_data_files(repository)?;
    let mut add_arguments = vec!["add", "--"];
    add_arguments.extend(managed_files.iter().copied());
    require_success(run_git(repository, &add_arguments)?, "暂存受管数据")?;
    if !staged_documents_changed(repository)? {
        return Ok(false);
    }

    let commit_result = (|| -> Result<(), AppError> {
        let message = sync_commit_message()?;
        let mut commit_arguments = vec!["commit", "-m", message.as_str(), "--"];
        commit_arguments.extend(managed_files.iter().copied());
        require_success(run_git(repository, &commit_arguments)?, "提交受管数据")?;
        Ok(())
    })();
    if let Err(error) = commit_result {
        return Err(rollback_staging_after_commit_failure(repository, error));
    }
    Ok(true)
}

/// 返回当前 rebase 中尚未解决的冲突路径；仅两个受管文件可进入应用内合并流程。
fn unresolved_conflict_paths(repository: &Path) -> Result<Vec<String>, AppError> {
    let output = require_success(
        run_git(repository, &["diff", "--name-only", "--diff-filter=U"])?,
        "检查受管文件冲突",
    )?;
    Ok(stdout_text(&output)
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

/// 尝试把本地提交重放到固定远端；受管文件冲突时退出 rebase 并返回三份固定快照。
///
/// 返回冲突前会确认只有 `commands.json` 或 `inbox.json` 未合并，并确保仓库已恢复到原本机提交。
pub fn rebase_or_capture_conflict(
    repository: &Path,
    upstream_oid: &str,
) -> Result<RebasePreparationOutcome, AppError> {
    let local_oid = resolve_commit_oid(repository, "HEAD", "解析本机提交")?;
    let base_oid = resolve_merge_base_oid(repository, &local_oid, upstream_oid)?;
    let output = run_git(repository, &["rebase", upstream_oid])?;
    if output.status.success() {
        return Ok(RebasePreparationOutcome::Rebased);
    }

    let original = command_failure(&output, "接入远端更新");
    let conflicts = unresolved_conflict_paths(repository)?;
    let abort = run_git(repository, &["rebase", "--abort"])?;
    if !abort.status.success() {
        let cleanup = command_failure(&abort, "中止远端更新");
        return Err(AppError::new(
            "GIT_FAILED",
            format!(
                "{}；同时无法自动退出 rebase：{}",
                original.message, cleanup.message
            ),
            "本地提交仍在仓库中；请在系统终端运行 git rebase --abort 后重试。",
            true,
        ));
    }

    let only_managed_conflicts = !conflicts.is_empty()
        && conflicts
            .iter()
            .all(|path| path == "commands.json" || path == "inbox.json");
    if !only_managed_conflicts {
        return Err(original);
    }

    let restored_oid = resolve_commit_oid(repository, "HEAD", "确认本机提交恢复")?;
    if restored_oid != local_oid {
        return Err(AppError::new(
            "GIT_FAILED",
            "退出 rebase 后本机分支没有恢复到原提交。",
            "本地数据仍然保留；请在系统终端检查分支状态后重试。",
            true,
        ));
    }
    let snapshot =
        capture_repository_conflict(repository, base_oid, local_oid, upstream_oid.to_string())?;
    Ok(RebasePreparationOutcome::Conflict(Box::new(snapshot)))
}

/// 保留旧同步接口的安全停止行为，直到上层完成冲突窗口接入。
fn rebase_onto_validated_upstream(repository: &Path, upstream_oid: &str) -> Result<(), AppError> {
    if matches!(
        rebase_or_capture_conflict(repository, upstream_oid)?,
        RebasePreparationOutcome::Rebased
    ) {
        return Ok(());
    }

    Err(AppError::new(
        "GIT_DIVERGED",
        "本地修改与远端更新存在冲突，自动同步已停止。",
        "本地提交和数据均已保留；应用内冲突窗口接入后即可继续处理。",
        false,
    ))
}

/// 使用已经过用户确认的完整文档完成冲突 rebase，并创建普通的受管数据提交。
///
/// 参数中的提交 OID 来自冲突会话；任一引用已经变化时会拒绝套用旧决议。
/// 副作用：重放本机提交、原子替换两份受管文档并按需创建一个本地提交；不会执行 push。
pub fn complete_conflict_rebase(
    repository: &Path,
    expected_local_oid: &str,
    expected_upstream_oid: &str,
    commands: &CommandDocument,
    inbox: &InboxDocument,
) -> Result<(), AppError> {
    ensure_empty_index(repository)?;
    ensure_no_unrelated_changes(repository)?;
    validate_document(commands)?;
    validate_inbox_document(inbox)?;
    let current_local_oid = resolve_commit_oid(repository, "HEAD", "确认冲突会话本机提交")?;
    let current_upstream_oid = resolve_upstream_commit_oid(repository)?;
    if current_local_oid != expected_local_oid || current_upstream_oid != expected_upstream_oid {
        return Err(AppError::new(
            "MERGE_SESSION_STALE",
            "冲突窗口打开后本机或远端提交已经变化。",
            "关闭当前窗口并重新点击拉取或推送，基于最新内容重新合并。",
            true,
        ));
    }

    let rebase = run_git(
        repository,
        &["rebase", "--strategy-option=ours", expected_upstream_oid],
    )?;
    if !rebase.status.success() {
        let original = command_failure(&rebase, "应用冲突决议前重放本机提交");
        let abort = run_git(repository, &["rebase", "--abort"])?;
        if !abort.status.success() {
            let cleanup = command_failure(&abort, "中止冲突决议 rebase");
            return Err(AppError::new(
                "GIT_FAILED",
                format!(
                    "{}；同时无法自动退出 rebase：{}",
                    original.message, cleanup.message
                ),
                "本地提交仍然保留；请在系统终端运行 git rebase --abort 后重试。",
                true,
            ));
        }
        return Err(original);
    }

    let command_bytes = serialize_document(commands)?;
    let inbox_bytes = serialize_inbox_document(inbox)?;
    atomic_write(&repository.join("commands.json"), &command_bytes)?;
    atomic_write(&repository.join("inbox.json"), &inbox_bytes)?;
    commit_managed_documents(repository)?;
    Ok(())
}

/// 生成不包含用户命令内容的固定自动提交消息。
fn sync_commit_message() -> Result<String, AppError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            AppError::new(
                "GIT_FAILED",
                format!("系统时间无法用于生成提交消息：{error}"),
                "校准系统时间后重试推送。",
                true,
            )
        })?
        .as_secs();
    Ok(format!("chore(data): sync CommandShelf data {seconds}"))
}

/// 执行显式安全推送：只提交数据文件，接入已校验远端更新，并且永不强制覆盖。
pub fn push_repository(repository: &Path) -> Result<PushOutcome, AppError> {
    ensure_empty_index(repository)?;
    ensure_no_unrelated_changes(repository)?;
    let ahead_before_commit = local_ahead_count(repository)?;
    let committed = commit_managed_documents(repository)?;
    if !committed && ahead_before_commit == 0 {
        return Ok(PushOutcome {
            committed: false,
            pushed: false,
        });
    }

    require_success(
        run_git(repository, &["fetch", "--prune", "origin"])?,
        "检查远端最新状态",
    )?;

    let upstream_oid = resolve_upstream_commit_oid(repository)?;
    let upstream_is_ancestor = is_ancestor(repository, &upstream_oid, "HEAD")?;
    if !upstream_is_ancestor {
        validate_commit_documents(repository, &upstream_oid)?;
        rebase_onto_validated_upstream(repository, &upstream_oid)?;
    }
    require_success(run_git(repository, &["push"])?, "推送受管数据")?;
    // `git push` 的零退出码就是远端接受提交的提交点；其后不再运行可能失败的状态确认。
    Ok(PushOutcome {
        committed,
        pushed: true,
    })
}

#[cfg(test)]
mod tests {
    //! 测试职责：确认仓库边界、Git 错误矩阵和超时终止都返回稳定结构化错误。

    use super::{
        classify_git_failure, pull_repository_with_after_validation, rebase_or_capture_conflict,
        run_git_process, spawn_failure, validate_repository, RebasePreparationOutcome,
    };
    use crate::command_store::load_document;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;
    #[cfg(windows)]
    use std::time::Instant;

    /// 运行测试 Git 命令；失败时输出标准错误，避免隐藏夹具问题。
    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .output()
            .expect("测试环境应能启动系统 Git");
        assert!(
            output.status.success(),
            "git {:?} 失败：{}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// 运行测试 Git 命令并返回去除末尾换行的标准输出。
    fn git_output(directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .output()
            .expect("测试环境应能启动系统 Git");
        assert!(
            output.status.success(),
            "git {:?} 失败：{}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// 生成包含单条命令的合法测试文档文本。
    fn document_json(title: &str) -> String {
        format!(
            r#"{{
  "schemaVersion": 1,
  "categories": [{{
    "id": "category-test",
    "name": "测试",
    "commands": [{{
      "id": "command-test",
      "title": "{title}",
      "command": "echo test",
      "outputExample": "test"
    }}]
  }}]
}}
"#
        )
    }

    /// 创建带初始有效文档和上游的本地远端、工作克隆与生产者克隆。
    fn pull_race_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let remote = root.join("remote.git");
        fs::create_dir_all(&remote).expect("应能创建远端目录");
        git(&remote, &["init", "--bare"]);

        let seed = root.join("seed");
        git(root, &["clone", remote.to_string_lossy().as_ref(), "seed"]);
        git(&seed, &["config", "user.name", "CommandShelf Test"]);
        git(
            &seed,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        fs::write(seed.join("commands.json"), document_json("本地初始命令"))
            .expect("应能写入初始文档");
        git(&seed, &["add", "commands.json"]);
        git(&seed, &["commit", "-m", "加入初始文档"]);
        git(&seed, &["push", "-u", "origin", "HEAD"]);

        let work = root.join("work");
        let producer = root.join("producer");
        git(root, &["clone", remote.to_string_lossy().as_ref(), "work"]);
        git(
            root,
            &["clone", remote.to_string_lossy().as_ref(), "producer"],
        );
        git(&producer, &["config", "user.name", "CommandShelf Test"]);
        git(
            &producer,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        (remote, work, producer)
    }

    /// 验证缺少 `.git` 的目录会返回稳定错误码。
    #[test]
    fn rejects_plain_directory() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let error = validate_repository(directory.path()).expect_err("普通目录应被拒绝");
        assert_eq!(error.code, "NOT_GIT_REPOSITORY");
    }

    /// 验证候选校验后即使远端跟踪引用推进，最终也只能快进到已验证提交。
    #[test]
    fn fast_forwards_only_to_validated_upstream_oid() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let (_remote, repository, producer) = pull_race_fixture(directory.path());
        fs::write(
            producer.join("commands.json"),
            document_json("已验证候选命令"),
        )
        .expect("应能写入有效候选");
        git(&producer, &["add", "commands.json"]);
        git(&producer, &["commit", "-m", "加入有效候选"]);
        git(&producer, &["push"]);
        let validated_oid = git_output(&producer, &["rev-parse", "HEAD"]);

        fs::write(producer.join("commands.json"), "{ invalid json").expect("应能写入无效后继文档");
        git(&producer, &["add", "commands.json"]);
        git(&producer, &["commit", "-m", "加入无效后继"]);
        let invalid_successor_oid = git_output(&producer, &["rev-parse", "HEAD"]);

        let outcome = pull_repository_with_after_validation(&repository, || {
            // 在候选通过校验后推进真实远端和本地跟踪引用，稳定复现重复解引用 `@{u}` 的竞态。
            git(&producer, &["push"]);
            git(&repository, &["fetch", "origin"]);
            Ok(())
        })
        .expect("引用推进不应改变已经选定的安全快进目标");

        assert!(outcome.updated);
        assert_eq!(
            git_output(&repository, &["rev-parse", "HEAD"]),
            validated_oid
        );
        assert_eq!(
            git_output(&repository, &["rev-parse", "@{u}"]),
            invalid_successor_oid,
            "测试 barrier 必须确实推进可变跟踪引用"
        );
        let (document, _) = load_document(&repository.join("commands.json"))
            .expect("工作区必须保留已校验的有效文档");
        assert_eq!(document.categories[0].commands[0].title, "已验证候选命令");
    }

    /// 验证受管文件 rebase 冲突会固定三方内容，并在返回预览前恢复干净的本机提交。
    #[test]
    fn captures_three_way_documents_and_aborts_rebase() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let (_remote, repository, producer) = pull_race_fixture(directory.path());
        git(&repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            &repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        let base_oid = git_output(&repository, &["rev-parse", "HEAD"]);

        fs::write(
            repository.join("commands.json"),
            document_json("本机修改命令"),
        )
        .expect("应能写入本机冲突版本");
        git(&repository, &["add", "commands.json"]);
        git(&repository, &["commit", "-m", "加入本机修改"]);
        let local_oid = git_output(&repository, &["rev-parse", "HEAD"]);

        fs::write(
            producer.join("commands.json"),
            document_json("远端修改命令"),
        )
        .expect("应能写入远端冲突版本");
        git(&producer, &["add", "commands.json"]);
        git(&producer, &["commit", "-m", "加入远端修改"]);
        git(&producer, &["push"]);
        git(&repository, &["fetch", "origin"]);
        let upstream_oid = git_output(&repository, &["rev-parse", "@{u}"]);

        let outcome = rebase_or_capture_conflict(&repository, &upstream_oid)
            .expect("受管文件冲突应返回应用内预览");
        let RebasePreparationOutcome::Conflict(snapshot) = outcome else {
            panic!("同一字段不同修改必须形成冲突快照");
        };

        assert_eq!(snapshot.base_oid, base_oid);
        assert_eq!(snapshot.local_oid, local_oid);
        assert_eq!(snapshot.upstream_oid, upstream_oid);
        assert_eq!(
            snapshot.base_commands.categories[0].commands[0].title,
            "本地初始命令"
        );
        assert_eq!(
            snapshot.local_commands.categories[0].commands[0].title,
            "本机修改命令"
        );
        assert_eq!(
            snapshot.remote_commands.categories[0].commands[0].title,
            "远端修改命令"
        );
        assert!(snapshot.base_inbox.items.is_empty());
        assert_eq!(git_output(&repository, &["rev-parse", "HEAD"]), local_oid);
        assert_eq!(git_output(&repository, &["status", "--porcelain"]), "");
        assert!(!repository.join(".git").join("rebase-merge").exists());
        assert!(!repository.join(".git").join("rebase-apply").exists());
    }

    /// 验证系统找不到 Git 时不会退化成含糊的通用错误。
    #[test]
    fn maps_missing_git_executable() {
        let mapped = spawn_failure(std::io::Error::from(ErrorKind::NotFound));
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let actual = run_git_process(
            directory.path(),
            "command-shelf-definitely-missing-git.exe",
            &[],
            Duration::from_secs(1),
        )
        .expect_err("真实缺失可执行文件应返回结构化错误");

        for error in [mapped, actual] {
            assert_eq!(error.code, "GIT_NOT_FOUND");
            assert!(error.retryable);
            assert!(error.message.contains("git.exe"));
            assert!(error.action.contains("Git for Windows"));
        }
    }

    /// 验证常见英文 Git 输出稳定映射到身份、认证、网络和拒绝错误。
    #[test]
    fn maps_common_git_failures() {
        let cases = [
            (
                "Author identity unknown\nPlease tell me who you are.",
                "GIT_IDENTITY_REQUIRED",
                true,
            ),
            (
                "fatal: Authentication failed for 'https://example.invalid/repo'",
                "GIT_AUTH_REQUIRED",
                true,
            ),
            (
                "fatal: unable to access: Could not resolve host: example.invalid",
                "GIT_NETWORK_FAILED",
                true,
            ),
            (
                "fatal: unable to access: Recv failure: Connection was reset",
                "GIT_NETWORK_FAILED",
                true,
            ),
            (
                "fatal: unable to access: Failed to connect: Connection refused",
                "GIT_NETWORK_FAILED",
                true,
            ),
            (
                "! [rejected] main -> main (non-fast-forward)",
                "GIT_PUSH_REJECTED",
                false,
            ),
        ];

        for (detail, expected_code, expected_retryable) in cases {
            let error = classify_git_failure(detail, "测试 Git 操作", "exit code: 1");
            assert_eq!(error.code, expected_code);
            assert_eq!(error.retryable, expected_retryable);
            assert!(!error.action.trim().is_empty());
        }
    }

    /// 验证超时会终止持有输出管道的整棵进程树，并在严格上限内返回。
    #[cfg(windows)]
    #[test]
    fn terminates_process_tree_after_timeout() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let process_directory = directory.path().to_path_buf();
        let execution = std::thread::spawn(move || {
            let started_at = Instant::now();
            let result = run_git_process(
                &process_directory,
                "powershell.exe",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$child = Start-Process -FilePath ping.exe -ArgumentList '127.0.0.1','-n','13' -NoNewWindow -PassThru; [System.IO.File]::WriteAllText('descendant.pid', [string]$child.Id); [Console]::Error.WriteLine($child.Id); Start-Sleep -Seconds 12",
                ],
                Duration::from_secs(4),
            );
            (result, started_at.elapsed())
        });

        let pid_path = directory.path().join("descendant.pid");
        let readiness_deadline = Instant::now() + Duration::from_millis(3_500);
        let mut descendant_pid = None;
        while Instant::now() < readiness_deadline {
            descendant_pid = fs::read_to_string(&pid_path)
                .ok()
                .and_then(|text| text.parse::<u32>().ok());
            if descendant_pid.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let ready_at = Instant::now();
        let (result, total_elapsed) = execution.join().expect("测试执行线程不应异常退出");
        let elapsed_after_ready = ready_at.elapsed();
        let descendant_pid = descendant_pid.unwrap_or_else(|| {
            panic!("全量并行环境中后代未在超时前启动；总耗时 {total_elapsed:?}")
        });
        let error = result.expect_err("带长寿命后代的慢进程应被测试超时终止");

        assert_eq!(error.code, "GIT_TIMEOUT");
        assert!(error.retryable);
        assert!(
            total_elapsed < Duration::from_secs(6),
            "超时总耗时 {total_elapsed:?}，说明后代仍持有管道并阻塞读取线程"
        );
        assert!(
            elapsed_after_ready < Duration::from_millis(5_500),
            "后代就绪后仍等待 {elapsed_after_ready:?}，进程树终止未在严格上限内完成"
        );
        let filter = format!("PID eq {descendant_pid}");
        let listing = Command::new("tasklist.exe")
            .args(["/FI", filter.as_str(), "/FO", "CSV", "/NH"])
            .output()
            .expect("应能检查测试后代是否残留");
        assert!(listing.status.success(), "tasklist 应能完成残留进程检查");
        let csv = String::from_utf8_lossy(&listing.stdout);
        assert!(
            !csv.contains(&format!(",\"{descendant_pid}\",")),
            "超时返回后仍残留测试后代进程 {descendant_pid}"
        );
    }
}
