//! 文件职责：对命令与临时收集文档执行不依赖 Git 工作区的三方语义合并。
//! 主要内容：按稳定 ID 对齐结构化记录，自动接入单边修改，并把真正的同字段分歧暴露为可选择项。
//! 重要约束：本模块只计算内存中的候选结果，不读写文件、不运行 Git，也不替用户决定真实冲突。

use crate::error::AppError;
use crate::model::{CommandCategory, CommandDocument, CommandEntry};
use serde::{Deserialize, Serialize};
use serde_json::{to_value, Value};
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
        // 复制次数的双端增量累计属于独立业务规则，在下一个原子中替换通用三方选择。
        copy_count: merge_command_field!(copy_count, "copyCount", "复制次数"),
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
            // 删除与修改及同 ID 异构新增需要额外的记录级决议，下一原子将补齐该行为。
            _ => {
                return Err(AppError::new(
                    "MERGE_DECISION_UNSUPPORTED",
                    format!("命令 {command_id} 需要记录级合并决议。"),
                    "等待应用完成删除与修改冲突支持后重试。",
                    false,
                ));
            }
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
            _ => {
                return Err(AppError::new(
                    "MERGE_DECISION_UNSUPPORTED",
                    format!("分类 {category_id} 需要记录级合并决议。"),
                    "等待应用完成删除与修改冲突支持后重试。",
                    false,
                ));
            }
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

#[cfg(test)]
mod tests {
    //! 测试职责：锁定命令文档按 ID 对齐、双方新增和字段级三方选择的纯函数规则。

    use super::{merge_command_documents, MergeFieldStatus, MergeRecordKind};
    use crate::model::{CommandCategory, CommandDocument, CommandEntry};

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
}
