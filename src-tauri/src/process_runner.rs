//! 文件职责：为桌面后端提供可复用的受控子进程执行边界。
//! 主要内容：执行固定程序和参数，限制输出体积、施加超时，并在 Windows 上清理完整进程树。
//! 重要约束：本模块不拼接 Shell 命令、不解释业务错误，也不在错误中记录程序参数或用户输入。

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::{ffi::c_void, os::windows::io::AsRawHandle};

/// Windows 的 `CREATE_NO_WINDOW` 标志，防止桌面应用调用子进程时闪出控制台。
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

// 最小 Win32 FFI 集合；避免为受控进程边界增加新的 Cargo feature 或执行框架。
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
    fn attach_and_resume(child: &Child) -> Result<Self, ProcessFailure> {
        // 安全依据：两个可选指针均为空，要求系统创建匿名且使用默认安全属性的 Job。
        let raw_job = unsafe { create_job_object(std::ptr::null(), std::ptr::null()) };
        if raw_job.is_null() {
            return Err(windows_process_boundary_failure("无法创建进程 Job Object"));
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
            return Err(windows_process_boundary_failure("无法配置进程 Job Object"));
        }

        let process_handle = child.as_raw_handle() as WindowsHandle;
        // 安全依据：子进程仍由 CREATE_SUSPENDED 挂起，句柄由 `Child` 保持有效且尚未派生后代。
        if unsafe { assign_process_to_job_object(tree.job.raw, process_handle) } == 0 {
            return Err(windows_process_boundary_failure(
                "无法把进程加入 Job Object",
            ));
        }
        resume_suspended_process(child.id())?;
        Ok(tree)
    }

    /// 主动终止整棵进程树并关闭 Job；关闭动作是 Terminate 失败时的第二层兜底。
    fn terminate(mut self) {
        // 安全依据：Job 句柄仍由守卫独占；退出码只用于被终止的测试或超时进程。
        let _ = unsafe { terminate_job_object(self.job.raw, 1) };
        let _ = self.job.close();
    }
}

/// 把 Windows 进程边界配置失败转成不包含程序参数的基础设施错误。
#[cfg(windows)]
fn windows_process_boundary_failure(operation: &'static str) -> ProcessFailure {
    ProcessFailure::Boundary {
        operation,
        source: std::io::Error::last_os_error(),
    }
}

/// 找到挂起创建进程的初始线程并恢复执行。
#[cfg(windows)]
fn resume_suspended_process(process_id: u32) -> Result<(), ProcessFailure> {
    // 安全依据：固定标志只请求只读线程快照，不传入任何用户内存。
    let snapshot = unsafe { create_toolhelp32_snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(windows_process_boundary_failure("无法枚举进程初始线程"));
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
                return Err(windows_process_boundary_failure("无法打开进程初始线程"));
            }
            let mut thread = OwnedWindowsHandle::new(thread);
            // 安全依据：目标是 CREATE_SUSPENDED 创建且尚未恢复的初始线程。
            let resume_result = unsafe { resume_thread(thread.raw) };
            let _ = thread.close();
            let _ = snapshot.close();
            if resume_result == u32::MAX {
                return Err(windows_process_boundary_failure("无法恢复进程初始线程"));
            }
            return Ok(());
        }
        // 安全依据：沿用同一有效快照和已初始化条目读取下一项。
        has_entry = unsafe { thread32_next(snapshot.raw, &mut entry) } != 0;
    }
    Err(ProcessFailure::Boundary {
        operation: "系统线程快照中没有找到进程初始线程",
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "初始线程不存在"),
    })
}

/// 受控进程启动和生命周期管理可能产生的基础设施错误。
#[derive(Debug)]
pub(crate) enum ProcessFailure {
    /// 操作系统拒绝启动程序，或 PATH 中不存在指定程序。
    Spawn(std::io::Error),
    /// Windows Job Object 或初始线程配置失败。
    Boundary {
        /// 不含程序名和参数的失败阶段。
        operation: &'static str,
        /// 操作系统返回的底层错误。
        source: std::io::Error,
    },
    /// 等待子进程状态时发生系统错误。
    Wait(std::io::Error),
    /// 标准输出或标准错误读取线程异常退出。
    ReaderPanicked {
        /// 失败的固定流名称。
        stream: &'static str,
    },
    /// 读取标准输出或标准错误时发生 I/O 错误。
    Read {
        /// 失败的固定流名称。
        stream: &'static str,
        /// 管道返回的底层错误。
        source: std::io::Error,
    },
    /// 进程超过调用方设置的时限，并已触发进程树清理。
    Timeout(Duration),
}

/// 受控进程的有限捕获结果；非零退出码仍由业务模块分类。
#[derive(Debug)]
pub(crate) struct ProcessOutput {
    /// 子进程退出状态。
    pub(crate) status: ExitStatus,
    /// 标准输出前若干字节；读取线程仍会排空超出部分以避免管道死锁。
    pub(crate) stdout: Vec<u8>,
    /// 标准错误前若干字节。
    pub(crate) stderr: Vec<u8>,
    /// 标准输出是否超出调用方设置的保留上限。
    pub(crate) stdout_truncated: bool,
    /// 标准错误是否超出调用方设置的保留上限。
    pub(crate) stderr_truncated: bool,
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

/// 等待并转换输出读取线程；线程异常和管道错误由业务模块决定如何呈现。
fn join_reader(
    reader: thread::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
    stream: &'static str,
) -> Result<(Vec<u8>, bool), ProcessFailure> {
    reader
        .join()
        .map_err(|_| ProcessFailure::ReaderPanicked { stream })?
        .map_err(|source| ProcessFailure::Read { stream, source })
}

/// 以参数数组执行受控进程，并施加无窗口、超时和独立输出上限。
///
/// 参数：程序、参数和环境变量均由调用方分别传入，不会经过 Shell 拼接；错误不会记录这些内容。
/// 返回值：成功启动并退出时返回有限捕获结果，非零退出码仍作为正常进程结果交给业务模块。
/// 副作用：会启动本机进程；超时或正常退出后都会终止仍附着在 Windows Job Object 中的后代。
pub(crate) fn run_process(
    current_directory: &Path,
    executable: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<ProcessOutput, ProcessFailure> {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .current_dir(current_directory)
        .envs(environment.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_windows_process(&mut command);
    let mut child = command.spawn().map_err(ProcessFailure::Spawn)?;
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
        .expect("已配置管道的受控进程必须提供标准输出");
    let stderr = child
        .stderr
        .take()
        .expect("已配置管道的受控进程必须提供标准错误");
    let stdout_reader = thread::spawn(move || read_limited(stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_limited(stderr, stderr_limit));
    let started_at = Instant::now();

    let status = loop {
        if let Some(status) = child.try_wait().map_err(ProcessFailure::Wait)? {
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
            return Err(ProcessFailure::Timeout(timeout));
        }
        thread::sleep(Duration::from_millis(25));
    };

    #[cfg(windows)]
    if let Some(tree) = process_tree.take() {
        // 主进程正常退出后仍清理可能遗留的 helper，防止其无限持有管道。
        tree.terminate();
    }

    let (stdout, stdout_truncated) = join_reader(stdout_reader, "标准输出")?;
    let (stderr, stderr_truncated) = join_reader(stderr_reader, "标准错误")?;
    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}
