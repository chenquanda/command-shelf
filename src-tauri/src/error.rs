//! 文件职责：定义跨越 Rust 后端与 Web 前端的结构化错误契约。
//! 主要内容：保存稳定错误码、用户说明、下一步动作和重试属性。
//! 重要约束：错误消息不得包含命令正文、参考输出、凭据或完整 Git 环境变量。

use serde::Serialize;
use std::fmt::{Display, Formatter};

/// 前端可以直接呈现和判断的应用错误。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    /// 稳定机器错误码；前端逻辑只能依赖此字段，不解析自然语言消息。
    pub code: &'static str,
    /// 面向用户的中文问题说明。
    pub message: String,
    /// 用户可以立即执行的恢复建议。
    pub action: String,
    /// 修复外部条件后是否适合直接重试相同操作。
    pub retryable: bool,
}

impl AppError {
    /// 创建结构化错误，并要求调用方显式给出恢复动作以避免无行动提示。
    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        action: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            action: action.into(),
            retryable,
        }
    }
}

impl Display for AppError {
    /// 供日志和测试输出简洁诊断；用户界面仍应使用结构化字段。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}
