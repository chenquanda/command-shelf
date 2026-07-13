//! 文件职责：定义第一版命令文档、界面快照与同步状态的数据契约。
//! 主要内容：承载 `schemaVersion: 1` 的可同步数据和机器无关的前端响应模型。
//! 重要约束：数组顺序就是用户顺序；稳定 ID 不得因重新排序而变化。

use crate::error::AppError;
use serde::{Deserialize, Serialize};

/// 一条命令参数的名称与解释。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandParameter {
    /// 命令参数、选项或需要替换的占位符。
    pub name: String,
    /// 参数用途、取值规则或边界说明。
    pub description: String,
}

/// 用户长期保存的一条命令记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandEntry {
    /// 跨排序和编辑保持不变的命令 UUID。
    pub id: String,
    /// 用任务目的描述的简短标题。
    pub title: String,
    /// 可复制的完整命令正文；JSON 字段名固定为 `command`。
    #[serde(rename = "command")]
    pub command_text: String,
    /// 命令解决的问题、适用环境或前提。
    #[serde(default)]
    pub description: String,
    /// 推荐用法；为空时界面回退显示命令正文。
    #[serde(default)]
    pub usage: String,
    /// 结构化参数说明；没有额外参数时为空数组。
    #[serde(default)]
    pub parameters: Vec<CommandParameter>,
    /// 帮助用户判断结果形态的典型输出。
    pub output_example: String,
    /// 删除、覆盖或强制操作等风险提示；为空表示无额外风险。
    #[serde(default)]
    pub risk_note: String,
    /// 用户自己的补充经验或限制。
    #[serde(default)]
    pub notes: String,
}

/// 一个位置稳定的命令分类。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandCategory {
    /// 跨排序和改名保持不变的分类 UUID。
    pub id: String,
    /// 用户可见的分类名称。
    pub name: String,
    /// 分类用途说明；旧数据缺失时允许为空。
    #[serde(default)]
    pub description: String,
    /// 界面内置图标名称；未知或缺失时由前端使用终端图标。
    #[serde(default)]
    pub icon: String,
    /// 按用户手动顺序排列的命令列表。
    #[serde(default)]
    pub commands: Vec<CommandEntry>,
}

/// `commands.json` 第一版根文档。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandDocument {
    /// 数据格式版本；第一版只接受整数 `1`。
    pub schema_version: u32,
    /// 按用户手动顺序排列的分类列表。
    #[serde(default)]
    pub categories: Vec<CommandCategory>,
}

impl CommandDocument {
    /// 创建不含示例内容的第一版空文档。
    pub fn empty() -> Self {
        Self {
            schema_version: 1,
            categories: Vec::new(),
        }
    }
}

/// 侧栏同步区域使用的互斥状态。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SyncState {
    /// 当前电脑尚未连接有效数据仓库。
    Unconfigured,
    /// 工作区与当前上游基线一致。
    Synced,
    /// 数据文件未提交，或本地存在尚未推送的提交。
    Dirty,
    /// 已连接路径当前无法加载，但错误不会冒充空数据。
    Error,
}

/// 桌面后端一次性返回给前端的完整应用快照。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    /// 最近一次通过完整校验的命令文档。
    pub document: CommandDocument,
    /// 当前电脑保存的规范化仓库根路径；未配置时为空。
    pub repository_path: Option<String>,
    /// 用于侧栏展示的仓库目录名。
    pub repository_name: Option<String>,
    /// 当前同步状态。
    pub sync_state: SyncState,
    /// 对当前状态的简短中文解释。
    pub status_message: String,
    /// 当前文档字节内容的 SHA-256，用于后续防止覆盖外部修改。
    pub document_hash: Option<String>,
    /// 本次连接是否新建了空数据文件。
    pub initialized_empty_document: bool,
    /// 启动恢复遇到的结构化错误；成功或未配置时为空。
    pub error: Option<AppError>,
}

impl AppSnapshot {
    /// 创建首次启动使用的未配置快照，不携带原型示例数据。
    pub fn unconfigured() -> Self {
        Self {
            document: CommandDocument::empty(),
            repository_path: None,
            repository_name: None,
            sync_state: SyncState::Unconfigured,
            status_message: "选择已经克隆到本机的个人数据仓库。".to_string(),
            document_hash: None,
            initialized_empty_document: false,
            error: None,
        }
    }
}
