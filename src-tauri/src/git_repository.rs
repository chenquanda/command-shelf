//! 文件职责：验证专用数据仓库、读取同步状态并执行安全快进拉取。
//! 主要内容：固定调用受控 Git 子命令，提供非交互、无窗口、限时和有限输出的进程边界。
//! 重要约束：程序名与参数由后端决定；禁止 Shell 字符串、merge commit、rebase、reset 和 force。

use crate::command_store::parse_document_bytes;
use crate::error::AppError;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Windows 的 `CREATE_NO_WINDOW` 标志，防止桌面应用调用 Git 时闪出控制台。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
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

/// 一次拉取是否实际推进了本地分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullOutcome {
    /// `true` 表示本地 `HEAD` 已快进到新的上游提交。
    pub updated: bool,
}

/// 一次推送是否创建了提交并实际访问远端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushOutcome {
    /// 本次操作是否为未提交的 `commands.json` 创建了新提交。
    pub committed: bool,
    /// 本次操作是否执行并成功完成了普通 `git push`。
    pub pushed: bool,
}

/// Git 子进程的有限捕获结果。
#[derive(Debug)]
struct GitOutput {
    /// 子进程退出状态。
    status: ExitStatus,
    /// 标准输出前若干字节；读取线程仍会排空超出部分以避免管道死锁。
    stdout: Vec<u8>,
    /// 标准错误前若干字节。
    stderr: Vec<u8>,
    /// 标准输出是否超出保留上限。
    stdout_truncated: bool,
    /// 标准错误是否超出保留上限。
    stderr_truncated: bool,
}

/// 为系统 Git 命令添加 Windows 无窗口选项。
fn hide_console_window(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

/// 排空进程输出但只保留有限字节，兼顾大输出安全与子进程退出。
fn read_limited<R: Read>(mut reader: R, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut captured = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        let retained = remaining.min(read);
        captured.extend_from_slice(&buffer[..retained]);
        if retained < read {
            truncated = true;
        }
    }
    Ok((captured, truncated))
}

/// 等待并转换输出读取线程；线程异常不得被当成正常 Git 结果。
fn join_reader(
    reader: thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
    stream_name: &str,
) -> Result<(Vec<u8>, bool), AppError> {
    reader
        .join()
        .map_err(|_| {
            AppError::new(
                "GIT_FAILED",
                format!("读取 Git {stream_name}的线程异常退出。"),
                "重新启动应用后重试。",
                true,
            )
        })?
        .map_err(|error| {
            AppError::new(
                "GIT_FAILED",
                format!("无法读取 Git {stream_name}：{error}"),
                "检查系统 Git 后重试。",
                true,
            )
        })
}

/// 以固定程序和参数数组执行 Git，并施加无窗口、非交互、超时和输出上限。
fn run_git(repository: &Path, arguments: &[&str]) -> Result<GitOutput, AppError> {
    let mut command = Command::new("git");
    command
        .args(arguments)
        .current_dir(repository)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console_window(&mut command);
    let mut child = command.spawn().map_err(|error| {
        let (code, message) = if error.kind() == std::io::ErrorKind::NotFound {
            ("GIT_NOT_FOUND", "系统中没有找到 git.exe。".to_string())
        } else {
            ("GIT_FAILED", format!("无法启动系统 Git：{error}"))
        };
        AppError::new(
            code,
            message,
            "安装 Git for Windows，或确认 git.exe 已加入 PATH 后重试。",
            true,
        )
    })?;

    let stdout = child
        .stdout
        .take()
        .expect("已配置管道的 Git 必须提供标准输出");
    let stderr = child
        .stderr
        .take()
        .expect("已配置管道的 Git 必须提供标准错误");
    let stdout_reader = thread::spawn(move || read_limited(stdout, STDOUT_CAPTURE_LIMIT));
    let stderr_reader = thread::spawn(move || read_limited(stderr, STDERR_CAPTURE_LIMIT));
    let started_at = Instant::now();

    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            AppError::new(
                "GIT_FAILED",
                format!("无法等待系统 Git：{error}"),
                "重新启动应用后重试。",
                true,
            )
        })? {
            break status;
        }
        if started_at.elapsed() >= GIT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_reader(stdout_reader, "标准输出");
            let _ = join_reader(stderr_reader, "标准错误");
            return Err(AppError::new(
                "GIT_TIMEOUT",
                "系统 Git 运行超过 60 秒，操作已终止。",
                "检查网络和凭据后重试；本地命令数据未被覆盖。",
                true,
            ));
        }
        thread::sleep(Duration::from_millis(25));
    };

    let (stdout, stdout_truncated) = join_reader(stdout_reader, "标准输出")?;
    let (stderr, stderr_truncated) = join_reader(stderr_reader, "标准错误")?;
    Ok(GitOutput {
        status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
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

/// 根据常见 Git 失败文本映射认证、网络和通用错误。
fn command_failure(output: &GitOutput, operation: &str) -> AppError {
    let detail = stderr_text(output);
    let normalized = detail.to_lowercase();
    if normalized.contains("author identity unknown")
        || normalized.contains("unable to auto-detect email address")
        || normalized.contains("please tell me who you are")
    {
        return AppError::new(
            "GIT_IDENTITY_REQUIRED",
            "系统 Git 尚未配置提交姓名或邮箱。",
            "先在系统终端配置 Git user.name 与 user.email，再重试推送。",
            true,
        );
    }
    if normalized.contains("non-fast-forward") || normalized.contains("[rejected]") {
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
            format!("{operation}失败，Git 退出码为 {}。", output.status)
        } else {
            format!("{operation}失败：{detail}")
        },
        "在系统终端检查仓库状态后重试。",
        true,
    )
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

/// 判断数据文件未提交或当前分支存在尚未推送的提交。
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
            ],
        )?,
        "检查 commands.json 状态",
    )?;
    if !stdout_text(&status_output).is_empty() {
        return Ok(true);
    }
    Ok(local_ahead_count(repository)? > 0)
}

/// 拉取前要求整个专用数据仓库干净，避免 Git 快进被其他文件阻塞。
fn ensure_clean_worktree(repository: &Path) -> Result<(), AppError> {
    let output = require_success(
        run_git(
            repository,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?,
        "检查仓库工作区",
    )?;
    if !stdout_text(&output).is_empty() {
        return Err(AppError::new(
            "LOCAL_CHANGES",
            "本地仓库还有未提交修改，拉取已停止。",
            "先推送 CommandShelf 本地修改，或在系统终端处理其他文件后重试。",
            false,
        ));
    }
    if local_ahead_count(repository)? > 0 {
        return Err(AppError::new(
            "LOCAL_COMMITS",
            "当前分支还有尚未推送的本地提交，拉取已停止。",
            "先推送本地提交，再拉取远端更新。",
            false,
        ));
    }
    Ok(())
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

/// 从远端跟踪分支读取并校验候选 `commands.json`，不改变当前工作区。
fn validate_upstream_document(repository: &Path) -> Result<(), AppError> {
    let upstream_output = require_success(
        run_git(
            repository,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )?,
        "读取当前上游",
    )?;
    let upstream = stdout_text(&upstream_output);
    let object_spec = format!("{upstream}:commands.json");
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
    parse_document_bytes(&candidate.stdout).map_err(|error| {
        AppError::new(
            "REMOTE_DATA_INVALID",
            format!("远端 commands.json 未通过校验：{}", error.message),
            "在另一台电脑修复并推送有效数据后重试；本地文件未改变。",
            false,
        )
    })?;
    Ok(())
}

/// 执行显式安全拉取：先检查本地，再 fetch，校验远端候选，最后只允许快进。
pub fn pull_repository(repository: &Path) -> Result<PullOutcome, AppError> {
    ensure_clean_worktree(repository)?;
    require_success(
        run_git(repository, &["fetch", "--prune", "origin"])?,
        "拉取远端引用",
    )?;

    let local_is_ancestor = is_ancestor(repository, "HEAD", "@{u}")?;
    let upstream_is_ancestor = is_ancestor(repository, "@{u}", "HEAD")?;
    match (local_is_ancestor, upstream_is_ancestor) {
        (true, true) => Ok(PullOutcome { updated: false }),
        (true, false) => {
            validate_upstream_document(repository)?;
            require_success(
                run_git(repository, &["merge", "--ff-only", "@{u}"])?,
                "快进本地分支",
            )?;
            Ok(PullOutcome { updated: true })
        }
        (false, true) => Err(AppError::new(
            "LOCAL_COMMITS",
            "当前分支包含尚未推送的本地提交，拉取已停止。",
            "先推送本地提交后再拉取。",
            false,
        )),
        (false, false) => Err(AppError::new(
            "GIT_DIVERGED",
            "本地分支与远端已经分叉，应用不会自动合并。",
            "在系统终端处理分叉并恢复干净工作区后重试。",
            false,
        )),
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

/// 拒绝 `commands.json` 之外的工作区变化，确保专用仓库边界可预测。
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
            ],
        )?,
        "检查仓库其他文件",
    )?;
    if !stdout_text(&output).is_empty() {
        return Err(AppError::new(
            "WORKTREE_DIRTY",
            "仓库中存在 commands.json 之外的未提交变化。",
            "在系统终端处理其他文件后重试；应用只会提交 commands.json。",
            false,
        ));
    }
    Ok(())
}

/// 判断 `commands.json` 暂存后是否确有差异；退出码 1 表示存在差异。
fn staged_document_changed(repository: &Path) -> Result<bool, AppError> {
    let output = run_git(
        repository,
        &["diff", "--cached", "--quiet", "--", "commands.json"],
    )?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(command_failure(&output, "检查待提交数据")),
    }
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

/// 执行显式安全推送：只提交数据文件，预检远端关系，并且永不强制覆盖。
pub fn push_repository(repository: &Path) -> Result<PushOutcome, AppError> {
    ensure_empty_index(repository)?;
    ensure_no_unrelated_changes(repository)?;
    require_success(
        run_git(repository, &["fetch", "--prune", "origin"])?,
        "检查远端最新状态",
    )?;

    let upstream_is_ancestor = is_ancestor(repository, "@{u}", "HEAD")?;
    if !upstream_is_ancestor {
        if is_ancestor(repository, "HEAD", "@{u}")? {
            return Err(AppError::new(
                "REMOTE_AHEAD",
                "远端包含本地尚未拉取的提交，推送已停止。",
                "保留当前本地修改，在系统终端处理远端更新后再重试。",
                false,
            ));
        }
        return Err(AppError::new(
            "GIT_DIVERGED",
            "本地分支与远端已经分叉，应用不会自动合并或强制推送。",
            "在系统终端处理分叉后重新打开应用。",
            false,
        ));
    }

    require_success(
        run_git(repository, &["add", "--", "commands.json"])?,
        "暂存命令数据",
    )?;
    let committed = if staged_document_changed(repository)? {
        let message = sync_commit_message()?;
        require_success(
            run_git(
                repository,
                &["commit", "-m", message.as_str(), "--", "commands.json"],
            )?,
            "提交命令数据",
        )?;
        true
    } else {
        false
    };

    if local_ahead_count(repository)? == 0 {
        return Ok(PushOutcome {
            committed,
            pushed: false,
        });
    }

    require_success(run_git(repository, &["push"])?, "推送命令数据")?;
    if local_ahead_count(repository)? > 0 {
        // 某些 Git 配置不会在 push 后立即刷新远端跟踪引用，再 fetch 一次用于最终事实确认。
        require_success(
            run_git(repository, &["fetch", "--prune", "origin"])?,
            "确认推送结果",
        )?;
    }
    if local_ahead_count(repository)? > 0 {
        return Err(AppError::new(
            "GIT_FAILED",
            "推送结束后本地仍显示未推送提交，状态无法确认。",
            "在系统终端运行 git status 确认远端状态。",
            true,
        ));
    }
    Ok(PushOutcome {
        committed,
        pushed: true,
    })
}

#[cfg(test)]
mod tests {
    //! 测试职责：确认普通目录不会被误识别成可连接数据仓库。

    use super::validate_repository;

    /// 验证缺少 `.git` 的目录会返回稳定错误码。
    #[test]
    fn rejects_plain_directory() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let error = validate_repository(directory.path()).expect_err("普通目录应被拒绝");
        assert_eq!(error.code, "NOT_GIT_REPOSITORY");
    }
}
