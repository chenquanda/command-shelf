//! 文件职责：管理只属于当前电脑的 CommandShelf 配置文件。
//! 主要内容：读取和原子保存仓库路径与配置版本。
//! 重要约束：机器配置位于 `%APPDATA%\CommandShelf`，绝不能写入用户数据仓库。

use crate::error::AppError;
use crate::file_io::atomic_write;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 当前电脑使用的配置结构。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    /// 配置格式版本；第一版固定为 `1`。
    pub config_version: u32,
    /// 已验证并规范化的本地 Git 仓库根路径。
    pub repository_path: String,
}

/// 返回当前平台的默认机器配置目录。
pub fn default_config_directory() -> PathBuf {
    if let Some(app_data) = std::env::var_os("APPDATA") {
        return PathBuf::from(app_data).join("CommandShelf");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("CommandShelf");
    }
    // 该回退只用于缺少标准目录变量的开发环境；正式 Windows 会始终命中 APPDATA。
    std::env::temp_dir().join("CommandShelf-config")
}

/// 从指定机器配置目录读取配置；首次运行没有文件时返回 `None`。
pub fn load_config(directory: &Path) -> Result<Option<AppConfig>, AppError> {
    let path = directory.join("config.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|error| {
        AppError::new(
            "CONFIG_READ_FAILED",
            format!("无法读取当前电脑配置：{error}"),
            "检查配置目录权限，或移走损坏的 config.json 后重试。",
            true,
        )
    })?;
    let config: AppConfig = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::new(
            "CONFIG_INVALID",
            format!("当前电脑配置格式无效：{error}"),
            "移走 config.json 后重新选择数据仓库。",
            false,
        )
    })?;
    if config.config_version != 1 || config.repository_path.trim().is_empty() {
        return Err(AppError::new(
            "CONFIG_INVALID",
            "当前电脑配置版本或仓库路径无效。",
            "移走 config.json 后重新选择数据仓库。",
            false,
        ));
    }
    Ok(Some(config))
}

/// 把当前电脑配置原子保存到指定目录。
pub fn save_config(directory: &Path, config: &AppConfig) -> Result<(), AppError> {
    let mut bytes = serde_json::to_vec_pretty(config).map_err(|error| {
        AppError::new(
            "CONFIG_WRITE_FAILED",
            format!("无法生成当前电脑配置：{error}"),
            "重新启动应用后重试。",
            true,
        )
    })?;
    bytes.push(b'\n');
    atomic_write(&directory.join("config.json"), &bytes).map_err(|error| {
        AppError::new(
            "CONFIG_WRITE_FAILED",
            error.message,
            "检查 APPDATA 目录权限后重试。",
            true,
        )
    })
}

#[cfg(test)]
mod tests {
    //! 测试职责：验证机器配置首次缺失、保存和重启读取行为。

    use super::{load_config, save_config, AppConfig};

    /// 验证配置在新的服务实例中仍能完整恢复。
    #[test]
    fn saves_and_loads_machine_config() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        assert_eq!(load_config(directory.path()).expect("首次读取应成功"), None);

        let expected = AppConfig {
            config_version: 1,
            repository_path: "C:\\data\\command-shelf".to_string(),
        };
        save_config(directory.path(), &expected).expect("配置保存应成功");

        assert_eq!(
            load_config(directory.path()).expect("配置读取应成功"),
            Some(expected)
        );
    }
}
