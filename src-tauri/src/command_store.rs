//! 文件职责：读取、校验、初始化和指纹化仓库中的 `commands.json`。
//! 主要内容：实现 `schemaVersion: 1` 业务规则，并把无效数据隔离在界面快照之外。
//! 重要约束：任何校验失败都不得自动覆盖原文件；第一版数据文件上限为 10 MB。

use crate::error::AppError;
use crate::file_io::atomic_write;
use crate::model::CommandDocument;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// 第一版允许加载的最大数据文件字节数，防止意外大文件拖垮启动。
const MAX_DOCUMENT_BYTES: u64 = 10 * 1024 * 1024;

/// 读取并完整校验命令文档，同时返回原始字节的 SHA-256。
pub fn load_document(path: &Path) -> Result<(CommandDocument, String), AppError> {
    let metadata = fs::metadata(path).map_err(|error| {
        AppError::new(
            "DATA_NOT_FOUND",
            format!("无法读取 commands.json：{error}"),
            "确认数据文件仍在仓库根目录后重试。",
            true,
        )
    })?;
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(AppError::new(
            "DATA_TOO_LARGE",
            "commands.json 超过第一版 10 MB 的加载上限。",
            "精简过长的参考输出后重新打开。",
            false,
        ));
    }

    let bytes = fs::read(path).map_err(|error| {
        AppError::new(
            "DATA_READ_FAILED",
            format!("无法读取 commands.json：{error}"),
            "检查文件权限和磁盘状态后重试。",
            true,
        )
    })?;
    parse_document_bytes(&bytes)
}

/// 从内存字节解析并完整校验第一版命令文档，供磁盘加载和远端候选预检复用。
pub fn parse_document_bytes(bytes: &[u8]) -> Result<(CommandDocument, String), AppError> {
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(AppError::new(
            "DATA_TOO_LARGE",
            "commands.json 超过第一版 10 MB 的加载上限。",
            "精简过长的参考输出后重试。",
            false,
        ));
    }
    std::str::from_utf8(bytes).map_err(|_| {
        AppError::new(
            "DATA_INVALID",
            "commands.json 不是有效 UTF-8 文本。",
            "用 UTF-8 编码修复文件后重新连接。",
            false,
        )
    })?;

    let document: CommandDocument = serde_json::from_slice(bytes).map_err(|error| {
        AppError::new(
            "DATA_INVALID",
            format!("commands.json 的 JSON 结构无效：{error}"),
            "修复文件格式后重新连接；应用不会覆盖原文件。",
            false,
        )
    })?;
    validate_document(&document)?;
    let hash = format!("{:x}", Sha256::digest(bytes));
    Ok((document, hash))
}

/// 校验第一版文档的版本、必填字段、稳定 ID 和参数完整性。
pub fn validate_document(document: &CommandDocument) -> Result<(), AppError> {
    if document.schema_version != 1 {
        return Err(AppError::new(
            "UNSUPPORTED_SCHEMA",
            format!("不支持 schemaVersion {}。", document.schema_version),
            "使用支持 schemaVersion 1 的数据文件。",
            false,
        ));
    }

    let mut category_ids = HashSet::new();
    let mut command_ids = HashSet::new();
    for category in &document.categories {
        if category.id.trim().is_empty() || category.name.trim().is_empty() {
            return Err(invalid_data("分类 ID 和名称不能为空。"));
        }
        if !category_ids.insert(category.id.as_str()) {
            return Err(invalid_data("分类 ID 必须在文档内唯一。"));
        }

        for command in &category.commands {
            if command.id.trim().is_empty()
                || command.title.trim().is_empty()
                || command.command_text.trim().is_empty()
                || command.output_example.trim().is_empty()
            {
                return Err(invalid_data("命令 ID、标题、命令正文和参考输出不能为空。"));
            }
            if !command_ids.insert(command.id.as_str()) {
                return Err(invalid_data("命令 ID 必须在整份文档内唯一。"));
            }
            if command.parameters.iter().any(|parameter| {
                parameter.name.trim().is_empty() || parameter.description.trim().is_empty()
            }) {
                return Err(invalid_data("参数名称和参数说明必须成对填写。"));
            }
        }
    }
    Ok(())
}

/// 校验并序列化命令文档，统一使用两空格缩进、LF 和结尾换行。
pub fn serialize_document(document: &CommandDocument) -> Result<Vec<u8>, AppError> {
    validate_document(document)?;
    let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| {
        AppError::new(
            "DATA_SERIALIZE_FAILED",
            format!("无法生成 commands.json：{error}"),
            "保留当前界面内容并重试保存。",
            true,
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(AppError::new(
            "DATA_TOO_LARGE",
            "保存后的 commands.json 将超过第一版 10 MB 上限。",
            "精简过长的参考输出后重试保存。",
            false,
        ));
    }
    Ok(bytes)
}

/// 在不存在数据文件时创建第一版空文档；已有文件绝不会被此函数替换。
pub fn initialize_empty_document(path: &Path) -> Result<(), AppError> {
    if path.exists() {
        return Err(AppError::new(
            "DATA_ALREADY_EXISTS",
            "commands.json 已经存在，初始化操作已停止。",
            "重新加载现有数据文件。",
            false,
        ));
    }
    let bytes = serialize_document(&CommandDocument::empty())?;
    atomic_write(path, &bytes)
}

/// 创建统一的数据校验错误，确保所有无效文档都明确说明不会被自动覆盖。
fn invalid_data(message: impl Into<String>) -> AppError {
    AppError::new(
        "DATA_INVALID",
        message,
        "修复 commands.json 后重新连接；应用不会覆盖原文件。",
        false,
    )
}

#[cfg(test)]
mod tests {
    //! 测试职责：锁定第一版文档版本、唯一 ID、必填字段和空初始化规则。

    use super::{initialize_empty_document, load_document, validate_document};
    use crate::model::{CommandCategory, CommandDocument, CommandEntry};

    /// 构造包含一条完整命令的最小合法文档。
    fn valid_document() -> CommandDocument {
        CommandDocument {
            schema_version: 1,
            categories: vec![CommandCategory {
                id: "category-1".to_string(),
                name: "Linux".to_string(),
                description: String::new(),
                icon: String::new(),
                commands: vec![CommandEntry {
                    id: "command-1".to_string(),
                    title: "查看进程".to_string(),
                    command_text: "ps aux".to_string(),
                    description: String::new(),
                    usage: String::new(),
                    parameters: Vec::new(),
                    output_example: "USER PID COMMAND".to_string(),
                    risk_note: String::new(),
                    notes: String::new(),
                }],
            }],
        }
    }

    /// 验证空文档会写入版本字段，并可被同一加载器读回。
    #[test]
    fn initializes_and_loads_empty_version_one_document() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let path = directory.path().join("commands.json");

        initialize_empty_document(&path).expect("空文档初始化应成功");
        let (document, hash) = load_document(&path).expect("初始化文档应可加载");

        assert_eq!(document, CommandDocument::empty());
        assert_eq!(hash.len(), 64, "SHA-256 应使用 64 位十六进制文本");
    }

    /// 验证未来版本不会被第一版静默读取。
    #[test]
    fn rejects_unsupported_schema_version() {
        let mut document = valid_document();
        document.schema_version = 2;
        let error = validate_document(&document).expect_err("未来版本应被拒绝");
        assert_eq!(error.code, "UNSUPPORTED_SCHEMA");
    }

    /// 验证重复命令 ID 即使位于不同分类也会被拒绝。
    #[test]
    fn rejects_duplicate_command_ids_across_document() {
        let mut document = valid_document();
        let duplicate = document.categories[0].commands[0].clone();
        document.categories.push(CommandCategory {
            id: "category-2".to_string(),
            name: "Docker".to_string(),
            description: String::new(),
            icon: String::new(),
            commands: vec![duplicate],
        });
        let error = validate_document(&document).expect_err("重复命令 ID 应被拒绝");
        assert_eq!(error.code, "DATA_INVALID");
    }

    /// 验证参考输出作为第一版必填字段不能只有空白。
    #[test]
    fn rejects_command_without_output_example() {
        let mut document = valid_document();
        document.categories[0].commands[0].output_example = "  ".to_string();
        let error = validate_document(&document).expect_err("缺少参考输出应被拒绝");
        assert_eq!(error.code, "DATA_INVALID");
    }
}
