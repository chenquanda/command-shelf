//! 文件职责：验证专用数据仓库、读取同步状态并执行安全快进拉取。
//! 主要内容：固定调用受控 Git 子命令，提供非交互、无窗口、限时和有限输出的进程边界。
//! 重要约束：程序名与参数由后端决定；禁止 Shell 字符串、merge commit、rebase、reset 和 force。

use crate::command_store::parse_document_bytes;
use crate::error::AppError;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::{ffi::c_void, os::windows::io::AsRawHandle};

/// Windows 的 `CREATE_NO_WINDOW` 标志，防止桌面应用调用 Git 时闪出控制台。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Windows 挂起创建标志，确保子进程在加入 Job Object 前不能派生逃逸后代。
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;
/// Job Object 关闭时终止其中全部进程，保证输出管道最终释放。
#[cfg(windows)]
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
/// `SetInformationJobObject` 使用的扩展限制信息类别。
#[cfg(windows)]
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
/// Toolhelp 只枚举线程，用于恢复挂起创建进程的唯一初始线程。
#[cfg(windows)]
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
/// 恢复线程所需的最小访问权限。
#[cfg(windows)]
const THREAD_SUSPEND_RESUME: u32 = 0x0000_0002;
/// Windows 无效句柄哨兵值。
#[cfg(windows)]
const INVALID_HANDLE_VALUE: WindowsHandle = -1_isize as WindowsHandle;
/// 单个 Git 子进程允许运行的最长时间。
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// 标准输出最多保留 12 MB，足以容纳第一版 10 MB 的候选数据及少量余量。
const STDOUT_CAPTURE_LIMIT: usize = 12 * 1024 * 1024;
/// 标准错误最多保留 128 KB，避免异常 Git 进程无限占用内存。
const STDERR_CAPTURE_LIMIT: usize = 128 * 1024;

/// 本文件所需的 Win32 内核句柄类型；仅用于受控进程树生命周期。
#[cfg(windows)]
type WindowsHandle = *mut c_void;

/// Job Object 基础限制信息的 ABI 镜像。
///
/// 字段必须与 Win32 `JOBOBJECT_BASIC_LIMIT_INFORMATION` 顺序和宽度一致。
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct JobObjectBasicLimitInformation {
    /// 单进程用户态时间限制；当前保持零表示不设置。
    per_process_user_time_limit: i64,
    /// 整个 Job 用户态时间限制；当前保持零表示不设置。
    per_job_user_time_limit: i64,
    /// 本实现只设置“关闭即终止”标志。
    limit_flags: u32,
    /// 最小工作集限制；当前不设置。
    minimum_working_set_size: usize,
    /// 最大工作集限制；当前不设置。
    maximum_working_set_size: usize,
    /// 活动进程数限制；当前不设置。
    active_process_limit: u32,
    /// CPU 亲和性限制；当前不设置。
    affinity: usize,
    /// 优先级限制；当前不设置。
    priority_class: u32,
    /// 调度类别限制；当前不设置。
    scheduling_class: u32,
}

/// Win32 I/O 计数器的 ABI 镜像；扩展 Job 信息要求保留该占位布局。
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    /// 读操作次数。
    read_operation_count: u64,
    /// 写操作次数。
    write_operation_count: u64,
    /// 其他操作次数。
    other_operation_count: u64,
    /// 读取字节数。
    read_transfer_count: u64,
    /// 写入字节数。
    write_transfer_count: u64,
    /// 其他传输字节数。
    other_transfer_count: u64,
}

/// Job Object 扩展限制信息的 ABI 镜像，只启用关闭时终止进程树。
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct JobObjectExtendedLimitInformation {
    /// Job 基础限制。
    basic_limit_information: JobObjectBasicLimitInformation,
    /// 未使用但 ABI 必需的 I/O 计数器。
    io_info: IoCounters,
    /// 单进程内存限制；当前不设置。
    process_memory_limit: usize,
    /// Job 内存限制；当前不设置。
    job_memory_limit: usize,
    /// 单进程内存峰值输出；设置限制时保持零。
    peak_process_memory_used: usize,
    /// Job 内存峰值输出；设置限制时保持零。
    peak_job_memory_used: usize,
}

/// Toolhelp 线程条目的 ABI 镜像；挂起创建保证目标进程此时只有初始线程。
#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ThreadEntry32 {
    /// 调用前必须写入结构体字节数。
    size: u32,
    /// 保留字段。
    usage_count: u32,
    /// 线程 ID。
    thread_id: u32,
    /// 所属进程 ID。
    owner_process_id: u32,
    /// 基础优先级。
    base_priority: i32,
    /// 优先级增量。
    delta_priority: i32,
    /// 保留标志。
    flags: u32,
}

// 最小 Win32 FFI 集合；避免为已有代码增加新的 Cargo feature 或执行框架。
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    /// 创建匿名 Job Object。
    #[link_name = "CreateJobObjectW"]
    fn create_job_object(attributes: *const c_void, name: *const u16) -> WindowsHandle;
    /// 设置 Job Object 的扩展限制。
    #[link_name = "SetInformationJobObject"]
    fn set_information_job_object(
        job: WindowsHandle,
        information_class: i32,
        information: *const c_void,
        information_length: u32,
    ) -> i32;
    /// 把挂起的直接子进程加入 Job Object。
    #[link_name = "AssignProcessToJobObject"]
    fn assign_process_to_job_object(job: WindowsHandle, process: WindowsHandle) -> i32;
    /// 终止 Job Object 内仍存活的直接和间接后代。
    #[link_name = "TerminateJobObject"]
    fn terminate_job_object(job: WindowsHandle, exit_code: u32) -> i32;
    /// 关闭内核句柄。
    #[link_name = "CloseHandle"]
    fn close_handle(handle: WindowsHandle) -> i32;
    /// 创建系统线程快照。
    #[link_name = "CreateToolhelp32Snapshot"]
    fn create_toolhelp32_snapshot(flags: u32, process_id: u32) -> WindowsHandle;
    /// 读取快照中的第一个线程。
    #[link_name = "Thread32First"]
    fn thread32_first(snapshot: WindowsHandle, entry: *mut ThreadEntry32) -> i32;
    /// 读取快照中的下一个线程。
    #[link_name = "Thread32Next"]
    fn thread32_next(snapshot: WindowsHandle, entry: *mut ThreadEntry32) -> i32;
    /// 打开目标初始线程。
    #[link_name = "OpenThread"]
    fn open_thread(access: u32, inherit_handle: i32, thread_id: u32) -> WindowsHandle;
    /// 恢复挂起线程；返回 `u32::MAX` 表示失败。
    #[link_name = "ResumeThread"]
    fn resume_thread(thread: WindowsHandle) -> u32;
}

/// 独占 Win32 句柄，确保正常、错误和 panic 路径都不会泄漏。
#[cfg(windows)]
struct OwnedWindowsHandle {
    /// 当前持有的原始句柄；关闭后设为空以防二次关闭。
    raw: WindowsHandle,
}

#[cfg(windows)]
impl OwnedWindowsHandle {
    /// 接管一个已经验证不为空且不等于无效哨兵的句柄。
    fn new(raw: WindowsHandle) -> Self {
        Self { raw }
    }

    /// 显式关闭句柄并报告系统错误；Drop 仍负责异常路径兜底。
    fn close(&mut self) -> std::io::Result<()> {
        if self.raw.is_null() || self.raw == INVALID_HANDLE_VALUE {
            return Ok(());
        }
        let raw = self.raw;
        self.raw = std::ptr::null_mut();
        // 安全依据：`raw` 由本类型独占且尚未关闭，调用后立即清空避免重复释放。
        if unsafe { close_handle(raw) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    /// 异常路径尽力关闭句柄；Job 的 kill-on-close 同时是后代进程清理兜底。
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Windows Job Object 守卫；直接进程必须在恢复执行前完成绑定。
#[cfg(windows)]
struct WindowsProcessTree {
    /// 配置了 kill-on-close 的匿名 Job Object。
    job: OwnedWindowsHandle,
}

#[cfg(windows)]
impl WindowsProcessTree {
    /// 创建 Job、设置关闭即终止、绑定挂起子进程并恢复其初始线程。
    fn attach_and_resume(child: &Child) -> Result<Self, AppError> {
        // 安全依据：两个可选指针均为空，要求系统创建匿名且使用默认安全属性的 Job。
        let raw_job = unsafe { create_job_object(std::ptr::null(), std::ptr::null()) };
        if raw_job.is_null() {
            return Err(windows_process_boundary_error(
                "无法创建 Git 进程 Job Object",
            ));
        }
        let tree = Self {
            job: OwnedWindowsHandle::new(raw_job),
        };
        let mut limits = JobObjectExtendedLimitInformation::default();
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // 安全依据：`limits` 使用与 Win32 完全一致的 `repr(C)` 布局，指针和长度仅在调用期间有效。
        let configured = unsafe {
            set_information_job_object(
                tree.job.raw,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                (&limits as *const JobObjectExtendedLimitInformation).cast(),
                std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
            )
        };
        if configured == 0 {
            return Err(windows_process_boundary_error(
                "无法配置 Git 进程 Job Object",
            ));
        }

        let process_handle = child.as_raw_handle() as WindowsHandle;
        // 安全依据：子进程仍由 CREATE_SUSPENDED 挂起，句柄由 `Child` 保持有效且尚未派生后代。
        if unsafe { assign_process_to_job_object(tree.job.raw, process_handle) } == 0 {
            return Err(windows_process_boundary_error(
                "无法把 Git 进程加入 Job Object",
            ));
        }
        resume_suspended_process(child.id())?;
        Ok(tree)
    }

    /// 主动终止整棵进程树并关闭 Job；关闭动作是 Terminate 失败时的第二层兜底。
    fn terminate(mut self) {
        // 安全依据：Job 句柄仍由守卫独占；退出码只用于被终止的测试/超时进程。
        let _ = unsafe { terminate_job_object(self.job.raw, 1) };
        let _ = self.job.close();
    }
}

/// 把 Windows 进程边界配置失败映射为不泄露命令内容的稳定错误。
#[cfg(windows)]
fn windows_process_boundary_error(operation: &str) -> AppError {
    let error = std::io::Error::last_os_error();
    AppError::new(
        "GIT_FAILED",
        format!("{operation}：{error}"),
        "重新启动应用后重试；若持续失败，请在系统终端执行 Git 操作。",
        true,
    )
}

/// 找到挂起创建进程的初始线程并恢复执行。
#[cfg(windows)]
fn resume_suspended_process(process_id: u32) -> Result<(), AppError> {
    // 安全依据：固定标志只请求只读线程快照，不传入任何用户内存。
    let snapshot = unsafe { create_toolhelp32_snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(windows_process_boundary_error("无法枚举 Git 初始线程"));
    }
    let mut snapshot = OwnedWindowsHandle::new(snapshot);
    let mut entry = ThreadEntry32 {
        size: std::mem::size_of::<ThreadEntry32>() as u32,
        ..ThreadEntry32::default()
    };
    // 安全依据：`entry` 是可写且大小已初始化的 ABI 结构，快照句柄在整个循环中有效。
    let mut has_entry = unsafe { thread32_first(snapshot.raw, &mut entry) } != 0;
    while has_entry {
        if entry.owner_process_id == process_id {
            // 安全依据：线程 ID 来自同一系统快照，只申请恢复所需的最小权限且不继承句柄。
            let thread = unsafe { open_thread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
            if thread.is_null() {
                return Err(windows_process_boundary_error("无法打开 Git 初始线程"));
            }
            let mut thread = OwnedWindowsHandle::new(thread);
            // 安全依据：目标是 CREATE_SUSPENDED 创建且尚未恢复的初始线程。
            let resume_result = unsafe { resume_thread(thread.raw) };
            let _ = thread.close();
            let _ = snapshot.close();
            if resume_result == u32::MAX {
                return Err(windows_process_boundary_error("无法恢复 Git 初始线程"));
            }
            return Ok(());
        }
        // 安全依据：沿用同一有效快照和已初始化条目读取下一项。
        has_entry = unsafe { thread32_next(snapshot.raw, &mut entry) } != 0;
    }
    Err(AppError::new(
        "GIT_FAILED",
        "系统线程快照中没有找到 Git 初始线程。",
        "重新启动应用后重试。",
        true,
    ))
}

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

/// 配置 Windows 子进程为无窗口且挂起创建，待绑定 Job Object 后再恢复。
fn configure_windows_process(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
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

/// 以指定程序和参数数组执行受控进程，并施加无窗口、非交互、超时和输出上限。
///
/// 参数：正式路径固定传入 `git` 与 60 秒；测试路径可注入无副作用的慢命令验证终止逻辑。
/// 返回值：成功启动并退出时返回有限捕获结果，非零退出码仍由上层按具体操作分类。
fn run_process(
    repository: &Path,
    executable: &str,
    arguments: &[&str],
    timeout: Duration,
) -> Result<GitOutput, AppError> {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .current_dir(repository)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        // Git for Windows 尊重该 locale；固定英文诊断后结构化分类不依赖用户系统语言。
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_windows_process(&mut command);
    let mut child = command.spawn().map_err(spawn_failure)?;
    #[cfg(windows)]
    let mut process_tree = match WindowsProcessTree::attach_and_resume(&child) {
        Ok(tree) => Some(tree),
        Err(error) => {
            // 挂起进程尚未运行用户代码；直接终止并回收，避免配置失败留下孤儿进程。
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };

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
        if started_at.elapsed() >= timeout {
            #[cfg(windows)]
            if let Some(tree) = process_tree.take() {
                // 必须先终止并关闭 Job，再等待读取线程；后代可能继承了输出管道写端。
                tree.terminate();
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_reader(stdout_reader, "标准输出");
            let _ = join_reader(stderr_reader, "标准错误");
            return Err(timeout_failure(timeout));
        }
        thread::sleep(Duration::from_millis(25));
    };

    #[cfg(windows)]
    if let Some(tree) = process_tree.take() {
        // Git 主进程正常退出后仍清理可能遗留的 helper，防止其无限持有管道。
        tree.terminate();
    }

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

/// 以固定系统 Git 和正式超时时限执行仓库命令。
fn run_git(repository: &Path, arguments: &[&str]) -> Result<GitOutput, AppError> {
    run_process(repository, "git", arguments, GIT_TIMEOUT)
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

/// 从固定提交 OID 读取并校验候选 `commands.json`，不改变当前工作区。
fn validate_commit_document(repository: &Path, commit_oid: &str) -> Result<(), AppError> {
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

/// 执行显式安全拉取：先检查本地，再 fetch，校验远端候选，最后只允许快进。
pub fn pull_repository(repository: &Path) -> Result<PullOutcome, AppError> {
    pull_repository_with_after_validation(repository, || Ok(()))
}

/// 执行拉取核心流程；校验后回调用于确定性测试可变远端跟踪引用竞态。
fn pull_repository_with_after_validation<F>(
    repository: &Path,
    after_validation: F,
) -> Result<PullOutcome, AppError>
where
    F: FnOnce() -> Result<(), AppError>,
{
    ensure_clean_worktree(repository)?;
    require_success(
        run_git(repository, &["fetch", "--prune", "origin"])?,
        "拉取远端引用",
    )?;

    let upstream_oid = resolve_upstream_commit_oid(repository)?;
    let local_is_ancestor = is_ancestor(repository, "HEAD", &upstream_oid)?;
    let upstream_is_ancestor = is_ancestor(repository, &upstream_oid, "HEAD")?;
    match (local_is_ancestor, upstream_is_ancestor) {
        (true, true) => Ok(PullOutcome { updated: false }),
        (true, false) => {
            validate_commit_document(repository, &upstream_oid)?;
            after_validation()?;
            require_success(
                run_git(repository, &["merge", "--ff-only", &upstream_oid])?,
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

/// 撤销本次自动流程对 `commands.json` 的暂存，不改变工作区文件内容。
///
/// 该清理只在自动提交失败时执行，使用户修复 Git 身份或钩子后可以直接重试。
fn unstage_document(repository: &Path) -> Result<(), AppError> {
    require_success(
        run_git(repository, &["reset", "--quiet", "--", "commands.json"])?,
        "撤销应用暂存",
    )?;
    Ok(())
}

/// 在保留原始提交错误的同时清理应用创建的暂存；清理失败时给出显式人工恢复提示。
fn rollback_staging_after_commit_failure(repository: &Path, original: AppError) -> AppError {
    match unstage_document(repository) {
        Ok(()) => original,
        Err(cleanup) => AppError::new(
            "GIT_FAILED",
            format!(
                "{}；同时无法撤销应用创建的暂存：{}",
                original.message, cleanup.message
            ),
            "本地文件仍然保留；请在系统终端取消暂存 commands.json 后重试。",
            true,
        ),
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
    // 所有 Git 状态查询都必须在本次可能创建提交之前完成，避免提交后查询失败被误报为无副作用失败。
    let ahead_before_commit = local_ahead_count(repository)?;

    require_success(
        run_git(repository, &["add", "--", "commands.json"])?,
        "暂存命令数据",
    )?;
    let committed = if staged_document_changed(repository)? {
        let commit_result = (|| -> Result<(), AppError> {
            let message = sync_commit_message()?;
            require_success(
                run_git(
                    repository,
                    &["commit", "-m", message.as_str(), "--", "commands.json"],
                )?,
                "提交命令数据",
            )?;
            Ok(())
        })();
        if let Err(error) = commit_result {
            return Err(rollback_staging_after_commit_failure(repository, error));
        }
        true
    } else {
        false
    };

    if !committed && ahead_before_commit == 0 {
        return Ok(PushOutcome {
            committed,
            pushed: false,
        });
    }

    require_success(run_git(repository, &["push"])?, "推送命令数据")?;
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
        classify_git_failure, pull_repository_with_after_validation, run_process, spawn_failure,
        validate_repository,
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

    /// 验证系统找不到 Git 时不会退化成含糊的通用错误。
    #[test]
    fn maps_missing_git_executable() {
        let mapped = spawn_failure(std::io::Error::from(ErrorKind::NotFound));
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let actual = run_process(
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
            let result = run_process(
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
