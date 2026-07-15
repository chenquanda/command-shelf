//! 文件职责：对命令与临时收集文档执行不依赖 Git 工作区的三方语义合并。
//! 主要内容：按稳定 ID 对齐结构化记录，自动接入单边修改，并把真正的同字段分歧暴露为可选择项。
//! 重要约束：本模块只计算内存中的候选结果，不读写文件、不运行 Git，也不替用户决定真实冲突。

use crate::command_store::validate_document;
use crate::error::AppError;
use crate::inbox_store::validate_inbox_document;
use crate::model::{CommandCategory, CommandDocument, CommandEntry, InboxDocument, InboxEntry};
use serde::{Deserialize, Serialize};
use serde_json::{from_value, to_value, Value};
use std::collections::{HashMap, HashSet};

/// 合并界面中一条变化所属的数据实体类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MergeEntityKind {
    /// 命令分类自身的名称、说明或图标。
    Category,
    /// 分类中的一条正式命令。
    Command,
    /// 临时收集文档中的一条记录；后续 Inbox 原子复用该契约。
    Inbox,
}

/// 一条结构化变化在三方合并中的整体形态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MergeRecordKind {
    /// 记录只在一侧或两侧新出现，可以按稳定 ID 自动并入结果。
    Added,
    /// 已有记录的一个或多个字段发生变化。
    Modified,
    /// 一侧删除而另一侧修改；后续原子必须要求用户明确选择。
    DeleteVsModify,
}

/// 单个字段的自动处理或待选择状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MergeFieldStatus {
    /// 三个版本值一致，不需要出现在“只看差异”中。
    Unchanged,
    /// 只有本机修改，结果已自动采用本机值。
    AutoLocal,
    /// 只有远端修改，结果已自动采用远端值。
    AutoRemote,
    /// 两边得到了相同的新值，结果已自动采用共同值。
    AutoBoth,
    /// 两边把同一字段改成不同值，必须由用户选择或编辑最终值。
    Conflict,
}

impl MergeFieldStatus {
    /// 返回当前字段是否需要用户给出决议。
    fn requires_decision(self) -> bool {
        self == Self::Conflict
    }
}

/// 三栏界面中一个字段的本机、基线、远端与当前结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MergeFieldPreview {
    /// 在本次会话中稳定定位字段的标识，后续决议必须使用该值。
    pub resolution_id: String,
    /// JSON 字段名，供界面使用等宽字体展示。
    pub key: String,
    /// 面向用户的简短中文字段名称。
    pub label: String,
    /// 双方分叉前的共同值；新增记录没有共同值时为空。
    pub base_value: Option<Value>,
    /// 当前电脑提交中的字段值；本机删除记录时为空。
    pub local_value: Option<Value>,
    /// 固定远端提交中的字段值；远端删除记录时为空。
    pub remote_value: Option<Value>,
    /// 已自动确定的中间栏值；真实冲突在用户决议前为空。
    pub result_value: Option<Value>,
    /// 当前字段的自动处理或待选择状态。
    pub status: MergeFieldStatus,
    /// 解释自动选择原因或下一步操作的短文本。
    pub explanation: String,
}

/// 合并界面中按稳定 ID 对齐的一条分类、命令或临时记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MergeRecordPreview {
    /// 实体类型决定界面使用的标题和字段展示规则。
    pub entity_kind: MergeEntityKind,
    /// 数据文档中的稳定 ID。
    pub entity_id: String,
    /// 用户可识别的标题；命令使用命令标题，分类使用分类名称。
    pub title: String,
    /// 新增、修改或删除与修改冲突。
    pub kind: MergeRecordKind,
    /// 只包含有意义差异的字段；完整 JSON 由最终文档单独生成。
    pub fields: Vec<MergeFieldPreview>,
}

/// `commands.json` 的三方合并计划与当前自动合并结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommandMergePlan {
    /// 已写入所有无歧义变化的候选文档；冲突字段暂时保留共同基线值。
    pub merged_document: CommandDocument,
    /// 供三栏界面展示的分类和命令变化。
    pub records: Vec<MergeRecordPreview>,
    /// 已经无需用户选择即可确定的字段或新增记录数量。
    pub automatic_count: usize,
    /// 仍需用户选择或编辑的字段数量。
    pub conflict_count: usize,
}

/// `inbox.json` 的三方合并计划与当前自动合并结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InboxMergePlan {
    /// 已写入无歧义变化的候选文档；记录级冲突暂时保留共同基线记录。
    pub merged_document: InboxDocument,
    /// 供三栏界面展示的新增、修改和删除差异。
    pub records: Vec<MergeRecordPreview>,
    /// 无需用户选择即可确定的记录数量。
    pub automatic_count: usize,
    /// 仍需用户选择的记录数量。
    pub conflict_count: usize,
}

/// 用户对一个真实冲突选择的来源。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MergeDecisionChoice {
    /// 采用当前电脑对应字段或记录；本机已删除时表示保持删除。
    Local,
    /// 采用固定远端提交对应字段或记录；远端已删除时表示保持删除。
    Remote,
    /// 使用用户在中间栏编辑并通过结构校验的自定义值。
    Custom,
}

/// 一项冲突的最终选择；后端按 `resolution_id` 重新定位，不能依赖界面数组下标。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MergeDecision {
    /// 来自合并预览的稳定冲突标识。
    pub resolution_id: String,
    /// 本机、远端或自定义结果。
    pub choice: MergeDecisionChoice,
    /// 仅 `Custom` 使用；其他选择携带该字段会被忽略。
    pub custom_value: Option<Value>,
}

/// 一个字段的内部合并结果；候选值与展示信息在同一处生成，避免两套规则漂移。
struct FieldMerge<T> {
    /// 写入候选文档的值；冲突时暂用共同基线值，最终提交前不得直接使用。
    value: T,
    /// 三栏界面展示的字段详情。
    preview: MergeFieldPreview,
}

/// 校验三份文档均属于当前支持的第一版格式。
fn validate_schema_versions(
    base: &CommandDocument,
    local: &CommandDocument,
    remote: &CommandDocument,
) -> Result<(), AppError> {
    if [
        base.schema_version,
        local.schema_version,
        remote.schema_version,
    ]
    .iter()
    .all(|version| *version == 1)
    {
        return Ok(());
    }
    Err(AppError::new(
        "MERGE_DATA_INVALID",
        "用于合并的 commands.json 版本不受支持。",
        "确认两台电脑都使用当前数据格式后重新同步。",
        false,
    ))
}

/// 将可序列化字段转换为 JSON 值；模型自身无法序列化时返回稳定错误而不是生成残缺预览。
fn json_value<T: Serialize>(value: &T) -> Result<Value, AppError> {
    to_value(value).map_err(|error| {
        AppError::new(
            "MERGE_DATA_INVALID",
            format!("无法生成结构化合并预览：{error}"),
            "重新加载数据仓库后重试。",
            true,
        )
    })
}

/// 按标准三方规则合并一个字段，并同时生成三栏界面需要的来源说明。
fn merge_field<T>(
    resolution_id: String,
    key: &str,
    label: &str,
    base: &T,
    local: &T,
    remote: &T,
) -> Result<FieldMerge<T>, AppError>
where
    T: Clone + PartialEq + Serialize,
{
    let (value, result_value, status, explanation) = if local == remote {
        let status = if local == base {
            MergeFieldStatus::Unchanged
        } else {
            MergeFieldStatus::AutoBoth
        };
        (
            local.clone(),
            Some(json_value(local)?),
            status,
            if status == MergeFieldStatus::Unchanged {
                "两边内容一致。"
            } else {
                "两边修改结果相同，已自动采用。"
            },
        )
    } else if local == base {
        (
            remote.clone(),
            Some(json_value(remote)?),
            MergeFieldStatus::AutoRemote,
            "仅远端修改，已自动采用。",
        )
    } else if remote == base {
        (
            local.clone(),
            Some(json_value(local)?),
            MergeFieldStatus::AutoLocal,
            "仅本机修改，已自动采用。",
        )
    } else {
        (
            base.clone(),
            None,
            MergeFieldStatus::Conflict,
            "两边修改结果不同，请选择或编辑最终内容。",
        )
    };

    Ok(FieldMerge {
        value,
        preview: MergeFieldPreview {
            resolution_id,
            key: key.to_string(),
            label: label.to_string(),
            base_value: Some(json_value(base)?),
            local_value: Some(json_value(local)?),
            remote_value: Some(json_value(remote)?),
            result_value,
            status,
            explanation: explanation.to_string(),
        },
    })
}

/// 按共同基线累计两台电脑产生的复制增量，避免简单取较大值丢失另一端使用次数。
fn merge_copy_count(
    resolution_id: String,
    base: u64,
    local: u64,
    remote: u64,
) -> Result<FieldMerge<u64>, AppError> {
    if local < base || remote < base {
        // 手工减少次数不符合单调累计规则，退回普通三方选择，避免擅自放大或覆盖人工修改。
        return merge_field(
            resolution_id,
            "copyCount",
            "复制次数",
            &base,
            &local,
            &remote,
        );
    }
    let local_delta = local - base;
    let remote_delta = remote - base;
    let value = base
        .checked_add(local_delta)
        .and_then(|count| count.checked_add(remote_delta))
        .ok_or_else(|| {
            AppError::new(
                "MERGE_DATA_INVALID",
                "两台电脑的复制次数累计后超过允许范围。",
                "在数据文件中修正异常复制次数后重新同步。",
                false,
            )
        })?;
    let status = match (local_delta > 0, remote_delta > 0) {
        (false, false) => MergeFieldStatus::Unchanged,
        (true, false) => MergeFieldStatus::AutoLocal,
        (false, true) => MergeFieldStatus::AutoRemote,
        (true, true) => MergeFieldStatus::AutoBoth,
    };
    Ok(FieldMerge {
        value,
        preview: MergeFieldPreview {
            resolution_id,
            key: "copyCount".to_string(),
            label: "复制次数".to_string(),
            base_value: Some(json_value(&base)?),
            local_value: Some(json_value(&local)?),
            remote_value: Some(json_value(&remote)?),
            result_value: Some(json_value(&value)?),
            status,
            explanation: if status == MergeFieldStatus::Unchanged {
                "两边复制次数一致。".to_string()
            } else {
                format!("已按共同基线自动累计：{base} + {local_delta} + {remote_delta}。")
            },
        },
    })
}

/// 创建一个整记录选择项，用于删除与修改冲突或同 ID 的不同新增内容。
struct RecordSides<'a, T> {
    /// 双方分叉前的共同记录；新增冲突时为空。
    base: Option<&'a T>,
    /// 当前电脑中的记录；本机删除时为空。
    local: Option<&'a T>,
    /// 固定远端提交中的记录；远端删除时为空。
    remote: Option<&'a T>,
}

/// 创建一个整记录选择项，用于删除与修改冲突或同 ID 的不同新增内容。
fn record_conflict_preview<T: Serialize>(
    entity_kind: MergeEntityKind,
    entity_id: &str,
    title: &str,
    kind: MergeRecordKind,
    sides: RecordSides<'_, T>,
    explanation: &str,
) -> Result<MergeRecordPreview, AppError> {
    Ok(MergeRecordPreview {
        entity_kind,
        entity_id: entity_id.to_string(),
        title: title.to_string(),
        kind,
        fields: vec![MergeFieldPreview {
            resolution_id: format!(
                "{}:{entity_id}:record",
                match entity_kind {
                    MergeEntityKind::Category => "category",
                    MergeEntityKind::Command => "command",
                    MergeEntityKind::Inbox => "inbox",
                }
            ),
            key: "record".to_string(),
            label: "整条记录".to_string(),
            base_value: sides.base.map(json_value).transpose()?,
            local_value: sides.local.map(json_value).transpose()?,
            remote_value: sides.remote.map(json_value).transpose()?,
            result_value: None,
            status: MergeFieldStatus::Conflict,
            explanation: explanation.to_string(),
        }],
    })
}

/// 创建一个无需选择的整记录变化；删除结果使用 JSON `null` 明确区别于未决状态。
fn automatic_record_preview<T: Serialize>(
    entity_kind: MergeEntityKind,
    entity_id: &str,
    title: &str,
    sides: RecordSides<'_, T>,
    result: Option<&T>,
    status: MergeFieldStatus,
    explanation: &str,
) -> Result<MergeRecordPreview, AppError> {
    Ok(MergeRecordPreview {
        entity_kind,
        entity_id: entity_id.to_string(),
        title: title.to_string(),
        kind: MergeRecordKind::Modified,
        fields: vec![MergeFieldPreview {
            resolution_id: format!(
                "{}:{entity_id}:record",
                match entity_kind {
                    MergeEntityKind::Category => "category",
                    MergeEntityKind::Command => "command",
                    MergeEntityKind::Inbox => "inbox",
                }
            ),
            key: "record".to_string(),
            label: "整条记录".to_string(),
            base_value: sides.base.map(json_value).transpose()?,
            local_value: sides.local.map(json_value).transpose()?,
            remote_value: sides.remote.map(json_value).transpose()?,
            result_value: Some(result.map(json_value).transpose()?.unwrap_or(Value::Null)),
            status,
            explanation: explanation.to_string(),
        }],
    })
}

/// 为单侧新增记录生成一个整记录预览；中间栏默认完整保留新增内容。
fn added_record_preview<T: Serialize>(
    entity_kind: MergeEntityKind,
    entity_id: &str,
    title: &str,
    source: MergeFieldStatus,
    value: &T,
) -> Result<MergeRecordPreview, AppError> {
    let (local_value, remote_value, explanation) = match source {
        MergeFieldStatus::AutoLocal => (
            Some(json_value(value)?),
            None,
            "本机新增，已自动加入合并结果。",
        ),
        MergeFieldStatus::AutoRemote => (
            None,
            Some(json_value(value)?),
            "远端新增，已自动加入合并结果。",
        ),
        _ => (
            Some(json_value(value)?),
            Some(json_value(value)?),
            "两边新增内容一致，已自动保留一份。",
        ),
    };
    Ok(MergeRecordPreview {
        entity_kind,
        entity_id: entity_id.to_string(),
        title: title.to_string(),
        kind: MergeRecordKind::Added,
        fields: vec![MergeFieldPreview {
            resolution_id: format!("{entity_id}:record"),
            key: "record".to_string(),
            label: "新增内容".to_string(),
            base_value: None,
            local_value,
            remote_value,
            result_value: Some(json_value(value)?),
            status: source,
            explanation: explanation.to_string(),
        }],
    })
}

/// 合并一条双方都保留的已有命令，并只展示发生变化的字段。
fn merge_existing_command(
    base: &CommandEntry,
    local: &CommandEntry,
    remote: &CommandEntry,
) -> Result<(CommandEntry, Option<MergeRecordPreview>), AppError> {
    let mut fields = Vec::new();
    macro_rules! merge_command_field {
        ($field:ident, $key:literal, $label:literal) => {{
            let merged = merge_field(
                format!("command:{}:{}", base.id, $key),
                $key,
                $label,
                &base.$field,
                &local.$field,
                &remote.$field,
            )?;
            if merged.preview.status != MergeFieldStatus::Unchanged {
                fields.push(merged.preview);
            }
            merged.value
        }};
    }

    let copy_count = merge_copy_count(
        format!("command:{}:copyCount", base.id),
        base.copy_count,
        local.copy_count,
        remote.copy_count,
    )?;
    if copy_count.preview.status != MergeFieldStatus::Unchanged {
        fields.push(copy_count.preview);
    }
    let merged = CommandEntry {
        id: base.id.clone(),
        title: merge_command_field!(title, "title", "标题"),
        command_text: merge_command_field!(command_text, "command", "命令正文"),
        description: merge_command_field!(description, "description", "命令说明"),
        usage: merge_command_field!(usage, "usage", "用法"),
        parameters: merge_command_field!(parameters, "parameters", "参数说明"),
        output_example: merge_command_field!(output_example, "outputExample", "参考输出"),
        risk_note: merge_command_field!(risk_note, "riskNote", "风险提示"),
        notes: merge_command_field!(notes, "notes", "个人备注"),
        copy_count: copy_count.value,
    };
    let preview = (!fields.is_empty()).then(|| MergeRecordPreview {
        entity_kind: MergeEntityKind::Command,
        entity_id: base.id.clone(),
        title: merged.title.clone(),
        kind: MergeRecordKind::Modified,
        fields,
    });
    Ok((merged, preview))
}

/// 合并一个双方都保留的已有分类及其命令列表。
fn merge_existing_category(
    base: &CommandCategory,
    local: &CommandCategory,
    remote: &CommandCategory,
    records: &mut Vec<MergeRecordPreview>,
) -> Result<CommandCategory, AppError> {
    let mut category_fields = Vec::new();
    macro_rules! merge_category_field {
        ($field:ident, $key:literal, $label:literal) => {{
            let merged = merge_field(
                format!("category:{}:{}", base.id, $key),
                $key,
                $label,
                &base.$field,
                &local.$field,
                &remote.$field,
            )?;
            if merged.preview.status != MergeFieldStatus::Unchanged {
                category_fields.push(merged.preview);
            }
            merged.value
        }};
    }

    let name = merge_category_field!(name, "name", "分类名称");
    let description = merge_category_field!(description, "description", "分类说明");
    let icon = merge_category_field!(icon, "icon", "分类图标");
    if !category_fields.is_empty() {
        records.push(MergeRecordPreview {
            entity_kind: MergeEntityKind::Category,
            entity_id: base.id.clone(),
            title: name.clone(),
            kind: MergeRecordKind::Modified,
            fields: category_fields,
        });
    }

    let base_by_id: HashMap<&str, &CommandEntry> = base
        .commands
        .iter()
        .map(|command| (command.id.as_str(), command))
        .collect();
    let local_by_id: HashMap<&str, &CommandEntry> = local
        .commands
        .iter()
        .map(|command| (command.id.as_str(), command))
        .collect();
    let remote_by_id: HashMap<&str, &CommandEntry> = remote
        .commands
        .iter()
        .map(|command| (command.id.as_str(), command))
        .collect();
    let mut ordered_ids: Vec<&str> = local
        .commands
        .iter()
        .map(|command| command.id.as_str())
        .collect();
    let mut seen: HashSet<&str> = ordered_ids.iter().copied().collect();
    for command in &remote.commands {
        if seen.insert(command.id.as_str()) {
            ordered_ids.push(command.id.as_str());
        }
    }

    let mut commands = Vec::with_capacity(ordered_ids.len());
    for command_id in ordered_ids {
        match (
            base_by_id.get(command_id).copied(),
            local_by_id.get(command_id).copied(),
            remote_by_id.get(command_id).copied(),
        ) {
            (Some(base_command), Some(local_command), Some(remote_command)) => {
                let (merged, preview) =
                    merge_existing_command(base_command, local_command, remote_command)?;
                commands.push(merged);
                if let Some(preview) = preview {
                    records.push(preview);
                }
            }
            (None, Some(local_command), None) => {
                commands.push(local_command.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Command,
                    &local_command.id,
                    &local_command.title,
                    MergeFieldStatus::AutoLocal,
                    local_command,
                )?);
            }
            (None, None, Some(remote_command)) => {
                commands.push(remote_command.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Command,
                    &remote_command.id,
                    &remote_command.title,
                    MergeFieldStatus::AutoRemote,
                    remote_command,
                )?);
            }
            (None, Some(local_command), Some(remote_command))
                if local_command == remote_command =>
            {
                commands.push(local_command.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Command,
                    &local_command.id,
                    &local_command.title,
                    MergeFieldStatus::AutoBoth,
                    local_command,
                )?);
            }
            (None, Some(local_command), Some(remote_command)) => {
                commands.push(local_command.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Command,
                    command_id,
                    &local_command.title,
                    MergeRecordKind::Added,
                    RecordSides {
                        base: None::<&CommandEntry>,
                        local: Some(local_command),
                        remote: Some(remote_command),
                    },
                    "两边新增了相同 ID 但内容不同的命令，请选择最终版本。",
                )?);
            }
            (Some(base_command), None, Some(remote_command)) if remote_command == base_command => {
                records.push(automatic_record_preview(
                    MergeEntityKind::Command,
                    command_id,
                    &base_command.title,
                    RecordSides {
                        base: Some(base_command),
                        local: None,
                        remote: Some(remote_command),
                    },
                    None,
                    MergeFieldStatus::AutoLocal,
                    "仅本机删除，已自动从合并结果移除。",
                )?);
            }
            (Some(base_command), Some(local_command), None) if local_command == base_command => {
                records.push(automatic_record_preview(
                    MergeEntityKind::Command,
                    command_id,
                    &base_command.title,
                    RecordSides {
                        base: Some(base_command),
                        local: Some(local_command),
                        remote: None,
                    },
                    None,
                    MergeFieldStatus::AutoRemote,
                    "仅远端删除，已自动从合并结果移除。",
                )?);
            }
            (Some(base_command), None, Some(remote_command)) => {
                commands.push(base_command.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Command,
                    command_id,
                    &remote_command.title,
                    MergeRecordKind::DeleteVsModify,
                    RecordSides {
                        base: Some(base_command),
                        local: None,
                        remote: Some(remote_command),
                    },
                    "本机删除了此命令，远端修改了内容，请选择保持删除或保留远端版本。",
                )?);
            }
            (Some(base_command), Some(local_command), None) => {
                commands.push(base_command.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Command,
                    command_id,
                    &local_command.title,
                    MergeRecordKind::DeleteVsModify,
                    RecordSides {
                        base: Some(base_command),
                        local: Some(local_command),
                        remote: None,
                    },
                    "远端删除了此命令，本机修改了内容，请选择保留本机版本或保持删除。",
                )?);
            }
            (Some(_), None, None) => unreachable!("双方均删除的命令不会出现在合并顺序中"),
            (None, None, None) => unreachable!("不存在于任一当前版本的命令不会进入合并顺序"),
        }
    }

    Ok(CommandCategory {
        id: base.id.clone(),
        name,
        description,
        icon,
        commands,
    })
}

/// 对三份命令文档执行纯内存三方合并。
///
/// 参数：`base` 是双方分叉前共同提交，`local` 是本机提交，`remote` 是固定远端提交。
/// 返回值：包含自动合并候选文档和三栏预览；`conflict_count > 0` 时不得直接写盘。
/// 副作用：无。本函数不会修改输入、文件系统或 Git 仓库。
pub fn merge_command_documents(
    base: &CommandDocument,
    local: &CommandDocument,
    remote: &CommandDocument,
) -> Result<CommandMergePlan, AppError> {
    validate_schema_versions(base, local, remote)?;
    let base_by_id: HashMap<&str, &CommandCategory> = base
        .categories
        .iter()
        .map(|category| (category.id.as_str(), category))
        .collect();
    let local_by_id: HashMap<&str, &CommandCategory> = local
        .categories
        .iter()
        .map(|category| (category.id.as_str(), category))
        .collect();
    let remote_by_id: HashMap<&str, &CommandCategory> = remote
        .categories
        .iter()
        .map(|category| (category.id.as_str(), category))
        .collect();
    // 本机顺序是个人使用场景下最可预测的结果；远端独有分类按远端顺序追加。
    let mut ordered_ids: Vec<&str> = local
        .categories
        .iter()
        .map(|category| category.id.as_str())
        .collect();
    let mut seen: HashSet<&str> = ordered_ids.iter().copied().collect();
    for category in &remote.categories {
        if seen.insert(category.id.as_str()) {
            ordered_ids.push(category.id.as_str());
        }
    }

    let mut categories = Vec::with_capacity(ordered_ids.len());
    let mut records = Vec::new();
    for category_id in ordered_ids {
        match (
            base_by_id.get(category_id).copied(),
            local_by_id.get(category_id).copied(),
            remote_by_id.get(category_id).copied(),
        ) {
            (Some(base_category), Some(local_category), Some(remote_category)) => {
                categories.push(merge_existing_category(
                    base_category,
                    local_category,
                    remote_category,
                    &mut records,
                )?);
            }
            (None, Some(local_category), None) => {
                categories.push(local_category.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Category,
                    &local_category.id,
                    &local_category.name,
                    MergeFieldStatus::AutoLocal,
                    local_category,
                )?);
            }
            (None, None, Some(remote_category)) => {
                categories.push(remote_category.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Category,
                    &remote_category.id,
                    &remote_category.name,
                    MergeFieldStatus::AutoRemote,
                    remote_category,
                )?);
            }
            (None, Some(local_category), Some(remote_category))
                if local_category == remote_category =>
            {
                categories.push(local_category.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Category,
                    &local_category.id,
                    &local_category.name,
                    MergeFieldStatus::AutoBoth,
                    local_category,
                )?);
            }
            (None, Some(local_category), Some(remote_category)) => {
                categories.push(local_category.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Category,
                    category_id,
                    &local_category.name,
                    MergeRecordKind::Added,
                    RecordSides {
                        base: None::<&CommandCategory>,
                        local: Some(local_category),
                        remote: Some(remote_category),
                    },
                    "两边新增了相同 ID 但内容不同的分类，请选择最终版本。",
                )?);
            }
            (Some(base_category), None, Some(remote_category))
                if remote_category == base_category =>
            {
                records.push(automatic_record_preview(
                    MergeEntityKind::Category,
                    category_id,
                    &base_category.name,
                    RecordSides {
                        base: Some(base_category),
                        local: None,
                        remote: Some(remote_category),
                    },
                    None,
                    MergeFieldStatus::AutoLocal,
                    "仅本机删除分类，已自动从合并结果移除。",
                )?);
            }
            (Some(base_category), Some(local_category), None)
                if local_category == base_category =>
            {
                records.push(automatic_record_preview(
                    MergeEntityKind::Category,
                    category_id,
                    &base_category.name,
                    RecordSides {
                        base: Some(base_category),
                        local: Some(local_category),
                        remote: None,
                    },
                    None,
                    MergeFieldStatus::AutoRemote,
                    "仅远端删除分类，已自动从合并结果移除。",
                )?);
            }
            (Some(base_category), None, Some(remote_category)) => {
                categories.push(base_category.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Category,
                    category_id,
                    &remote_category.name,
                    MergeRecordKind::DeleteVsModify,
                    RecordSides {
                        base: Some(base_category),
                        local: None,
                        remote: Some(remote_category),
                    },
                    "本机删除了此分类，远端修改了内容，请选择最终结果。",
                )?);
            }
            (Some(base_category), Some(local_category), None) => {
                categories.push(base_category.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Category,
                    category_id,
                    &local_category.name,
                    MergeRecordKind::DeleteVsModify,
                    RecordSides {
                        base: Some(base_category),
                        local: Some(local_category),
                        remote: None,
                    },
                    "远端删除了此分类，本机修改了内容，请选择最终结果。",
                )?);
            }
            (Some(_), None, None) => unreachable!("双方均删除的分类不会出现在合并顺序中"),
            (None, None, None) => unreachable!("不存在于任一当前版本的分类不会进入合并顺序"),
        }
    }

    let automatic_count = records
        .iter()
        .flat_map(|record| &record.fields)
        .filter(|field| !field.status.requires_decision())
        .count();
    let conflict_count = records
        .iter()
        .flat_map(|record| &record.fields)
        .filter(|field| field.status.requires_decision())
        .count();
    Ok(CommandMergePlan {
        merged_document: CommandDocument {
            schema_version: 1,
            categories,
        },
        records,
        automatic_count,
        conflict_count,
    })
}

/// 把 JSON 值还原为指定模型字段，并将类型错误转换为稳定的合并错误。
fn typed_value<T>(value: Value, label: &str) -> Result<T, AppError>
where
    T: for<'de> Deserialize<'de>,
{
    from_value(value).map_err(|error| {
        AppError::new(
            "MERGE_DECISION_INVALID",
            format!("{label}的合并结果格式无效：{error}"),
            "重新选择本机或远端内容，或修正中间栏的自定义值。",
            true,
        )
    })
}

/// 根据用户选择取得一个字段或整记录的最终 JSON 值；`None` 只表示选择了已删除的一侧。
fn selected_value(
    field: &MergeFieldPreview,
    decision: &MergeDecision,
) -> Result<Option<Value>, AppError> {
    let value = match decision.choice {
        MergeDecisionChoice::Local => field.local_value.clone(),
        MergeDecisionChoice::Remote => field.remote_value.clone(),
        MergeDecisionChoice::Custom => Some(decision.custom_value.clone().ok_or_else(|| {
            AppError::new(
                "MERGE_DECISION_INVALID",
                "自定义合并结果不能为空。",
                "在中间栏填写最终内容，或改为采用本机/远端版本。",
                true,
            )
        })?),
    };
    Ok(value)
}

/// 建立决议索引并拒绝重复选择，避免同一冲突的最终值依赖请求数组顺序。
fn decision_index(decisions: &[MergeDecision]) -> Result<HashMap<&str, &MergeDecision>, AppError> {
    let mut indexed = HashMap::new();
    for decision in decisions {
        if indexed
            .insert(decision.resolution_id.as_str(), decision)
            .is_some()
        {
            return Err(AppError::new(
                "MERGE_DECISION_INVALID",
                format!("冲突 {} 被重复选择。", decision.resolution_id),
                "重新打开冲突窗口后再完成合并。",
                true,
            ));
        }
    }
    Ok(indexed)
}

/// 在候选命令文档中替换或删除一个整分类。
fn apply_category_record(
    document: &mut CommandDocument,
    category_id: &str,
    value: Option<Value>,
) -> Result<(), AppError> {
    if let Some(value) = value {
        let category: CommandCategory = typed_value(value, "分类")?;
        if category.id != category_id {
            return Err(AppError::new(
                "MERGE_DECISION_INVALID",
                "分类合并结果改变了稳定 ID。",
                "保留原分类 ID 后重试。",
                true,
            ));
        }
        let existing = document
            .categories
            .iter_mut()
            .find(|candidate| candidate.id == category_id)
            .ok_or_else(|| {
                AppError::new(
                    "MERGE_DECISION_INVALID",
                    "合并候选中找不到需要替换的分类。",
                    "重新启动同步并生成新的冲突预览。",
                    true,
                )
            })?;
        *existing = category;
    } else {
        document
            .categories
            .retain(|category| category.id != category_id);
    }
    Ok(())
}

/// 在候选命令文档中替换或删除一条整命令。
fn apply_command_record(
    document: &mut CommandDocument,
    command_id: &str,
    value: Option<Value>,
) -> Result<(), AppError> {
    if let Some(value) = value {
        let command: CommandEntry = typed_value(value, "命令")?;
        if command.id != command_id {
            return Err(AppError::new(
                "MERGE_DECISION_INVALID",
                "命令合并结果改变了稳定 ID。",
                "保留原命令 ID 后重试。",
                true,
            ));
        }
        let existing = document
            .categories
            .iter_mut()
            .flat_map(|category| category.commands.iter_mut())
            .find(|candidate| candidate.id == command_id)
            .ok_or_else(|| {
                AppError::new(
                    "MERGE_DECISION_INVALID",
                    "合并候选中找不到需要替换的命令。",
                    "重新启动同步并生成新的冲突预览。",
                    true,
                )
            })?;
        *existing = command;
    } else {
        for category in &mut document.categories {
            category.commands.retain(|command| command.id != command_id);
        }
    }
    Ok(())
}

/// 把一个字段级选择写入候选命令文档；字段集合与 `CommandEntry` 契约保持一一对应。
fn apply_command_field(
    document: &mut CommandDocument,
    entity_kind: MergeEntityKind,
    entity_id: &str,
    key: &str,
    value: Value,
) -> Result<(), AppError> {
    match entity_kind {
        MergeEntityKind::Category => {
            let category = document
                .categories
                .iter_mut()
                .find(|candidate| candidate.id == entity_id)
                .ok_or_else(|| {
                    AppError::new(
                        "MERGE_DECISION_INVALID",
                        "合并候选中找不到需要修改的分类。",
                        "重新启动同步并生成新的冲突预览。",
                        true,
                    )
                })?;
            match key {
                "name" => category.name = typed_value(value, "分类名称")?,
                "description" => category.description = typed_value(value, "分类说明")?,
                "icon" => category.icon = typed_value(value, "分类图标")?,
                _ => {
                    return Err(AppError::new(
                        "MERGE_DECISION_INVALID",
                        format!("无法识别分类字段 {key}。"),
                        "重新打开冲突窗口后再试。",
                        true,
                    ));
                }
            }
        }
        MergeEntityKind::Command => {
            let command = document
                .categories
                .iter_mut()
                .flat_map(|category| category.commands.iter_mut())
                .find(|candidate| candidate.id == entity_id)
                .ok_or_else(|| {
                    AppError::new(
                        "MERGE_DECISION_INVALID",
                        "合并候选中找不到需要修改的命令。",
                        "重新启动同步并生成新的冲突预览。",
                        true,
                    )
                })?;
            match key {
                "title" => command.title = typed_value(value, "标题")?,
                "command" => command.command_text = typed_value(value, "命令正文")?,
                "description" => command.description = typed_value(value, "命令说明")?,
                "usage" => command.usage = typed_value(value, "用法")?,
                "parameters" => command.parameters = typed_value(value, "参数说明")?,
                "outputExample" => command.output_example = typed_value(value, "参考输出")?,
                "riskNote" => command.risk_note = typed_value(value, "风险提示")?,
                "notes" => command.notes = typed_value(value, "个人备注")?,
                "copyCount" => command.copy_count = typed_value(value, "复制次数")?,
                _ => {
                    return Err(AppError::new(
                        "MERGE_DECISION_INVALID",
                        format!("无法识别命令字段 {key}。"),
                        "重新打开冲突窗口后再试。",
                        true,
                    ));
                }
            }
        }
        MergeEntityKind::Inbox => {
            return Err(AppError::new(
                "MERGE_DECISION_INVALID",
                "临时记录不能写入 commands.json。",
                "重新打开冲突窗口后再试。",
                true,
            ));
        }
    }
    Ok(())
}

/// 将全部必需决议应用到命令候选文档，并在返回前执行完整数据校验。
///
/// 未决项缺少选择、携带未知选择或自定义值类型错误时，本函数拒绝生成最终文件。
pub fn apply_command_decisions(
    mut plan: CommandMergePlan,
    decisions: &[MergeDecision],
) -> Result<CommandDocument, AppError> {
    let indexed = decision_index(decisions)?;
    let expected: HashSet<&str> = plan
        .records
        .iter()
        .flat_map(|record| record.fields.iter())
        .filter(|field| field.status.requires_decision())
        .map(|field| field.resolution_id.as_str())
        .collect();
    if let Some(unknown) = indexed.keys().find(|id| !expected.contains(**id)) {
        return Err(AppError::new(
            "MERGE_DECISION_INVALID",
            format!("合并请求包含未知冲突 {unknown}。"),
            "重新打开冲突窗口后再试。",
            true,
        ));
    }

    for record in &plan.records {
        for field in record
            .fields
            .iter()
            .filter(|field| field.status.requires_decision())
        {
            let decision = indexed.get(field.resolution_id.as_str()).ok_or_else(|| {
                AppError::new(
                    "MERGE_DECISION_REQUIRED",
                    format!("{}仍未选择最终内容。", record.title),
                    "处理所有黄色待选择项后再完成合并。",
                    true,
                )
            })?;
            let value = selected_value(field, decision)?;
            if field.key == "record" {
                match record.entity_kind {
                    MergeEntityKind::Category => {
                        apply_category_record(&mut plan.merged_document, &record.entity_id, value)?
                    }
                    MergeEntityKind::Command => {
                        apply_command_record(&mut plan.merged_document, &record.entity_id, value)?
                    }
                    MergeEntityKind::Inbox => unreachable!("命令计划不包含临时记录"),
                }
            } else {
                apply_command_field(
                    &mut plan.merged_document,
                    record.entity_kind,
                    &record.entity_id,
                    &field.key,
                    value.ok_or_else(|| {
                        AppError::new(
                            "MERGE_DECISION_INVALID",
                            format!("字段 {} 不能选择删除结果。", field.label),
                            "改为采用存在的字段值，或填写自定义内容。",
                            true,
                        )
                    })?,
                )?;
            }
        }
    }
    validate_document(&plan.merged_document)?;
    Ok(plan.merged_document)
}

/// 对三份临时收集文档执行按记录 ID 的三方合并。
///
/// 同一条记录同时被不同修改时按整记录选择，避免把 `content` 与对应 `updatedAt` 拆成不一致组合。
pub fn merge_inbox_documents(
    base: &InboxDocument,
    local: &InboxDocument,
    remote: &InboxDocument,
) -> Result<InboxMergePlan, AppError> {
    if [
        base.schema_version,
        local.schema_version,
        remote.schema_version,
    ]
    .iter()
    .any(|version| *version != 1)
    {
        return Err(AppError::new(
            "MERGE_DATA_INVALID",
            "用于合并的 inbox.json 版本不受支持。",
            "确认两台电脑都使用当前数据格式后重新同步。",
            false,
        ));
    }
    let base_by_id: HashMap<&str, &InboxEntry> = base
        .items
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect();
    let local_by_id: HashMap<&str, &InboxEntry> = local
        .items
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect();
    let remote_by_id: HashMap<&str, &InboxEntry> = remote
        .items
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect();
    let mut ordered_ids: Vec<&str> = local.items.iter().map(|item| item.id.as_str()).collect();
    let mut seen: HashSet<&str> = ordered_ids.iter().copied().collect();
    for item in &remote.items {
        if seen.insert(item.id.as_str()) {
            ordered_ids.push(item.id.as_str());
        }
    }

    let mut items = Vec::with_capacity(ordered_ids.len());
    let mut records = Vec::new();
    for item_id in ordered_ids {
        let triple = (
            base_by_id.get(item_id).copied(),
            local_by_id.get(item_id).copied(),
            remote_by_id.get(item_id).copied(),
        );
        match triple {
            (Some(_), Some(local_item), Some(remote_item)) if local_item == remote_item => {
                items.push(local_item.clone());
            }
            (Some(base_item), Some(local_item), Some(remote_item)) if local_item == base_item => {
                items.push(remote_item.clone());
                records.push(automatic_record_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "临时记录",
                    RecordSides {
                        base: Some(base_item),
                        local: Some(local_item),
                        remote: Some(remote_item),
                    },
                    Some(remote_item),
                    MergeFieldStatus::AutoRemote,
                    "仅远端修改，已自动采用。",
                )?);
            }
            (Some(base_item), Some(local_item), Some(remote_item)) if remote_item == base_item => {
                items.push(local_item.clone());
                records.push(automatic_record_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "临时记录",
                    RecordSides {
                        base: Some(base_item),
                        local: Some(local_item),
                        remote: Some(remote_item),
                    },
                    Some(local_item),
                    MergeFieldStatus::AutoLocal,
                    "仅本机修改，已自动采用。",
                )?);
            }
            (Some(base_item), Some(local_item), Some(remote_item)) => {
                items.push(base_item.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "临时记录",
                    MergeRecordKind::Modified,
                    RecordSides {
                        base: Some(base_item),
                        local: Some(local_item),
                        remote: Some(remote_item),
                    },
                    "两边修改了同一条临时记录，请选择最终版本。",
                )?);
            }
            (None, Some(local_item), None) => {
                items.push(local_item.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "本机新增记录",
                    MergeFieldStatus::AutoLocal,
                    local_item,
                )?);
            }
            (None, None, Some(remote_item)) => {
                items.push(remote_item.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "远端新增记录",
                    MergeFieldStatus::AutoRemote,
                    remote_item,
                )?);
            }
            (None, Some(local_item), Some(remote_item)) if local_item == remote_item => {
                items.push(local_item.clone());
                records.push(added_record_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "双方新增记录",
                    MergeFieldStatus::AutoBoth,
                    local_item,
                )?);
            }
            (None, Some(local_item), Some(remote_item)) => {
                items.push(local_item.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "临时记录",
                    MergeRecordKind::Added,
                    RecordSides {
                        base: None::<&InboxEntry>,
                        local: Some(local_item),
                        remote: Some(remote_item),
                    },
                    "两边新增了相同 ID 但内容不同的记录，请选择最终版本。",
                )?);
            }
            (Some(base_item), None, Some(remote_item)) if remote_item == base_item => {
                records.push(automatic_record_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "临时记录",
                    RecordSides {
                        base: Some(base_item),
                        local: None,
                        remote: Some(remote_item),
                    },
                    None,
                    MergeFieldStatus::AutoLocal,
                    "仅本机删除，已自动移除。",
                )?);
            }
            (Some(base_item), Some(local_item), None) if local_item == base_item => {
                records.push(automatic_record_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "临时记录",
                    RecordSides {
                        base: Some(base_item),
                        local: Some(local_item),
                        remote: None,
                    },
                    None,
                    MergeFieldStatus::AutoRemote,
                    "仅远端删除，已自动移除。",
                )?);
            }
            (Some(base_item), None, Some(remote_item)) => {
                items.push(base_item.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "临时记录",
                    MergeRecordKind::DeleteVsModify,
                    RecordSides {
                        base: Some(base_item),
                        local: None,
                        remote: Some(remote_item),
                    },
                    "本机删除、远端修改了同一条记录，请选择最终结果。",
                )?);
            }
            (Some(base_item), Some(local_item), None) => {
                items.push(base_item.clone());
                records.push(record_conflict_preview(
                    MergeEntityKind::Inbox,
                    item_id,
                    "临时记录",
                    MergeRecordKind::DeleteVsModify,
                    RecordSides {
                        base: Some(base_item),
                        local: Some(local_item),
                        remote: None,
                    },
                    "远端删除、本机修改了同一条记录，请选择最终结果。",
                )?);
            }
            (Some(_), None, None) => {}
            (None, None, None) => unreachable!("不存在于任一当前版本的记录不会进入合并顺序"),
        }
    }
    // 临时收集本身就是时间流；双方新增后按 ISO 8601 创建时间重新形成稳定的新到旧顺序。
    items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    let automatic_count = records
        .iter()
        .flat_map(|record| &record.fields)
        .filter(|field| !field.status.requires_decision())
        .count();
    let conflict_count = records
        .iter()
        .flat_map(|record| &record.fields)
        .filter(|field| field.status.requires_decision())
        .count();
    Ok(InboxMergePlan {
        merged_document: InboxDocument {
            schema_version: 1,
            items,
        },
        records,
        automatic_count,
        conflict_count,
    })
}

/// 将临时收集的记录级决议应用到候选文档，并恢复时间流顺序。
pub fn apply_inbox_decisions(
    mut plan: InboxMergePlan,
    decisions: &[MergeDecision],
) -> Result<InboxDocument, AppError> {
    let indexed = decision_index(decisions)?;
    let expected: HashSet<&str> = plan
        .records
        .iter()
        .flat_map(|record| &record.fields)
        .filter(|field| field.status.requires_decision())
        .map(|field| field.resolution_id.as_str())
        .collect();
    if let Some(unknown) = indexed.keys().find(|id| !expected.contains(**id)) {
        return Err(AppError::new(
            "MERGE_DECISION_INVALID",
            format!("合并请求包含未知冲突 {unknown}。"),
            "重新打开冲突窗口后再试。",
            true,
        ));
    }
    for record in &plan.records {
        let Some(field) = record
            .fields
            .iter()
            .find(|field| field.status.requires_decision())
        else {
            continue;
        };
        let decision = indexed.get(field.resolution_id.as_str()).ok_or_else(|| {
            AppError::new(
                "MERGE_DECISION_REQUIRED",
                "仍有临时记录未选择最终内容。",
                "处理所有黄色待选择项后再完成合并。",
                true,
            )
        })?;
        let value = selected_value(field, decision)?;
        plan.merged_document
            .items
            .retain(|item| item.id != record.entity_id);
        if let Some(value) = value {
            let item: InboxEntry = typed_value(value, "临时记录")?;
            if item.id != record.entity_id {
                return Err(AppError::new(
                    "MERGE_DECISION_INVALID",
                    "临时记录合并结果改变了稳定 ID。",
                    "保留原记录 ID 后重试。",
                    true,
                ));
            }
            plan.merged_document.items.push(item);
        }
    }
    plan.merged_document
        .items
        .sort_by(|left, right| right.created_at.cmp(&left.created_at));
    validate_inbox_document(&plan.merged_document)?;
    Ok(plan.merged_document)
}

#[cfg(test)]
mod tests {
    //! 测试职责：锁定命令文档按 ID 对齐、双方新增和字段级三方选择的纯函数规则。

    use super::{
        apply_command_decisions, apply_inbox_decisions, merge_command_documents,
        merge_inbox_documents, MergeDecision, MergeDecisionChoice, MergeFieldStatus,
        MergeRecordKind,
    };
    use crate::model::{CommandCategory, CommandDocument, CommandEntry, InboxDocument, InboxEntry};

    /// 构造字段完整的命令，避免测试样例因默认值掩盖字段合并。
    fn command(id: &str, title: &str) -> CommandEntry {
        CommandEntry {
            id: id.to_string(),
            title: title.to_string(),
            command_text: format!("echo {id}"),
            description: "共同说明".to_string(),
            usage: format!("echo {id}"),
            parameters: Vec::new(),
            output_example: id.to_string(),
            risk_note: String::new(),
            notes: String::new(),
            copy_count: 0,
        }
    }

    /// 构造只有一个分类的命令文档，便于聚焦命令数组合并。
    fn document(commands: Vec<CommandEntry>) -> CommandDocument {
        CommandDocument {
            schema_version: 1,
            categories: vec![CommandCategory {
                id: "category-linux".to_string(),
                name: "Linux".to_string(),
                description: "Linux 命令".to_string(),
                icon: "terminal".to_string(),
                commands,
            }],
        }
    }

    /// 构造时间字段有效的临时记录。
    fn inbox_item(id: &str, content: &str, created_at: &str) -> InboxEntry {
        InboxEntry {
            id: id.to_string(),
            content: content.to_string(),
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
        }
    }

    /// 构造第一版临时收集文档。
    fn inbox(items: Vec<InboxEntry>) -> InboxDocument {
        InboxDocument {
            schema_version: 1,
            items,
        }
    }

    /// 验证两台电脑分别新增命令时全部进入中间结果，并保持本机新增在前、远端独有在后。
    #[test]
    fn combines_independent_additions_from_both_computers() {
        let base = document(vec![command("base", "共同命令")]);
        let local = document(vec![
            command("local-a", "本机新增 A"),
            command("base", "共同命令"),
        ]);
        let remote = document(vec![
            command("remote-b", "远端新增 B"),
            command("base", "共同命令"),
        ]);

        let plan = merge_command_documents(&base, &local, &remote).expect("独立新增应自动合并");

        let ids: Vec<&str> = plan.merged_document.categories[0]
            .commands
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(ids, vec!["local-a", "base", "remote-b"]);
        assert_eq!(plan.conflict_count, 0);
        assert_eq!(plan.automatic_count, 2);
        assert!(plan
            .records
            .iter()
            .all(|record| record.kind == MergeRecordKind::Added));
    }

    /// 验证双方修改不同字段时结果会组合两边变化，不制造无意义冲突。
    #[test]
    fn combines_changes_to_different_fields() {
        let base_command = command("logs", "查看日志");
        let mut local_command = base_command.clone();
        local_command.description = "查看最近 100 行日志".to_string();
        let mut remote_command = base_command.clone();
        remote_command.output_example = "最近日志内容".to_string();

        let plan = merge_command_documents(
            &document(vec![base_command]),
            &document(vec![local_command]),
            &document(vec![remote_command]),
        )
        .expect("不同字段修改应自动组合");

        let merged = &plan.merged_document.categories[0].commands[0];
        assert_eq!(merged.description, "查看最近 100 行日志");
        assert_eq!(merged.output_example, "最近日志内容");
        assert_eq!(plan.conflict_count, 0);
        assert_eq!(plan.automatic_count, 2);
    }

    /// 验证双方把同一字段改成不同值时中间栏保持未决，并完整返回三方值。
    #[test]
    fn exposes_same_field_change_as_user_decision() {
        let base_command = command("logs", "查看日志");
        let mut local_command = base_command.clone();
        local_command.usage = "docker logs --tail 100 web".to_string();
        let mut remote_command = base_command.clone();
        remote_command.usage = "docker logs -f web".to_string();

        let plan = merge_command_documents(
            &document(vec![base_command]),
            &document(vec![local_command]),
            &document(vec![remote_command]),
        )
        .expect("同字段分歧应生成待选择项");

        assert_eq!(plan.conflict_count, 1);
        let field = &plan.records[0].fields[0];
        assert_eq!(field.key, "usage");
        assert_eq!(field.status, MergeFieldStatus::Conflict);
        assert_eq!(field.result_value, None);
        assert_eq!(
            field.local_value.as_ref().unwrap(),
            "docker logs --tail 100 web"
        );
        assert_eq!(field.remote_value.as_ref().unwrap(), "docker logs -f web");
        assert_eq!(
            plan.merged_document.categories[0].commands[0].usage, "echo logs",
            "未决字段在内部候选中暂存共同基线，不能伪装成已选择"
        );
    }

    /// 验证两台电脑分别产生的复制增量都被累计，而不是简单取本机或远端较大值。
    #[test]
    fn accumulates_copy_count_deltas_from_both_computers() {
        let mut base_command = command("logs", "查看日志");
        base_command.copy_count = 10;
        let mut local_command = base_command.clone();
        local_command.copy_count = 13;
        let mut remote_command = base_command.clone();
        remote_command.copy_count = 15;

        let plan = merge_command_documents(
            &document(vec![base_command]),
            &document(vec![local_command]),
            &document(vec![remote_command]),
        )
        .expect("复制增量应自动累计");

        assert_eq!(
            plan.merged_document.categories[0].commands[0].copy_count,
            18
        );
        let field = plan.records[0]
            .fields
            .iter()
            .find(|field| field.key == "copyCount")
            .expect("应展示复制次数自动累计");
        assert_eq!(field.status, MergeFieldStatus::AutoBoth);
        assert!(field.explanation.contains("10 + 3 + 5"));
    }

    /// 验证删除与修改冲突可以选择保持删除，也可以选择保留被修改的远端版本。
    #[test]
    fn applies_delete_vs_modify_record_decision() {
        let base_command = command("prune", "清理镜像");
        let mut remote_command = base_command.clone();
        remote_command.risk_note = "会删除所有未使用镜像".to_string();
        let base = document(vec![base_command]);
        let local = document(Vec::new());
        let remote = document(vec![remote_command.clone()]);
        let plan =
            merge_command_documents(&base, &local, &remote).expect("删除与修改应生成记录级选择");
        assert_eq!(plan.conflict_count, 1);
        assert_eq!(plan.records[0].kind, MergeRecordKind::DeleteVsModify);
        let resolution_id = plan.records[0].fields[0].resolution_id.clone();

        let deleted = apply_command_decisions(
            plan.clone(),
            &[MergeDecision {
                resolution_id: resolution_id.clone(),
                choice: MergeDecisionChoice::Local,
                custom_value: None,
            }],
        )
        .expect("选择本机应保持删除");
        assert!(deleted.categories[0].commands.is_empty());

        let kept = apply_command_decisions(
            plan,
            &[MergeDecision {
                resolution_id,
                choice: MergeDecisionChoice::Remote,
                custom_value: None,
            }],
        )
        .expect("选择远端应保留修改");
        assert_eq!(kept.categories[0].commands[0], remote_command);
    }

    /// 验证临时记录双方新增会自动汇总，同记录双边编辑则只要求一次整记录选择。
    #[test]
    fn merges_inbox_additions_and_applies_record_choice() {
        let base_item = inbox_item("shared", "共同内容", "2026-07-10T08:00:00Z");
        let mut local_edited = base_item.clone();
        local_edited.content = "本机修改".to_string();
        local_edited.updated_at = "2026-07-11T08:00:00Z".to_string();
        let mut remote_edited = base_item.clone();
        remote_edited.content = "远端修改".to_string();
        remote_edited.updated_at = "2026-07-12T08:00:00Z".to_string();
        let local_added = inbox_item("local", "本机新增", "2026-07-13T08:00:00Z");
        let remote_added = inbox_item("remote", "远端新增", "2026-07-14T08:00:00Z");

        let plan = merge_inbox_documents(
            &inbox(vec![base_item]),
            &inbox(vec![local_added.clone(), local_edited]),
            &inbox(vec![remote_added.clone(), remote_edited.clone()]),
        )
        .expect("临时收集应按记录合并");

        assert_eq!(plan.automatic_count, 2);
        assert_eq!(plan.conflict_count, 1);
        let resolution_id = plan
            .records
            .iter()
            .find(|record| record.entity_id == "shared")
            .expect("应展示共同记录冲突")
            .fields[0]
            .resolution_id
            .clone();
        let resolved = apply_inbox_decisions(
            plan,
            &[MergeDecision {
                resolution_id,
                choice: MergeDecisionChoice::Remote,
                custom_value: None,
            }],
        )
        .expect("选择远端记录后应生成有效时间流");
        assert_eq!(
            resolved
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["remote", "local", "shared"]
        );
        assert_eq!(
            resolved
                .items
                .iter()
                .find(|item| item.id == "shared")
                .unwrap(),
            &remote_edited
        );
    }
}
