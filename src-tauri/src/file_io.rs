//! 文件职责：提供同目录临时文件与写穿替换组成的原子写入原语。
//! 主要内容：先完整写入并刷新临时文件，再在 Windows 上一次性替换目标路径。
//! 重要约束：失败时尽力删除临时文件，绝不能先删除最后一份有效目标文件。

use crate::error::AppError;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

/// 进程内临时文件序号，避免同一毫秒内多次保存使用相同路径。
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 在目标目录生成不会进入 Git 管理的临时文件路径。
fn temporary_path(target: &Path) -> Result<PathBuf, AppError> {
    let parent = target.parent().ok_or_else(|| {
        AppError::new(
            "WRITE_FAILED",
            "目标文件没有可写父目录。",
            "重新选择有效的数据仓库后重试。",
            true,
        )
    })?;
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("data");
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    )))
}

/// 用平台原子替换语义把已刷新临时文件切换为正式目标。
#[cfg(windows)]
fn replace_target(temporary: &Path, target: &Path) -> std::io::Result<()> {
    let mut temporary_wide: Vec<u16> = temporary.as_os_str().encode_wide().collect();
    temporary_wide.push(0);
    let mut target_wide: Vec<u16> = target.as_os_str().encode_wide().collect();
    target_wide.push(0);

    // Windows 标准重命名不能覆盖现有文件；MoveFileExW 同时提供替换和写穿语义。
    let result = unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 非 Windows 构建保留相同接口，便于未来测试和明确平台边界。
#[cfg(not(windows))]
fn replace_target(temporary: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(temporary, target)
}

/// 把完整字节内容原子写入目标文件。
///
/// 副作用：创建目标父目录，并在短时间内创建同目录隐藏临时文件；成功后临时文件消失。
pub fn atomic_write(target: &Path, content: &[u8]) -> Result<(), AppError> {
    let parent = target.parent().ok_or_else(|| {
        AppError::new(
            "WRITE_FAILED",
            "目标文件没有可写父目录。",
            "重新选择有效的数据仓库后重试。",
            true,
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        AppError::new(
            "WRITE_FAILED",
            format!("无法创建数据目录：{error}"),
            "检查目录权限后重试。",
            true,
        )
    })?;

    let temporary = temporary_path(target)?;
    let result = (|| -> Result<(), AppError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                AppError::new(
                    "WRITE_FAILED",
                    format!("无法创建临时文件：{error}"),
                    "检查目录权限和磁盘空间后重试。",
                    true,
                )
            })?;
        file.write_all(content).map_err(|error| {
            AppError::new(
                "WRITE_FAILED",
                format!("无法完整写入临时文件：{error}"),
                "检查磁盘空间后重试。",
                true,
            )
        })?;
        file.sync_all().map_err(|error| {
            AppError::new(
                "WRITE_FAILED",
                format!("无法把临时文件刷新到磁盘：{error}"),
                "检查磁盘状态后重试。",
                true,
            )
        })?;
        drop(file);
        replace_target(&temporary, target).map_err(|error| {
            AppError::new(
                "WRITE_FAILED",
                format!("无法替换目标文件：{error}"),
                "关闭占用数据文件的程序后重试。",
                true,
            )
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    //! 测试职责：确认原子写入既能创建新文件，也能替换已有文件且不留下临时文件。

    use super::atomic_write;
    use std::fs;

    /// 验证连续写入后目标只保留最后一份完整内容。
    #[test]
    fn atomic_write_creates_and_replaces_complete_content() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let target = directory.path().join("commands.json");

        atomic_write(&target, b"first").expect("首次写入应成功");
        atomic_write(&target, b"second").expect("替换写入应成功");

        assert_eq!(fs::read(&target).expect("应能读取目标"), b"second");
        let remaining_files = fs::read_dir(directory.path())
            .expect("应能读取目录")
            .count();
        assert_eq!(remaining_files, 1, "成功后不应残留临时文件");
    }
}
