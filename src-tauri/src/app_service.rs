//! 文件职责：编排机器配置、命令文档持久化与安全 Git 同步用例。
//! 主要内容：向 Tauri 命令提供稳定接口，并生成前端一次性消费的完整快照。
//! 重要约束：启动恢复失败返回错误快照；主动连接失败返回 `Err`，两者都不覆盖原数据。

use crate::backup_store::backup_document;
use crate::command_store::{
    initialize_empty_document, load_document, serialize_document, validate_document,
};
use crate::config_store::{load_config, save_config, AppConfig};
use crate::error::AppError;
use crate::file_io::atomic_write;
use crate::git_repository::{
    pull_repository as git_pull_repository, push_repository as git_push_repository,
    repository_has_local_changes, validate_repository, RepositoryInfo,
};
use crate::model::{AppSnapshot, CommandDocument, SyncState};
use std::path::{Path, PathBuf};

/// 桌面应用用例服务；配置目录可注入以支持隔离测试。
#[derive(Debug, Clone)]
pub struct AppService {
    /// 当前服务实例读写的机器配置目录。
    config_directory: PathBuf,
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

    /// 从当前分支上游安全拉取，并使用同一固定提交已校验的文档构造快照。
    ///
    /// 副作用：会刷新 `origin` 远端引用；快进后不再运行可能把成功误报成失败的磁盘或状态查询。
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
        let baseline_document = load_document(&document_path)?;
        let outcome = git_pull_repository(&repository.root)?;
        // Blob 只负责合并前校验；快进后必须重新读取工作树，以包含 EOL 转换后的真实字节哈希。
        let (document, document_hash) = if outcome.updated {
            load_document(&document_path)?
        } else {
            baseline_document
        };
        let mut snapshot = success_snapshot(repository, document, document_hash, false, false);
        snapshot.status_message = if outcome.updated {
            "已拉取并加载远端最新命令。"
        } else {
            "本地数据已经是远端最新版本。"
        }
        .to_string();
        Ok(snapshot)
    }

    /// 保存范围校验通过后，只提交 `commands.json` 并执行普通推送。
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
        let (document, document_hash) = load_document(&document_path)?;
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

    use super::AppService;
    use crate::command_store::{load_document, serialize_document};
    use crate::config_store::{save_config, AppConfig};
    use crate::git_repository::repository_has_local_changes;
    use crate::model::SyncState;
    use crate::model::{CommandCategory, CommandDocument, CommandEntry};
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

    /// 验证未提交本地文件会在 fetch 前停止，数据文件和提交均保持不变。
    #[test]
    fn stops_pull_when_local_document_is_dirty() {
        let directory = tempfile::tempdir().expect("应能创建测试目录");
        let repository = cloned_repository(directory.path());
        let service = AppService::new(directory.path().join("config"));
        let connected = service
            .choose_repository(repository.to_string_lossy().as_ref())
            .expect("首次连接应成功");
        let before = fs::read(repository.join("commands.json")).expect("应能读取本地文档");

        let error = service
            .pull_repository()
            .expect_err("本地未提交文档不得被拉取覆盖");

        assert_eq!(error.code, "LOCAL_CHANGES");
        assert_eq!(
            fs::read(repository.join("commands.json")).expect("应能再次读取本地文档"),
            before
        );
        assert_eq!(connected.document, CommandDocument::empty());
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

    /// 验证一次推送只提交命令数据，并可由另一克隆获得相同文档。
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

        let no_op = service.push_repository().expect("无变化推送应安全完成");
        assert_eq!(no_op.sync_state, SyncState::Synced);
    }

    /// 验证真实本地连接拒绝只让推送失败，文档字节、哈希、Git 状态与重启数据保持不变。
    #[test]
    fn preserves_document_when_loopback_remote_refuses_connection() {
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
        let before_status = git_output(
            &repository,
            &["status", "--porcelain=v1", "--", "commands.json"],
        );
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
        assert_eq!(git_output(&repository, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            git_output(
                &repository,
                &["status", "--porcelain=v1", "--", "commands.json"],
            ),
            before_status
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

    /// 验证远端领先时推送不会提交或覆盖本地未提交数据。
    #[test]
    fn stops_push_before_commit_when_remote_is_ahead() {
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
            .expect_err("远端领先时普通推送必须停止");

        assert_eq!(error.code, "REMOTE_AHEAD");
        assert_eq!(git_output(&repository, &["rev-parse", "HEAD"]), before_head);
        assert!(
            git_output(
                &repository,
                &["status", "--porcelain", "--", "commands.json"]
            )
            .contains("commands.json"),
            "本地数据修改应继续保留"
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
