//! 文件职责：编排机器配置、命令与临时收集文档持久化及安全 Git 同步用例。
//! 主要内容：向 Tauri 命令提供稳定接口，并生成前端一次性消费的数据快照。
//! 重要约束：启动恢复失败返回错误快照；主动连接失败返回 `Err`，两者都不覆盖原数据。

use crate::backup_store::{backup_document, backup_inbox_document};
use crate::command_store::{
    initialize_empty_document, load_document, serialize_document, validate_document,
};
use crate::config_store::{load_config, save_config, AppConfig};
use crate::error::AppError;
use crate::file_io::atomic_write;
use crate::git_repository::{
    complete_conflict_rebase, prepare_pull_repository, prepare_push_repository,
    pull_repository as git_pull_repository, push_repository as git_push_repository,
    repository_has_local_changes, validate_repository, PullPreparationOutcome,
    PushPreparationOutcome, RepositoryInfo,
};
use crate::inbox_store::{
    initialize_empty_inbox_document, load_inbox_document, serialize_inbox_document,
    validate_inbox_document,
};
use crate::merge_engine::{
    apply_command_decisions, apply_inbox_decisions, merge_command_documents, merge_inbox_documents,
    CommandMergePlan, InboxMergePlan, MergeDecision,
};
use crate::model::{AppSnapshot, CommandDocument, InboxDocument, InboxSnapshot, SyncState};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 桌面应用用例服务；配置目录可注入以支持隔离测试。
#[derive(Debug, Clone)]
pub struct AppService {
    /// 当前服务实例读写的机器配置目录。
    config_directory: PathBuf,
}

/// 一次待完成冲突会话最初由拉取还是推送触发。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SyncOperationKind {
    /// 用户点击了拉取；确认后保留本地合并提交等待主动推送。
    Pull,
    /// 用户点击了推送；确认后还要继续执行普通 `git push`。
    Push,
}

/// 前端三栏窗口消费并原样带回的固定冲突会话。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncConflictSession {
    /// 触发冲突的用户操作。
    pub operation: SyncOperationKind,
    /// 共同祖先提交 OID，仅用于展示会话完整性和后续诊断。
    pub base_oid: String,
    /// 自动提交本机数据后的固定本机 OID。
    pub local_oid: String,
    /// fetch 后已校验的固定远端 OID。
    pub upstream_oid: String,
    /// `commands.json` 的自动结果、差异和待选择字段。
    pub command_plan: CommandMergePlan,
    /// `inbox.json` 的自动结果、差异和待选择记录。
    pub inbox_plan: InboxMergePlan,
    /// 两份文档已自动处理的变化总数。
    pub automatic_count: usize,
    /// 两份文档仍需用户选择的冲突总数。
    pub conflict_count: usize,
}

/// 一次显式同步的前端结果：已完成，或需要打开应用内冲突窗口。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SyncOperationResult {
    /// 同步没有真实冲突，直接返回新的应用快照。
    Completed {
        /// 同步成功后的完整应用快照。
        snapshot: AppSnapshot,
    },
    /// 仓库已恢复干净，前端应展示三栏冲突窗口。
    Conflict {
        /// 固定三方内容生成的语义合并会话。
        session: Box<SyncConflictSession>,
    },
}

impl AppService {
    /// 使用给定机器配置目录创建服务，不在构造阶段访问磁盘。
    pub fn new(config_directory: PathBuf) -> Self {
        Self { config_directory }
    }

    /// 恢复上次仓库和有效文档；首次运行返回未配置快照。
    pub fn load_app(&self) -> AppSnapshot {
        let config = match load_config(&self.config_directory) {
            Ok(Some(config)) => config,
            Ok(None) => return AppSnapshot::unconfigured(),
            Err(error) => return error_snapshot(None, error),
        };
        let repository_path = PathBuf::from(&config.repository_path);
        match self.load_repository(&repository_path, false) {
            Ok(snapshot) => snapshot,
            Err(error) => error_snapshot(Some(config.repository_path), error),
        }
    }

    /// 连接一个已克隆仓库，必要时创建空文档，并在全部校验通过后保存机器配置。
    pub fn choose_repository(&self, repository_path: &str) -> Result<AppSnapshot, AppError> {
        let snapshot = self.load_repository(Path::new(repository_path), true)?;
        let config = AppConfig {
            config_version: 1,
            repository_path: snapshot
                .repository_path
                .clone()
                .expect("成功仓库快照必须包含规范化路径"),
        };
        save_config(&self.config_directory, &config)?;
        Ok(snapshot)
    }

    /// 从当前已配置仓库读取临时收集文档，文件缺失时仅初始化一次空文档。
    ///
    /// 返回值：包含完整校验后的文档、原始字节哈希和本次是否初始化的标记。
    /// 副作用：仅当仓库根目录缺少 `inbox.json` 时创建空文件；已有文件绝不被替换。
    pub fn load_inbox_document(&self) -> Result<InboxSnapshot, AppError> {
        let config = load_config(&self.config_directory)?.ok_or_else(|| {
            AppError::new(
                "REPO_NOT_CONFIGURED",
                "当前电脑尚未选择数据仓库。",
                "先连接已经克隆到本机的个人数据仓库。",
                false,
            )
        })?;
        let repository = validate_repository(Path::new(&config.repository_path))?;
        let inbox_path = repository.root.join("inbox.json");
        let initialized = if inbox_path.exists() {
            false
        } else {
            initialize_empty_inbox_document(&inbox_path)?;
            true
        };
        let (document, document_hash) = load_inbox_document(&inbox_path)?;

        Ok(InboxSnapshot {
            document,
            document_hash,
            initialized_empty_document: initialized,
        })
    }

    /// 在外部基线未变化时备份并原子保存完整临时收集文档。
    ///
    /// 参数：`expected_hash` 必须来自最近一次成功读取或保存结果；不一致时拒绝覆盖磁盘。
    /// 返回值：保存后重新读取并完整校验的文档和新哈希。
    /// 副作用：在机器配置目录创建备份，并原子替换仓库中的 `inbox.json`；不访问 Git 或网络。
    pub fn save_inbox_document(
        &self,
        document: InboxDocument,
        expected_hash: &str,
    ) -> Result<InboxSnapshot, AppError> {
        validate_inbox_document(&document)?;
        let config = load_config(&self.config_directory)?.ok_or_else(|| {
            AppError::new(
                "REPO_NOT_CONFIGURED",
                "当前电脑尚未选择数据仓库。",
                "先连接已经克隆到本机的个人数据仓库。",
                false,
            )
        })?;
        let repository = validate_repository(Path::new(&config.repository_path))?;
        let inbox_path = repository.root.join("inbox.json");
        let (_, current_hash) = load_inbox_document(&inbox_path)?;
        if current_hash != expected_hash {
            return Err(AppError::new(
                "INBOX_BASELINE_CHANGED",
                "inbox.json 已被其他程序或窗口修改，本次保存已停止。",
                "重新加载临时收集内容，确认最新数据后再编辑。",
                true,
            ));
        }

        // 所有业务校验与基线检查必须先完成，备份成功后才允许原子替换正式文件。
        let bytes = serialize_inbox_document(&document)?;
        backup_inbox_document(&self.config_directory, &repository.root, &inbox_path)?;
        atomic_write(&inbox_path, &bytes)?;

        let (saved_document, saved_hash) = load_inbox_document(&inbox_path)?;
        Ok(InboxSnapshot {
            document: saved_document,
            document_hash: saved_hash,
            initialized_empty_document: false,
        })
    }

    /// 在外部基线未变化且 Git 状态可确认的前提下，备份并原子保存完整命令文档。
    ///
    /// 参数：`expected_hash` 必须来自最近一次成功快照；不一致时拒绝覆盖磁盘。
    /// 副作用：写入前完成所有可能失败的 Git 查询，再创建备份并替换仓库中的 `commands.json`。
    pub fn save_document(
        &self,
        document: CommandDocument,
        expected_hash: &str,
    ) -> Result<AppSnapshot, AppError> {
        validate_document(&document)?;
        let config = load_config(&self.config_directory)?.ok_or_else(|| {
            AppError::new(
                "REPO_NOT_CONFIGURED",
                "当前电脑尚未选择数据仓库。",
                "先连接已经克隆到本机的个人数据仓库。",
                false,
            )
        })?;
        let repository = validate_repository(Path::new(&config.repository_path))?;
        let document_path = repository.root.join("commands.json");
        let (_, current_hash) = load_document(&document_path)?;
        if current_hash != expected_hash {
            return Err(AppError::new(
                "BASELINE_CHANGED",
                "commands.json 已被其他程序或窗口修改，本次保存已停止。",
                "重新启动或重新连接仓库，确认最新内容后再编辑。",
                true,
            ));
        }

        // Git 状态必须在任何磁盘副作用之前确认；否则写入成功后的查询失败会让界面错误回滚。
        let dirty_before_save = repository_has_local_changes(&repository.root)?;

        let bytes = serialize_document(&document)?;
        backup_document(&self.config_directory, &repository.root, &document_path)?;
        atomic_write(&document_path, &bytes)?;

        let (saved_document, saved_hash) = load_document(&document_path)?;
        // 写入后不再调用 Git；原状态已脏或文件字节发生变化，都足以确定需要后续推送。
        let dirty = dirty_before_save || saved_hash != current_hash;
        Ok(success_snapshot(
            repository,
            saved_document,
            saved_hash,
            false,
            dirty,
        ))
    }

    /// 从当前分支上游安全拉取，并在保留本地数据提交的前提下校验和接入远端更新。
    ///
    /// 副作用：会刷新 `origin` 远端引用；接入远端后不再运行可能把成功误报成失败的 Git 状态查询。
    pub fn pull_repository(&self) -> Result<AppSnapshot, AppError> {
        let config = load_config(&self.config_directory)?.ok_or_else(|| {
            AppError::new(
                "REPO_NOT_CONFIGURED",
                "当前电脑尚未选择数据仓库。",
                "先连接已经克隆到本机的个人数据仓库。",
                false,
            )
        })?;
        let repository = validate_repository(Path::new(&config.repository_path))?;
        let document_path = repository.root.join("commands.json");
        let inbox_path = repository.root.join("inbox.json");
        let baseline_document = load_document(&document_path)?;
        let outcome = git_pull_repository(&repository.root)?;
        // Blob 只负责接入前校验；远端更新落入工作树后必须重新读取，以包含 EOL 转换后的真实字节哈希。
        let (document, document_hash) = if outcome.updated {
            load_document(&document_path)?
        } else {
            baseline_document
        };
        // 旧仓库允许暂时缺少 inbox.json；一旦文件存在，就必须与远端候选一样通过完整校验。
        if inbox_path.exists() {
            load_inbox_document(&inbox_path)?;
        }
        let mut snapshot = success_snapshot(
            repository,
            document,
            document_hash,
            false,
            outcome.has_local_changes,
        );
        snapshot.status_message = match (outcome.updated, outcome.has_local_changes) {
            (true, true) => "已接入远端更新，本地修改仍待推送。",
            (true, false) => "已拉取并加载远端最新命令。",
            (false, true) => "远端没有新更新，本地修改仍待推送。",
            (false, false) => "本地数据已经是远端最新版本。",
        }
        .to_string();
        Ok(snapshot)
    }

    /// 启动一次支持应用内冲突窗口的拉取。
    ///
    /// 无冲突时返回成功快照；受管文件冲突时 Git 已恢复干净，并返回固定三方语义合并会话。
    pub fn start_pull_repository(&self) -> Result<SyncOperationResult, AppError> {
        let config = load_config(&self.config_directory)?.ok_or_else(|| {
            AppError::new(
                "REPO_NOT_CONFIGURED",
                "当前电脑尚未选择数据仓库。",
                "先连接已经克隆到本机的个人数据仓库。",
                false,
            )
        })?;
        let repository = validate_repository(Path::new(&config.repository_path))?;
        let document_path = repository.root.join("commands.json");
        let inbox_path = repository.root.join("inbox.json");
        let baseline_document = load_document(&document_path)?;
        match prepare_pull_repository(&repository.root)? {
            PullPreparationOutcome::Completed(outcome) => {
                let (document, document_hash) = if outcome.updated {
                    load_document(&document_path)?
                } else {
                    baseline_document
                };
                if inbox_path.exists() {
                    load_inbox_document(&inbox_path)?;
                }
                let mut snapshot = success_snapshot(
                    repository,
                    document,
                    document_hash,
                    false,
                    outcome.has_local_changes,
                );
                snapshot.status_message = match (outcome.updated, outcome.has_local_changes) {
                    (true, true) => "已接入远端更新，本地修改仍待推送。",
                    (true, false) => "已拉取并加载远端最新命令。",
                    (false, true) => "远端没有新更新，本地修改仍待推送。",
                    (false, false) => "本地数据已经是远端最新版本。",
                }
                .to_string();
                Ok(SyncOperationResult::Completed { snapshot })
            }
            PullPreparationOutcome::Conflict { snapshot, .. } => {
                let command_plan = merge_command_documents(
                    &snapshot.base_commands,
                    &snapshot.local_commands,
                    &snapshot.remote_commands,
                )?;
                let inbox_plan = merge_inbox_documents(
                    &snapshot.base_inbox,
                    &snapshot.local_inbox,
                    &snapshot.remote_inbox,
                )?;
                let automatic_count = command_plan.automatic_count + inbox_plan.automatic_count;
                let conflict_count = command_plan.conflict_count + inbox_plan.conflict_count;
                Ok(SyncOperationResult::Conflict {
                    session: Box::new(SyncConflictSession {
                        operation: SyncOperationKind::Pull,
                        base_oid: snapshot.base_oid,
                        local_oid: snapshot.local_oid,
                        upstream_oid: snapshot.upstream_oid,
                        command_plan,
                        inbox_plan,
                        automatic_count,
                        conflict_count,
                    }),
                })
            }
        }
    }

    /// 应用三栏窗口中的全部决议并完成拉取；合并提交保留在本机等待用户主动推送。
    ///
    /// 副作用：创建应用外备份、重放本机提交、原子写入两份有效文档并创建普通本地提交。
    pub fn complete_pull_conflict(
        &self,
        session: SyncConflictSession,
        decisions: &[MergeDecision],
    ) -> Result<AppSnapshot, AppError> {
        if session.operation != SyncOperationKind::Pull {
            return Err(AppError::new(
                "MERGE_DECISION_INVALID",
                "当前冲突会话不是由拉取操作创建。",
                "关闭窗口并重新点击拉取。",
                true,
            ));
        }
        let (repository, document, document_hash) =
            self.apply_conflict_session(session, decisions)?;
        let mut snapshot = success_snapshot(repository, document, document_hash, false, true);
        snapshot.status_message = "冲突已合并，本地修改仍待推送。".to_string();
        Ok(snapshot)
    }

    /// 把已确认决议写入仓库并返回重新加载的命令文档；拉取与推送共享同一安全实现。
    fn apply_conflict_session(
        &self,
        session: SyncConflictSession,
        decisions: &[MergeDecision],
    ) -> Result<(RepositoryInfo, CommandDocument, String), AppError> {
        let config = load_config(&self.config_directory)?.ok_or_else(|| {
            AppError::new(
                "REPO_NOT_CONFIGURED",
                "当前电脑尚未选择数据仓库。",
                "重新连接数据仓库后再同步。",
                false,
            )
        })?;
        let repository = validate_repository(Path::new(&config.repository_path))?;
        let command_decisions: Vec<MergeDecision> = decisions
            .iter()
            .filter(|decision| !decision.resolution_id.starts_with("inbox:"))
            .cloned()
            .collect();
        let inbox_decisions: Vec<MergeDecision> = decisions
            .iter()
            .filter(|decision| decision.resolution_id.starts_with("inbox:"))
            .cloned()
            .collect();
        let commands = apply_command_decisions(session.command_plan, &command_decisions)?;
        let inbox = apply_inbox_decisions(session.inbox_plan, &inbox_decisions)?;
        let document_path = repository.root.join("commands.json");
        let inbox_path = repository.root.join("inbox.json");
        backup_document(&self.config_directory, &repository.root, &document_path)?;
        if inbox_path.exists() {
            backup_inbox_document(&self.config_directory, &repository.root, &inbox_path)?;
        }
        complete_conflict_rebase(
            &repository.root,
            &session.local_oid,
            &session.upstream_oid,
            &commands,
            &inbox,
        )?;
        let (document, document_hash) = load_document(&document_path)?;
        load_inbox_document(&inbox_path)?;
        Ok((repository, document, document_hash))
    }

    /// 保存范围校验通过后，只提交两个受管数据文件，接入已校验远端更新并执行普通推送。
    ///
    /// 副作用：可能创建一个本地 Git 提交并访问 `origin`；成功推送后不再执行状态确认查询。
    pub fn push_repository(&self) -> Result<AppSnapshot, AppError> {
        let config = load_config(&self.config_directory)?.ok_or_else(|| {
            AppError::new(
                "REPO_NOT_CONFIGURED",
                "当前电脑尚未选择数据仓库。",
                "先连接已经克隆到本机的个人数据仓库。",
                false,
            )
        })?;
        let repository = validate_repository(Path::new(&config.repository_path))?;
        let document_path = repository.root.join("commands.json");
        let inbox_path = repository.root.join("inbox.json");
        let (document, document_hash) = load_document(&document_path)?;
        // 未使用临时收集页的旧仓库可以没有 inbox.json；存在时必须先阻止无效本地数据进入提交。
        if inbox_path.exists() {
            load_inbox_document(&inbox_path)?;
        }
        let outcome = git_push_repository(&repository.root)?;
        let mut snapshot = success_snapshot(repository, document, document_hash, false, false);
        snapshot.status_message = match (outcome.committed, outcome.pushed) {
            (true, true) => "本地修改已提交并推送。",
            (false, true) => "已有本地提交已推送。",
            _ => "本地与远端已经一致，无需推送。",
        }
        .to_string();
        Ok(snapshot)
    }

    /// 启动一次支持应用内冲突窗口的普通推送。
    ///
    /// 无冲突时保持既有推送结果；受管文件冲突时返回三方会话且绝不强制覆盖远端。
    pub fn start_push_repository(&self) -> Result<SyncOperationResult, AppError> {
        let config = load_config(&self.config_directory)?.ok_or_else(|| {
            AppError::new(
                "REPO_NOT_CONFIGURED",
                "当前电脑尚未选择数据仓库。",
                "先连接已经克隆到本机的个人数据仓库。",
                false,
            )
        })?;
        let repository = validate_repository(Path::new(&config.repository_path))?;
        let document_path = repository.root.join("commands.json");
        let inbox_path = repository.root.join("inbox.json");
        let (document, document_hash) = load_document(&document_path)?;
        if inbox_path.exists() {
            load_inbox_document(&inbox_path)?;
        }
        match prepare_push_repository(&repository.root)? {
            PushPreparationOutcome::Completed(outcome) => {
                let mut snapshot =
                    success_snapshot(repository, document, document_hash, false, false);
                snapshot.status_message = match (outcome.committed, outcome.pushed) {
                    (true, true) => "本地修改已提交并推送。",
                    (false, true) => "已有本地提交已推送。",
                    _ => "本地与远端已经一致，无需推送。",
                }
                .to_string();
                Ok(SyncOperationResult::Completed { snapshot })
            }
            PushPreparationOutcome::Conflict(snapshot) => {
                let command_plan = merge_command_documents(
                    &snapshot.base_commands,
                    &snapshot.local_commands,
                    &snapshot.remote_commands,
                )?;
                let inbox_plan = merge_inbox_documents(
                    &snapshot.base_inbox,
                    &snapshot.local_inbox,
                    &snapshot.remote_inbox,
                )?;
                let automatic_count = command_plan.automatic_count + inbox_plan.automatic_count;
                let conflict_count = command_plan.conflict_count + inbox_plan.conflict_count;
                Ok(SyncOperationResult::Conflict {
                    session: Box::new(SyncConflictSession {
                        operation: SyncOperationKind::Push,
                        base_oid: snapshot.base_oid,
                        local_oid: snapshot.local_oid,
                        upstream_oid: snapshot.upstream_oid,
                        command_plan,
                        inbox_plan,
                        automatic_count,
                        conflict_count,
                    }),
                })
            }
        }
    }

    /// 应用三栏窗口决议后继续普通推送；远端再次前进时仍按正常拒绝或新冲突安全停止。
    pub fn complete_push_conflict(
        &self,
        session: SyncConflictSession,
        decisions: &[MergeDecision],
    ) -> Result<AppSnapshot, AppError> {
        if session.operation != SyncOperationKind::Push {
            return Err(AppError::new(
                "MERGE_DECISION_INVALID",
                "当前冲突会话不是由推送操作创建。",
                "关闭窗口并重新点击推送。",
                true,
            ));
        }
        let (repository, document, document_hash) =
            self.apply_conflict_session(session, decisions)?;
        let outcome = git_push_repository(&repository.root)?;
        let mut snapshot = success_snapshot(repository, document, document_hash, false, false);
        snapshot.status_message = match (outcome.committed, outcome.pushed) {
            (true, true) => "冲突已合并，本地结果已提交并推送。",
            (false, true) => "冲突已合并并推送。",
            _ => "冲突合并后本地与远端已经一致。",
        }
        .to_string();
        Ok(snapshot)
    }

    /// 校验仓库、按需初始化空文档并构造成功快照。
    fn load_repository(
        &self,
        repository_path: &Path,
        initialize_when_missing: bool,
    ) -> Result<AppSnapshot, AppError> {
        let repository = validate_repository(repository_path)?;
        let document_path = repository.root.join("commands.json");
        let initialized = if !document_path.exists() {
            if !initialize_when_missing {
                return Err(AppError::new(
                    "DATA_NOT_FOUND",
                    "已保存的仓库中缺少 commands.json。",
                    "重新打开仓库设置以初始化空数据，或恢复原数据文件。",
                    true,
                ));
            }
            initialize_empty_document(&document_path)?;
            true
        } else {
            false
        };

        let (document, document_hash) = load_document(&document_path)?;
        let dirty = repository_has_local_changes(&repository.root)?;
        Ok(success_snapshot(
            repository,
            document,
            document_hash,
            initialized,
            dirty,
        ))
    }
}

/// 构造已连接仓库的成功快照，并从 Git 事实推导同步状态。
fn success_snapshot(
    repository: RepositoryInfo,
    document: CommandDocument,
    document_hash: String,
    initialized: bool,
    dirty: bool,
) -> AppSnapshot {
    let sync_state = if dirty {
        SyncState::Dirty
    } else {
        SyncState::Synced
    };
    let status_message = if initialized {
        "已创建空数据文件，等待后续推送。"
    } else if dirty {
        "本地数据已加载，并检测到尚未推送的修改。"
    } else {
        "本地数据仓库已连接。"
    };
    AppSnapshot {
        document,
        repository_path: Some(user_visible_path(&repository.root)),
        repository_name: Some(repository.name),
        sync_state,
        status_message: status_message.to_string(),
        document_hash: Some(document_hash),
        initialized_empty_document: initialized,
        error: None,
    }
}

/// 把 Windows 规范化路径转换为适合配置和界面展示的普通路径文本。
fn user_visible_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        const VERBATIM_PREFIX: &str = "\\\\?\\";
        const VERBATIM_UNC_PREFIX: &str = "\\\\?\\UNC\\";
        if let Some(remainder) = text.strip_prefix(VERBATIM_UNC_PREFIX) {
            return format!("\\\\{remainder}");
        }
        if let Some(remainder) = text.strip_prefix(VERBATIM_PREFIX) {
            return remainder.to_string();
        }
    }
    text.to_string()
}

/// 构造启动恢复失败快照，明确区分无效数据与真正空数据。
fn error_snapshot(repository_path: Option<String>, error: AppError) -> AppSnapshot {
    let repository_name = repository_path.as_ref().and_then(|path| {
        Path::new(path)
            .file_name()
            .and_then(|value| value.to_str())
            .map(ToOwned::to_owned)
    });
    AppSnapshot {
        document: CommandDocument::empty(),
        repository_path,
        repository_name,
        sync_state: SyncState::Error,
        status_message: format!("{} {}", error.message, error.action),
        document_hash: None,
        initialized_empty_document: false,
        error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    //! 测试职责：使用临时裸仓库和本地克隆验证数据持久化、同步与失败恢复闭环。

    use super::{AppService, SyncOperationResult};
    use crate::command_store::{load_document, serialize_document};
    use crate::config_store::{save_config, AppConfig};
    use crate::git_repository::repository_has_local_changes;
    use crate::inbox_store::{load_inbox_document, serialize_inbox_document};
    use crate::merge_engine::{MergeDecision, MergeDecisionChoice};
    use crate::model::SyncState;
    use crate::model::{CommandCategory, CommandDocument, CommandEntry};
    use crate::model::{InboxDocument, InboxEntry};
    use std::fs;
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// 运行测试专用 Git 命令；失败时保留标准错误以便直接定位环境问题。
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

    /// 运行并返回测试 Git 命令的标准输出。
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

    /// 判断测试 Git 命令是否成功，供需要验证暂存区退出码的用例使用。
    fn git_succeeds(directory: &Path, arguments: &[&str]) -> bool {
        Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .status()
            .expect("测试环境应能启动系统 Git")
            .success()
    }

    /// 创建包含初始提交和上游的远端与工作克隆。
    fn cloned_repository(root: &Path) -> PathBuf {
        let remote = root.join("remote.git");
        fs::create_dir_all(&remote).expect("应能创建远端目录");
        git(&remote, &["init", "--bare"]);

        let seed = root.join("seed");
        let root_text = root.to_string_lossy().to_string();
        git(root, &["clone", remote.to_string_lossy().as_ref(), "seed"]);
        git(&seed, &["config", "user.name", "CommandShelf Test"]);
        git(
            &seed,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        fs::write(seed.join("README.md"), "# test\n").expect("应能写入种子文件");
        git(&seed, &["add", "README.md"]);
        git(&seed, &["commit", "-m", "初始化测试仓库"]);
        git(&seed, &["push", "-u", "origin", "HEAD"]);

        let clone = root.join("work");
        git(
            &PathBuf::from(&root_text),
            &["clone", remote.to_string_lossy().as_ref(), "work"],
        );
        clone
    }

    /// 生成只改变标题的合法第一版文档，便于分辨拉取前后内容。
    fn document_json(title: &str) -> String {
        format!(
            r#"{{
  "schemaVersion": 1,
  "categories": [
    {{
      "id": "category-linux",
      "name": "Linux",
      "commands": [
        {{
          "id": "command-process",
          "title": "{title}",
          "command": "ps aux",
          "outputExample": "USER PID COMMAND"
        }}
      ]
    }}
  ]
}}
"#
        )
    }

    /// 在指定克隆中配置测试身份、提交当前数据文件并推送。
    fn commit_and_push_document(repository: &Path, message: &str) {
        git(repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        git(repository, &["add", "commands.json"]);
        git(repository, &["commit", "-m", message]);
        git(repository, &["push"]);
    }

    /// 在另一克隆中只提交临时收集文件，供拉取候选校验场景使用。
    fn commit_and_push_inbox(repository: &Path, message: &str) {
        git(repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        git(repository, &["add", "inbox.json"]);
        git(repository, &["commit", "-m", message]);
        git(repository, &["push"]);
    }

    /// 在裸远端安装确定性拒绝 hook；Unix 需要显式补充可执行权限。
    fn install_rejecting_pre_receive_hook(remote: &Path) {
        let hook = remote.join("hooks").join("pre-receive");
        fs::write(
            &hook,
            "#!/bin/sh\necho 'CommandShelf deterministic rejection' >&2\nexit 1\n",
        )
        .expect("应能写入远端拒绝 hook");
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&hook)
                .expect("应能读取 hook 权限")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&hook, permissions).expect("应能设置 hook 可执行权限");
        }
    }

    /// 验证首次运行不会把原型样例当成正式数据。
    #[test]
    fn first_run_is_unconfigured_and_empty() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let service = AppService::new(directory.path().join("config"));

        let snapshot = service.load_app();

        assert_eq!(snapshot.sync_state, SyncState::Unconfigured);
        assert!(snapshot.document.categories.is_empty());
        assert!(snapshot.repository_path.is_none());
    }

    /// 验证未配置仓库时不能在任意目录创建临时收集文件。
    #[test]
    fn rejects_inbox_load_without_repository_config() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let service = AppService::new(directory.path().join("config"));

        let error = service
            .load_inbox_document()
            .expect_err("未配置仓库时应拒绝读取临时收集文档");

        assert_eq!(error.code, "REPO_NOT_CONFIGURED");
    }

    /// 验证首次读取创建空文件，后续服务实例保留原文件并返回相同哈希。
    #[test]
    fn initializes_and_reloads_inbox_document() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let config_directory = directory.path().join("config");
        let service = AppService::new(config_directory.clone());
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("测试仓库应能连接");

        let initialized = service
            .load_inbox_document()
            .expect("首次读取应初始化空文档");
        assert!(initialized.initialized_empty_document);
        assert_eq!(initialized.document, InboxDocument::empty());
        assert!(repository.join("inbox.json").exists());

        let reloaded = AppService::new(config_directory)
            .load_inbox_document()
            .expect("再次读取应保留现有文档");
        assert!(!reloaded.initialized_empty_document);
        assert_eq!(reloaded.document, initialized.document);
        assert_eq!(reloaded.document_hash, initialized.document_hash);
    }

    /// 验证新增或修改 inbox.json 会单独让仓库进入本地有修改状态。
    #[test]
    fn reports_inbox_as_managed_local_change() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("已提交命令"),
        )
        .expect("应能写入命令文档");
        commit_and_push_document(&repository, "加入命令数据");
        let service = AppService::new(directory.path().join("config"));
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("干净仓库应能连接");
        assert!(!repository_has_local_changes(&repository).expect("应能检查初始状态"));

        service
            .load_inbox_document()
            .expect("首次读取应初始化临时收集文件");

        assert!(repository_has_local_changes(&repository).expect("应能识别 Inbox 变化"));
    }

    /// 验证仓库中已有无效临时收集文件会报错，且不会被空文档初始化覆盖。
    #[test]
    fn rejects_invalid_existing_inbox_without_overwrite() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let service = AppService::new(directory.path().join("config"));
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("测试仓库应能连接");
        let invalid_bytes = br#"{"schemaVersion":2,"items":[]}"#;
        fs::write(repository.join("inbox.json"), invalid_bytes).expect("应能写入无效测试文件");

        let error = service.load_inbox_document().expect_err("未知版本应被拒绝");

        assert_eq!(error.code, "INBOX_UNSUPPORTED_SCHEMA");
        assert_eq!(
            fs::read(repository.join("inbox.json")).expect("无效原文件应保留"),
            invalid_bytes
        );
    }

    /// 构造安全保存测试使用的一条临时记录文档。
    fn inbox_document(content: &str) -> InboxDocument {
        InboxDocument {
            schema_version: 1,
            items: vec![InboxEntry {
                id: "inbox-save-1".to_string(),
                content: content.to_string(),
                created_at: "2026-07-14T06:32:00.000Z".to_string(),
                updated_at: "2026-07-14T07:00:00.000Z".to_string(),
            }],
        }
    }

    /// 验证临时收集保存会产生写入前备份、新哈希，并可由新服务实例完整恢复。
    #[test]
    fn saves_inbox_with_backup_and_restart_recovery() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let config_directory = directory.path().join("config");
        let service = AppService::new(config_directory.clone());
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("测试仓库应能连接");
        let loaded = service
            .load_inbox_document()
            .expect("首次读取应初始化空文档");
        let original_bytes = fs::read(repository.join("inbox.json")).expect("应能读取原文件");

        let saved = service
            .save_inbox_document(inbox_document("稍后处理"), &loaded.document_hash)
            .expect("合法临时收集文档应保存成功");

        assert_eq!(saved.document.items[0].content, "稍后处理");
        assert_ne!(saved.document_hash, loaded.document_hash);
        let backup_files: Vec<_> = fs::read_dir(config_directory.join("backups"))
            .expect("应能读取备份根目录")
            .flat_map(|entry| {
                fs::read_dir(entry.expect("仓库备份目录应有效").path()).expect("应能读取备份目录")
            })
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("inbox."))
            })
            .collect();
        assert_eq!(backup_files.len(), 1);
        assert_eq!(
            fs::read(backup_files[0].path()).expect("应能读取备份"),
            original_bytes
        );

        let restarted = AppService::new(config_directory)
            .load_inbox_document()
            .expect("重启后应能恢复保存内容");
        assert_eq!(restarted.document, saved.document);
        assert_eq!(restarted.document_hash, saved.document_hash);
    }

    /// 验证外部修改哈希后拒绝陈旧保存，并完整保留外部写入内容。
    #[test]
    fn rejects_stale_inbox_hash_without_overwrite() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let service = AppService::new(directory.path().join("config"));
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("测试仓库应能连接");
        let loaded = service
            .load_inbox_document()
            .expect("首次读取应初始化空文档");
        let external_bytes =
            serialize_inbox_document(&inbox_document("外部修改")).expect("外部测试文档应可序列化");
        fs::write(repository.join("inbox.json"), &external_bytes).expect("应能模拟外部写入");

        let error = service
            .save_inbox_document(inbox_document("界面旧内容"), &loaded.document_hash)
            .expect_err("陈旧哈希不得覆盖外部修改");

        assert_eq!(error.code, "INBOX_BASELINE_CHANGED");
        assert_eq!(
            fs::read(repository.join("inbox.json")).expect("外部内容应保留"),
            external_bytes
        );
    }

    /// 验证备份目录不可创建时停止正式写入，最后一份有效临时收集文件保持不变。
    #[test]
    fn preserves_inbox_when_backup_creation_fails() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let config_directory = directory.path().join("config");
        let service = AppService::new(config_directory.clone());
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("测试仓库应能连接");
        let loaded = service
            .load_inbox_document()
            .expect("首次读取应初始化空文档");
        let original_bytes = fs::read(repository.join("inbox.json")).expect("应能读取原文件");
        fs::write(config_directory.join("backups"), "阻止创建备份目录")
            .expect("应能制造稳定的备份失败条件");

        let error = service
            .save_inbox_document(inbox_document("不得写入"), &loaded.document_hash)
            .expect_err("备份失败时保存必须停止");

        assert_eq!(error.code, "BACKUP_FAILED");
        assert_eq!(
            fs::read(repository.join("inbox.json")).expect("原文件应保留"),
            original_bytes
        );
    }

    /// 验证机器配置损坏时启动快照保留结构化错误，而不是伪装成首次运行。
    #[test]
    fn reports_invalid_machine_config_on_startup() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let config_directory = directory.path().join("config");
        fs::create_dir_all(&config_directory).expect("应能创建配置目录");
        fs::write(config_directory.join("config.json"), "{invalid").expect("应能写入损坏配置");

        let snapshot = AppService::new(config_directory).load_app();

        assert_eq!(snapshot.sync_state, SyncState::Error);
        assert!(snapshot.repository_path.is_none());
        assert_eq!(
            snapshot.error.expect("错误快照应携带结构化原因").code,
            "CONFIG_INVALID"
        );
    }

    /// 验证已保存仓库被移动后仍显示原路径和错误，便于用户重新选择而不误判为空数据。
    #[test]
    fn reports_stale_repository_path_on_startup() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let config_directory = directory.path().join("config");
        let missing_repository = directory.path().join("moved-repository");
        save_config(
            &config_directory,
            &AppConfig {
                config_version: 1,
                repository_path: missing_repository.to_string_lossy().to_string(),
            },
        )
        .expect("测试配置应能保存");

        let snapshot = AppService::new(config_directory).load_app();

        assert_eq!(snapshot.sync_state, SyncState::Error);
        assert_eq!(
            snapshot.repository_path.as_deref(),
            Some(missing_repository.to_string_lossy().as_ref())
        );
        assert_eq!(
            snapshot.error.expect("失效路径应携带结构化原因").code,
            "PATH_NOT_FOUND"
        );
    }

    /// 验证连接仓库会初始化空文档，并可由新服务实例恢复。
    #[test]
    fn connects_initializes_and_restores_repository() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let config_directory = directory.path().join("config");
        let service = AppService::new(config_directory.clone());

        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        assert!(connected.initialized_empty_document);
        assert_eq!(connected.sync_state, SyncState::Dirty);
        assert!(connected.document.categories.is_empty());
        assert!(repository.join("commands.json").exists());

        let restarted = AppService::new(config_directory).load_app();
        assert!(!restarted.initialized_empty_document);
        assert_eq!(restarted.sync_state, SyncState::Dirty);
        assert_eq!(restarted.document, connected.document);
        assert_eq!(restarted.repository_path, connected.repository_path);
    }

    /// 验证已有第一版文档会原样加载，不会被空初始化覆盖。
    #[test]
    fn connects_and_loads_existing_document() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let document = r#"{
  "schemaVersion": 1,
  "categories": [
    {
      "id": "category-linux",
      "name": "Linux",
      "commands": [
        {
          "id": "command-process",
          "title": "查看进程",
          "command": "ps aux",
          "outputExample": "USER PID COMMAND"
        }
      ]
    }
  ]
}
"#;
        fs::write(repository.join("commands.json"), document).expect("应能写入现有文档");
        git(&repository, &["add", "commands.json"]);
        git(&repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            &repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        git(&repository, &["commit", "-m", "加入现有命令数据"]);
        git(&repository, &["push"]);

        let service = AppService::new(directory.path().join("config"));
        let snapshot = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("已有文档连接应成功");

        assert!(!snapshot.initialized_empty_document);
        assert_eq!(snapshot.sync_state, SyncState::Synced);
        assert_eq!(snapshot.document.categories.len(), 1);
        assert_eq!(
            snapshot.document.categories[0].commands[0].command_text,
            "ps aux"
        );
        assert!(
            !snapshot
                .repository_path
                .as_deref()
                .unwrap_or_default()
                .starts_with("\\\\?\\"),
            "界面路径不应暴露 Windows verbatim 前缀"
        );
    }

    /// 构造保存测试使用的一条命令文档；标题参数便于制造序列化后等长的内容变化。
    fn edited_document_with_title(title: &str) -> CommandDocument {
        CommandDocument {
            schema_version: 1,
            categories: vec![CommandCategory {
                id: "category-linux".to_string(),
                name: "Linux".to_string(),
                description: "系统命令".to_string(),
                icon: "terminal".to_string(),
                commands: vec![CommandEntry {
                    id: "command-process".to_string(),
                    title: title.to_string(),
                    command_text: "ps aux".to_string(),
                    description: "查看全部进程".to_string(),
                    usage: "ps aux".to_string(),
                    parameters: Vec::new(),
                    output_example: "USER PID COMMAND".to_string(),
                    risk_note: String::new(),
                    notes: String::new(),
                    copy_count: 0,
                }],
            }],
        }
    }

    /// 构造 S2 保存测试使用的默认命令文档。
    fn edited_document() -> CommandDocument {
        edited_document_with_title("查看进程")
    }

    /// 验证编辑文档会备份、原子保存并在重启后恢复，同时拒绝陈旧哈希覆盖。
    #[test]
    fn saves_document_with_backup_and_baseline_protection() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let config_directory = directory.path().join("config");
        let service = AppService::new(config_directory.clone());
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        let original_hash = connected.document_hash.expect("连接快照应包含哈希");

        let saved = service
            .save_document(edited_document(), &original_hash)
            .expect("编辑文档保存应成功");
        assert_eq!(saved.sync_state, SyncState::Dirty);
        assert_eq!(saved.document.categories.len(), 1);
        assert_ne!(saved.document_hash.as_deref(), Some(original_hash.as_str()));

        let stale_error = service
            .save_document(CommandDocument::empty(), &original_hash)
            .expect_err("陈旧哈希不得覆盖新数据");
        assert_eq!(stale_error.code, "BASELINE_CHANGED");

        let restarted = AppService::new(config_directory.clone()).load_app();
        assert_eq!(restarted.document, edited_document());
        let backup_count = fs::read_dir(config_directory.join("backups"))
            .expect("应能读取备份根目录")
            .filter_map(Result::ok)
            .flat_map(|entry| fs::read_dir(entry.path()).into_iter().flatten())
            .filter_map(Result::ok)
            .count();
        assert_eq!(backup_count, 1, "首次编辑应创建一份写入前备份");
    }

    /// 验证写入后即使 Git 状态已不可读，保存仍返回与磁盘一致的新文档和哈希。
    #[test]
    fn preserves_disk_baseline_when_git_status_check_fails() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let original_document = edited_document_with_title("查看进程");
        fs::write(
            repository.join("commands.json"),
            serialize_document(&original_document).expect("原始文档应能序列化"),
        )
        .expect("应能写入原始文档");
        fs::write(
            repository.join(".gitattributes"),
            "commands.json filter=break-status\n",
        )
        .expect("应能写入测试属性");
        git(&repository, &["add", "commands.json", ".gitattributes"]);
        git(&repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            &repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        git(&repository, &["commit", "-m", "加入状态失败测试基线"]);
        git(&repository, &["push"]);

        let config_directory = directory.path().join("config");
        let service = AppService::new(config_directory.clone());
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("过滤器失效前应能连接仓库");
        let original_hash = connected.document_hash.expect("连接快照应包含哈希");
        // `required` clean filter 只在 Git 读取变化后的工作区内容时失败；等长标题确保状态检查进入过滤路径。
        git(
            &repository,
            &["config", "filter.break-status.clean", "false"],
        );
        git(
            &repository,
            &["config", "filter.break-status.required", "true"],
        );
        let replacement = edited_document_with_title("检查进程");
        let saved = service
            .save_document(replacement.clone(), &original_hash)
            .expect("写入前状态可确认时，后续状态故障不得把成功保存改成失败");
        let status_error = repository_has_local_changes(&repository)
            .expect_err("变化后的文档应稳定触发测试 clean filter 故障");

        assert_eq!(status_error.code, "GIT_FAILED");
        assert_eq!(saved.sync_state, SyncState::Dirty);
        assert_eq!(saved.document, replacement);
        let (disk_document, disk_hash) =
            load_document(&repository.join("commands.json")).expect("新文档应完整写入磁盘");
        assert_eq!(saved.document, disk_document);
        assert_eq!(saved.document_hash.as_deref(), Some(disk_hash.as_str()));
        assert_ne!(disk_hash, original_hash);

        git(
            &repository,
            &["config", "--unset", "filter.break-status.required"],
        );
        git(
            &repository,
            &["config", "--unset", "filter.break-status.clean"],
        );
        let restarted = AppService::new(config_directory).load_app();
        assert_eq!(restarted.document, disk_document);
        assert_eq!(restarted.document_hash.as_deref(), Some(disk_hash.as_str()));
    }

    /// 验证另一克隆推送有效文档后，本地只通过快进更新并加载新内容。
    #[test]
    fn pulls_valid_remote_document_with_fast_forward() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("本地初始命令"),
        )
        .expect("应能写入初始文档");
        commit_and_push_document(&repository, "加入初始命令数据");

        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("连接干净仓库应成功");
        assert_eq!(connected.sync_state, SyncState::Synced);

        let producer = directory.path().join("producer");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer",
            ],
        );
        fs::write(
            producer.join("commands.json"),
            document_json("来自另一台电脑的命令"),
        )
        .expect("应能写入远端候选文档");
        commit_and_push_document(&producer, "更新远端命令数据");

        let pulled = service.pull_repository().expect("有效远端应能快进拉取");
        assert_eq!(pulled.sync_state, SyncState::Synced);
        assert_eq!(
            pulled.document.categories[0].commands[0].title,
            "来自另一台电脑的命令"
        );
        assert_eq!(
            git_output(&repository, &["rev-parse", "HEAD"]),
            git_output(&repository, &["rev-parse", "@{u}"])
        );
        assert!(
            !repository.join("inbox.json").exists(),
            "旧远端缺少 inbox.json 时拉取仍应成功，初始化延后到首次进入临时收集页"
        );
        let inbox = service
            .load_inbox_document()
            .expect("旧仓库拉取后应可兼容初始化空 Inbox");
        assert!(inbox.initialized_empty_document);
        assert_eq!(inbox.document, InboxDocument::empty());
    }

    /// 验证 CRLF 工作树使用实际文件哈希作为拉取基线，拉取后可立即保存。
    #[test]
    fn uses_worktree_hash_after_pull_with_crlf_checkout() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join(".gitattributes"),
            "commands.json text eol=crlf\n",
        )
        .expect("应能写入 EOL 属性");
        fs::write(
            repository.join("commands.json"),
            document_json("本地 CRLF 基线"),
        )
        .expect("应能写入初始文档");
        git(&repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            &repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        git(&repository, &["add", ".gitattributes", "commands.json"]);
        git(&repository, &["commit", "-m", "加入 CRLF 数据基线"]);
        git(&repository, &["push"]);
        fs::remove_file(repository.join("commands.json")).expect("应能移除工作树副本");
        git(&repository, &["checkout", "--", "commands.json"]);

        let service = AppService::new(directory.path().join("config"));
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("CRLF 工作树应能连接");
        let producer = directory.path().join("producer-crlf");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer-crlf",
            ],
        );
        fs::write(
            producer.join("commands.json"),
            document_json("来自远端的 CRLF 命令"),
        )
        .expect("应能写入远端候选文档");
        commit_and_push_document(&producer, "更新 CRLF 命令数据");

        let pulled = service.pull_repository().expect("有效 CRLF 候选应能快进");
        let worktree_bytes =
            fs::read(repository.join("commands.json")).expect("快进后应能读取工作树文档");
        assert!(
            worktree_bytes.windows(2).any(|pair| pair == b"\r\n"),
            "测试必须实际覆盖 CRLF 工作树字节"
        );
        let (_, worktree_hash) =
            load_document(&repository.join("commands.json")).expect("工作树文档应有效");
        assert_eq!(
            pulled.document_hash.as_deref(),
            Some(worktree_hash.as_str()),
            "pull 快照必须返回工作树实际字节哈希，而不是 Git blob 哈希"
        );

        let mut edited = pulled.document.clone();
        edited.categories[0].commands[0].title = "拉取后立即保存".to_string();
        let saved = service
            .save_document(
                edited.clone(),
                pulled.document_hash.as_deref().expect("拉取快照应有哈希"),
            )
            .expect("使用 pull 返回哈希立即保存时不应误报 BASELINE_CHANGED");
        assert_eq!(saved.document, edited);
    }

    /// 验证本地命令修改与远端 Inbox 更新并存时，拉取会接入远端并保留待推送的本地提交。
    #[test]
    fn rebases_remote_update_while_preserving_local_changes() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("本地基线命令"),
        )
        .expect("应能写入本地基线");
        commit_and_push_document(&repository, "加入本地基线");
        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        let producer = directory.path().join("producer");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer",
            ],
        );
        fs::write(
            producer.join("inbox.json"),
            serialize_inbox_document(&InboxDocument::empty()).expect("空 Inbox 应能序列化"),
        )
        .expect("应能写入远端 Inbox 更新");
        commit_and_push_inbox(&producer, "制造不冲突的远端更新");
        service
            .save_document(
                edited_document(),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("本地命令修改应能保存");

        let pulled = service
            .pull_repository()
            .expect("不冲突的远端更新应自动接入");

        assert_eq!(pulled.sync_state, SyncState::Dirty);
        assert_eq!(pulled.document, edited_document());
        assert_eq!(
            git_output(&repository, &["rev-list", "--count", "@{u}..HEAD"]),
            "1",
            "拉取后本地数据提交应等待用户主动推送"
        );
        assert_eq!(
            load_inbox_document(&repository.join("inbox.json"))
                .expect("拉取后应加载远端 Inbox")
                .0,
            InboxDocument::empty()
        );
    }

    /// 验证拉取仍拒绝受管数据之外的工作区变化，不会把其他文件带入自动提交。
    #[test]
    fn refuses_to_pull_unrelated_worktree_changes() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let service = AppService::new(directory.path().join("config"));
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        fs::write(repository.join("notes.txt"), "不属于应用的数据").expect("应能创建无关文件");
        let before_head = git_output(&repository, &["rev-parse", "HEAD"]);

        let error = service
            .pull_repository()
            .expect_err("无关工作区变化必须阻止拉取");

        assert_eq!(error.code, "WORKTREE_DIRTY");
        assert_eq!(git_output(&repository, &["rev-parse", "HEAD"]), before_head);
    }

    /// 验证远端候选 JSON 无效时只更新远端引用，不快进 HEAD 或覆盖本地文件。
    #[test]
    fn rejects_invalid_remote_document_before_fast_forward() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("有效本地命令"),
        )
        .expect("应能写入本地文档");
        commit_and_push_document(&repository, "加入有效数据");
        let service = AppService::new(directory.path().join("config"));
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("连接有效仓库应成功");
        let before_head = git_output(&repository, &["rev-parse", "HEAD"]);
        let before_file = fs::read(repository.join("commands.json")).expect("应能读取有效本地文档");

        let producer = directory.path().join("producer");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer",
            ],
        );
        fs::write(producer.join("commands.json"), "{ invalid json").expect("应能写入无效远端候选");
        commit_and_push_document(&producer, "写入无效远端数据");

        let error = service
            .pull_repository()
            .expect_err("无效远端不得快进到本地");

        assert_eq!(error.code, "REMOTE_DATA_INVALID");
        assert_eq!(git_output(&repository, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            fs::read(repository.join("commands.json")).expect("本地有效文档应保留"),
            before_file
        );
    }

    /// 验证远端 inbox.json 无效时不快进 HEAD，也不在本地创建或覆盖临时收集文件。
    #[test]
    fn rejects_invalid_remote_inbox_before_fast_forward() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("有效本地命令"),
        )
        .expect("应能写入本地命令文档");
        commit_and_push_document(&repository, "加入有效命令数据");
        let service = AppService::new(directory.path().join("config"));
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("连接有效仓库应成功");
        let before_head = git_output(&repository, &["rev-parse", "HEAD"]);

        let producer = directory.path().join("producer-invalid-inbox");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer-invalid-inbox",
            ],
        );
        fs::write(
            producer.join("inbox.json"),
            r#"{"schemaVersion":1,"items":[{"id":"dup","content":"一","createdAt":"t","updatedAt":"t"},{"id":"dup","content":"二","createdAt":"t","updatedAt":"t"}]}"#,
        )
        .expect("应能写入重复 ID 候选");
        commit_and_push_inbox(&producer, "写入无效临时收集数据");

        let error = service
            .pull_repository()
            .expect_err("无效远端 Inbox 不得快进到本地");

        assert_eq!(error.code, "REMOTE_INBOX_INVALID");
        assert_eq!(git_output(&repository, &["rev-parse", "HEAD"]), before_head);
        assert!(!repository.join("inbox.json").exists());
    }

    /// 验证一次推送只提交两个受管数据文件，并可由另一克隆获得相同内容。
    #[test]
    fn commits_and_pushes_document_to_another_clone() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        git(&repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            &repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        let saved = service
            .save_document(
                edited_document(),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("本地编辑保存应成功");
        assert_eq!(saved.sync_state, SyncState::Dirty);
        let loaded_inbox = service
            .load_inbox_document()
            .expect("首次读取应初始化临时收集文件");
        let saved_inbox = service
            .save_inbox_document(inbox_document("跨电脑同步"), &loaded_inbox.document_hash)
            .expect("临时收集修改应保存成功");

        let pushed = service.push_repository().expect("普通推送应成功");
        assert_eq!(pushed.sync_state, SyncState::Synced);
        assert_eq!(
            git_output(&repository, &["rev-parse", "HEAD"]),
            git_output(&repository, &["rev-parse", "@{u}"])
        );
        let subject = git_output(&repository, &["log", "-1", "--pretty=%s"]);
        assert!(subject.starts_with("chore(data): sync CommandShelf data "));

        let verifier = directory.path().join("verifier");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "verifier",
            ],
        );
        let verifier_document =
            fs::read_to_string(verifier.join("commands.json")).expect("另一克隆应能读取推送数据");
        assert!(verifier_document.contains("查看进程"));
        let (verifier_inbox, _) = load_inbox_document(&verifier.join("inbox.json"))
            .expect("另一克隆应能读取推送的临时收集数据");
        assert_eq!(verifier_inbox, saved_inbox.document);

        let no_op = service.push_repository().expect("无变化推送应安全完成");
        assert_eq!(no_op.sync_state, SyncState::Synced);
    }

    /// 验证真实本地连接拒绝会保留安全本地提交，文档字节、哈希与重启数据保持不变。
    #[test]
    fn preserves_document_and_local_commit_when_loopback_remote_refuses_connection() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let config_directory = directory.path().join("config");
        let service = AppService::new(config_directory.clone());
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        let saved = service
            .save_document(
                edited_document(),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("本地文档保存应成功");
        let saved_hash = saved.document_hash.clone().expect("保存快照应有哈希");
        let before_bytes = fs::read(repository.join("commands.json")).expect("应能读取保存文档");
        let before_head = git_output(&repository, &["rev-parse", "HEAD"]);
        let original_origin = git_output(&repository, &["remote", "get-url", "origin"]);

        // 先由系统分配空闲端口再关闭监听器，紧接着访问可稳定得到本机连接拒绝而不依赖互联网。
        let listener = TcpListener::bind("127.0.0.1:0").expect("应能分配本地测试端口");
        let port = listener.local_addr().expect("应能读取本地端口").port();
        drop(listener);
        let refused_origin = format!("http://127.0.0.1:{port}/commands.git");
        git(
            &repository,
            &["remote", "set-url", "origin", refused_origin.as_str()],
        );
        let error = service
            .push_repository()
            .expect_err("连接拒绝时真实 Git push 流程应停止");

        assert_eq!(error.code, "GIT_NETWORK_FAILED");
        assert_eq!(
            fs::read(repository.join("commands.json")).unwrap(),
            before_bytes
        );
        assert_ne!(git_output(&repository, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            git_output(
                &repository,
                &["status", "--porcelain=v1", "--", "commands.json"],
            ),
            "",
            "联网失败前创建的安全提交不应残留工作区修改"
        );
        assert_eq!(
            git_output(&repository, &["rev-list", "--count", "@{u}..HEAD"]),
            "1",
            "网络恢复后应能直接重试推送本地提交"
        );
        let (disk_document, disk_hash) =
            load_document(&repository.join("commands.json")).expect("失败后文档仍应有效");
        assert_eq!(disk_document, saved.document);
        assert_eq!(disk_hash, saved_hash);

        git(
            &repository,
            &["remote", "set-url", "origin", original_origin.as_str()],
        );
        let restarted = AppService::new(config_directory).load_app();
        assert_eq!(restarted.sync_state, SyncState::Dirty);
        assert_eq!(restarted.document, disk_document);
        assert_eq!(restarted.document_hash.as_deref(), Some(disk_hash.as_str()));
    }

    /// 验证 pre-receive 拒绝后只保留本地提交，磁盘/hash 与重启快照仍完全一致。
    #[test]
    fn preserves_document_and_local_commit_after_pre_receive_rejection() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let remote = directory.path().join("remote.git");
        git(&repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            &repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        let config_directory = directory.path().join("config");
        let service = AppService::new(config_directory.clone());
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        let saved = service
            .save_document(
                edited_document(),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("本地文档保存应成功");
        let saved_hash = saved.document_hash.clone().expect("保存快照应有哈希");
        let before_bytes = fs::read(repository.join("commands.json")).expect("应能读取保存文档");
        let before_local_head = git_output(&repository, &["rev-parse", "HEAD"]);
        let before_remote_head = git_output(&remote, &["rev-parse", "HEAD"]);
        install_rejecting_pre_receive_hook(&remote);

        let error = service
            .push_repository()
            .expect_err("远端 hook 应拒绝真实推送");

        assert_eq!(error.code, "GIT_PUSH_REJECTED");
        assert_eq!(
            fs::read(repository.join("commands.json")).unwrap(),
            before_bytes
        );
        assert_ne!(
            git_output(&repository, &["rev-parse", "HEAD"]),
            before_local_head,
            "自动提交应作为可重试的本地进度保留"
        );
        assert_eq!(
            git_output(&remote, &["rev-parse", "HEAD"]),
            before_remote_head,
            "拒绝 hook 不得推进远端"
        );
        assert_eq!(
            git_output(
                &repository,
                &["status", "--porcelain=v1", "--", "commands.json"],
            ),
            "",
            "已提交的数据文件不应残留工作区变化"
        );
        assert_eq!(
            git_output(&repository, &["rev-list", "--count", "@{u}..HEAD"]),
            "1"
        );
        let (disk_document, disk_hash) =
            load_document(&repository.join("commands.json")).expect("拒绝后文档仍应有效");
        assert_eq!(disk_document, saved.document);
        assert_eq!(disk_hash, saved_hash);

        let restarted = AppService::new(config_directory).load_app();
        assert_eq!(restarted.sync_state, SyncState::Dirty);
        assert_eq!(restarted.document, disk_document);
        assert_eq!(restarted.document_hash.as_deref(), Some(disk_hash.as_str()));
    }

    /// 验证本地命令修改与远端 Inbox 更新并存时，推送会自动接入远端并保留双方内容。
    #[test]
    fn rebases_remote_update_before_push() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("本地基线命令"),
        )
        .expect("应能写入本地基线");
        commit_and_push_document(&repository, "加入本地基线");
        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("连接干净仓库应成功");
        let producer = directory.path().join("producer");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer",
            ],
        );
        fs::write(
            producer.join("inbox.json"),
            serialize_inbox_document(&InboxDocument::empty()).expect("空 Inbox 应能序列化"),
        )
        .expect("应能写入远端 Inbox 更新");
        commit_and_push_inbox(&producer, "制造不冲突的远端领先");

        service
            .save_document(
                edited_document(),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("远端更新前的本地编辑应保存");
        let pushed = service
            .push_repository()
            .expect("不冲突的远端更新应自动接入并推送");

        assert_eq!(pushed.sync_state, SyncState::Synced);
        assert_eq!(
            git_output(&repository, &["rev-parse", "HEAD"]),
            git_output(&repository, &["rev-parse", "@{u}"])
        );
        assert_eq!(
            load_document(&repository.join("commands.json"))
                .expect("推送后本地命令应有效")
                .0,
            edited_document()
        );
        assert_eq!(
            load_inbox_document(&repository.join("inbox.json"))
                .expect("推送后应保留远端 Inbox")
                .0,
            InboxDocument::empty()
        );
    }

    /// 验证拉取冲突会返回三栏会话，并能按用户选择生成保留双方非冲突变化的本地提交。
    #[test]
    fn completes_pull_conflict_from_structured_decision() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("共同基线命令"),
        )
        .expect("应能写入共同基线");
        commit_and_push_document(&repository, "加入共同基线");
        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("连接干净仓库应成功");
        let producer = directory.path().join("producer");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer",
            ],
        );
        fs::write(
            producer.join("commands.json"),
            document_json("远端最终标题"),
        )
        .expect("应能写入远端冲突版本");
        commit_and_push_document(&producer, "加入远端修改");

        service
            .save_document(
                edited_document_with_title("本机最终标题"),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("本机冲突版本应保存");
        let started = service
            .start_pull_repository()
            .expect("冲突拉取应返回结构化会话");
        let SyncOperationResult::Conflict { session } = started else {
            panic!("同一标题的不同修改必须打开冲突窗口");
        };
        assert!(session.automatic_count >= 1);
        assert_eq!(session.conflict_count, 1);
        let title_resolution = session
            .command_plan
            .records
            .iter()
            .flat_map(|record| &record.fields)
            .find(|field| field.key == "title")
            .expect("应存在标题冲突")
            .resolution_id
            .clone();

        let completed = service
            .complete_pull_conflict(
                *session,
                &[MergeDecision {
                    resolution_id: title_resolution,
                    choice: MergeDecisionChoice::Remote,
                    custom_value: None,
                }],
            )
            .expect("选择远端标题后应完成本地合并");

        assert_eq!(completed.sync_state, SyncState::Dirty);
        assert_eq!(
            completed.document.categories[0].commands[0].title,
            "远端最终标题"
        );
        assert_eq!(
            completed.document.categories[0].commands[0].description, "查看全部进程",
            "本机独有的说明修改必须保留"
        );
        assert_eq!(
            git_output(&repository, &["status", "--porcelain"]),
            "",
            "合并完成后工作区应保持干净"
        );
        assert_ne!(
            git_output(&repository, &["rev-parse", "HEAD"]),
            git_output(&repository, &["rev-parse", "@{u}"]),
            "拉取合并后应保留本地提交等待主动推送"
        );
    }

    /// 验证推送冲突使用同一结构化决议，并在确认后继续完成普通推送。
    #[test]
    fn completes_push_conflict_and_updates_upstream() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("共同基线命令"),
        )
        .expect("应能写入共同基线");
        commit_and_push_document(&repository, "加入共同基线");
        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("连接干净仓库应成功");
        let producer = directory.path().join("producer");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer",
            ],
        );
        fs::write(
            producer.join("commands.json"),
            document_json("远端竞争标题"),
        )
        .expect("应能写入远端冲突版本");
        commit_and_push_document(&producer, "加入远端竞争修改");
        service
            .save_document(
                edited_document_with_title("本机保留标题"),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("本机冲突版本应保存");

        let started = service
            .start_push_repository()
            .expect("冲突推送应返回结构化会话");
        let SyncOperationResult::Conflict { session } = started else {
            panic!("推送同字段分歧必须打开冲突窗口");
        };
        let title_resolution = session
            .command_plan
            .records
            .iter()
            .flat_map(|record| &record.fields)
            .find(|field| field.key == "title")
            .expect("应存在标题冲突")
            .resolution_id
            .clone();
        let completed = service
            .complete_push_conflict(
                *session,
                &[MergeDecision {
                    resolution_id: title_resolution,
                    choice: MergeDecisionChoice::Local,
                    custom_value: None,
                }],
            )
            .expect("选择本机标题后应继续普通推送");

        assert_eq!(completed.sync_state, SyncState::Synced);
        assert_eq!(
            completed.document.categories[0].commands[0].title,
            "本机保留标题"
        );
        assert_eq!(
            git_output(&repository, &["rev-parse", "HEAD"]),
            git_output(&repository, &["rev-parse", "@{u}"]),
            "完成冲突推送后本地与上游应一致"
        );
        assert_eq!(git_output(&repository, &["status", "--porcelain"]), "");
    }

    /// 验证双方修改同一命令产生冲突时，推送会退出 rebase 并保留已提交的本地数据。
    #[test]
    fn aborts_push_rebase_conflict_and_preserves_local_commit() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        fs::write(
            repository.join("commands.json"),
            document_json("本地基线命令"),
        )
        .expect("应能写入本地基线");
        commit_and_push_document(&repository, "加入本地基线");
        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("连接干净仓库应成功");
        let producer = directory.path().join("producer");
        git(
            directory.path(),
            &[
                "clone",
                directory
                    .path()
                    .join("remote.git")
                    .to_string_lossy()
                    .as_ref(),
                "producer",
            ],
        );
        fs::write(
            producer.join("commands.json"),
            document_json("远端领先命令"),
        )
        .expect("应能写入远端更新");
        commit_and_push_document(&producer, "制造远端领先");

        service
            .save_document(
                edited_document(),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("远端更新前的本地编辑应保存");
        let before_head = git_output(&repository, &["rev-parse", "HEAD"]);
        let error = service
            .push_repository()
            .expect_err("同一内容冲突时推送必须安全停止");

        assert_eq!(error.code, "GIT_DIVERGED");
        assert_ne!(git_output(&repository, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            git_output(&repository, &["status", "--porcelain"]),
            "",
            "自动中止后不应残留冲突或工作区修改"
        );
        assert!(!repository.join(".git").join("rebase-merge").exists());
        assert!(!repository.join(".git").join("rebase-apply").exists());
        assert_eq!(
            load_document(&repository.join("commands.json"))
                .expect("冲突停止后本地命令应保持有效")
                .0,
            edited_document()
        );
    }

    /// 验证专用仓库中夹带其他未提交文件时不会创建自动提交。
    #[test]
    fn refuses_to_push_unrelated_worktree_changes() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let service = AppService::new(directory.path().join("config"));
        service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        fs::write(repository.join("notes.txt"), "不属于应用的数据").expect("应能创建无关测试文件");

        let before_head = git_output(&repository, &["rev-parse", "HEAD"]);
        let error = service
            .push_repository()
            .expect_err("无关工作区变化应阻止推送");

        assert_eq!(error.code, "WORKTREE_DIRTY");
        assert_eq!(git_output(&repository, &["rev-parse", "HEAD"]), before_head);
    }

    /// 验证提交身份缺失时撤销应用暂存，修复身份后可直接重试且数据不丢失。
    #[test]
    fn unstages_document_after_identity_failure_and_allows_retry() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        let saved = service
            .save_document(
                edited_document(),
                connected
                    .document_hash
                    .as_deref()
                    .expect("连接快照应有哈希"),
            )
            .expect("本地数据保存应成功");
        let before_failure =
            fs::read(repository.join("commands.json")).expect("应能读取失败前文档");

        // 空的仓库级身份覆盖可能存在的全局身份，稳定触发 Git 身份失败分支。
        git(&repository, &["config", "user.name", ""]);
        git(&repository, &["config", "user.email", ""]);
        let error = service
            .push_repository()
            .expect_err("缺少有效提交身份时应停止推送");

        assert_eq!(error.code, "GIT_IDENTITY_REQUIRED");
        assert!(
            git_succeeds(&repository, &["diff", "--cached", "--quiet"]),
            "应用创建的暂存必须在提交失败后撤销"
        );
        assert_eq!(
            fs::read(repository.join("commands.json")).expect("本地文档应继续存在"),
            before_failure
        );

        git(&repository, &["config", "user.name", "CommandShelf Test"]);
        git(
            &repository,
            &["config", "user.email", "commandshelf-test@example.invalid"],
        );
        let retried = service.push_repository().expect("修复身份后应可直接重试");
        assert_eq!(retried.sync_state, SyncState::Synced);
        assert_eq!(retried.document, saved.document);
    }
}
