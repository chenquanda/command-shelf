# CommandShelf AI 维护指南

## 项目定位

CommandShelf 是一个个人使用的 Windows 常用命令看板。用户把 Linux、Docker、Git、SSH 等命令保存在独立 Git 仓库的 `commands.json` 中，应用负责分类浏览、编辑、排序、复制以及显式拉取和推送。

保持产品轻量、单机、本地优先。除非用户明确提出新需求，不要主动加入搜索、AI 生成、命令执行、后台同步、应用内 GitHub 登录、Token 管理、冲突合并或多人协作。

## 首次接手顺序

1. 阅读本文件、`项目说明.md` 和 `docs/安装与使用说明.md`。
2. 执行 `git status -sb`，确认用户改动和当前分支状态。
3. 根据任务只读取相关模块，不恢复已经删除的原型、阶段截图或开发过程文档。
4. 修改前确认任务针对程序仓库还是数据仓库，两者不要混用。

## 仓库与目录

- 程序仓库：`git@github.com:chenquanda/command-shelf.git`。
- 数据仓库：`git@github.com:chenquanda/command-shelf-data.git`。
- 正式前端：`frontend/index.html`，为无构建步骤的单文件 HTML、CSS 和 JavaScript。
- Tauri 后端：`src-tauri/src`。
- Tauri 配置：`src-tauri/tauri.conf.json`。
- 桌面验收脚本：`scripts/desktop-smoke.mjs`。
- 发布门禁：`scripts/release-candidate.ps1`。
- 安装包：`release/CommandShelf_0.1.0_x64-setup.exe`。
- 本机构建缓存和临时证据统一放在 `.local`，不得提交。

不要重新创建根目录浏览器原型、设计对比图、S1～S6 过程截图、`design-qa.md`、产品构想或 AI 项目工作台；这些内容已经在项目收尾时删除。

## 技术栈与运行边界

- Rust 2021，最低 Rust 版本为 1.77.2。
- Tauri 2，Windows 桌面壳使用 WebView2。
- 前端没有 Node 构建流程；Node 只用于桌面验收脚本和前端语法检查。
- Git 同步由 Rust 后端调用系统 `git.exe`，不使用 GitHub API。
- Git for Windows 是连接数据仓库的前置条件，缺失时只提示安装，不维护第二套无 Git 数据模式。
- 主窗口默认 1180×820，最小尺寸 980×700。

## 核心模块职责

- `app_service.rs`：编排配置、文档保存、拉取和推送用例。
- `command_store.rs`：读取、校验和序列化 `commands.json`。
- `git_repository.rs`：验证仓库、运行受控 Git 命令并处理超时和错误分类。
- `config_store.rs`：在 `%APPDATA%\CommandShelf` 保存当前机器选择的数据仓库路径。
- `backup_store.rs`：写入前备份命令数据，备份不进入数据仓库。
- `file_io.rs`：原子写入、刷新和文件哈希。
- `model.rs`：`schemaVersion: 1` 数据模型和前后端快照契约。
- `error.rs`：稳定的结构化错误码、用户消息和下一步建议。
- `lib.rs`：向前端注册 Tauri 命令。

保持这些边界：文件和 Git 操作只能经过 Rust 后端；前端不得直接访问本机文件系统或启动进程。

## 数据格式

数据仓库根目录只维护一个 `commands.json`。第一版根结构如下：

```json
{
  "schemaVersion": 1,
  "categories": []
}
```

分类字段：

- `id`：非空且文档内唯一。
- `name`：非空。
- `description`、`icon`：可以为空。
- `commands`：有序命令数组，数组顺序就是界面顺序。

命令字段：

- `id`：非空且整份文档内唯一。
- `title`、`command`、`outputExample`：必填且非空。
- `description`、`usage`、`riskNote`、`notes`：可选文本。
- `parameters`：参数数组；每项的 `name` 和 `description` 必须同时非空。

文档最大 10 MB。保存时统一使用两空格缩进、LF 和结尾换行。不要随意改变字段名或 `schemaVersion`；格式升级必须同时提供兼容或迁移方案和测试。

## 加载与保存行为

- 应用在启动、选择仓库和拉取成功后读取并完整校验 `commands.json`。
- 应用不监听外部文件实时变化。手动修改 JSON 后需要重启、重新选择仓库或完成一次有效拉取。
- 界面新增、编辑和排序会先保存本地文件，再更新同步状态。
- 保存前必须核对文档基线，外部文件已经变化时拒绝覆盖。
- 无效远端数据不能替换当前可用数据。

## Git 同步规则

- 数据仓库必须是 Git 根目录，并具有 `origin`、当前分支和可用上游。
- 拉取只允许快进，不覆盖未提交或未推送的本地内容。
- 推送只暂存 `commands.json`，创建普通提交后执行普通推送。
- 不使用 `--force`，不在应用内自动合并分叉或冲突。
- 网络、认证、身份、超时或远端拒绝失败时必须保留本地数据，并返回可执行的中文提示。
- 应用不会在启动、退出或后台定时访问网络，网络操作必须由用户明确点击。

## 编码要求

- 所有手写代码必须有中文文件级、类型、字段、方法和关键逻辑注释。
- 注释说明职责、设计原因、业务规则、边界和副作用，不要只复述代码。
- 修改行为时同步更新相关注释，禁止留下过期说明。
- Rust 公共接口使用符合 Rustdoc 的中文注释；JavaScript 公共函数使用中文 JSDoc。
- 保持错误码稳定，用户消息和建议使用中文，不把命令正文、凭据或完整 Git 环境变量写入错误信息。
- 避免引入新依赖；确需引入时说明现有实现为何不能满足，并检查桌面体积和编译成本。

## 常用验证命令

代码修改至少执行与影响范围匹配的检查：

```powershell
cargo fmt --manifest-path src-tauri\Cargo.toml --check
cargo test --manifest-path src-tauri\Cargo.toml --all-targets
cargo clippy --manifest-path src-tauri\Cargo.toml --all-targets -- -D warnings
node --check scripts\desktop-smoke.mjs
```

正式发布使用：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\release-candidate.ps1
```

发布脚本会在 `.local` 下生成大量 Rust/Tauri 缓存。任务结束后如果用户要求清理，先保留需要交付的安装包，再删除 `.local` 中的缓存和验收夹具。

## 变更验收清单

- 数据模型变化：覆盖合法数据、缺失字段、重复 ID、未知版本和大小上限。
- 保存变化：覆盖原子写入、备份、外部基线变化和重启恢复。
- Git 变化：覆盖正常拉取/推送、远端领先、本地未提交、身份/认证/网络失败和进程超时。
- UI 变化：至少检查 1440×1024、1024×768、最小窗口、无横向溢出和长内容纵向滚动。
- 键盘变化：检查模态焦点圈、Escape 返回原入口、保存后焦点恢复及 `Alt+↑`、`Alt+↓` 排序。
- 安装变化：检查安装、开始菜单启动、卸载和数据仓库保留。
- 发布变化：核对 EXE、安装包、版本、架构、体积和 SHA-256。

界面操作测试可以交给用户时，应明确列出步骤；非界面自动测试由 AI 完成。没有修改代码时，不要为了形式重复完整编译并重新制造数 GiB 缓存。

## 当前状态与已知问题

- 当前版本：`0.1.0`。
- 已通过 Rust 测试、Clippy、格式、双尺寸界面、键盘、安装启动和卸载数据保留验证。
- 当前安装包 SHA-256：`C3CF971098263136EA4559B6D9D1A88727A420868CB219DCBA27ED20069411D8`。
- 已知发布问题：安装包内主程序与最后一次免安装构建的 SHA-256 不一致。下次发布前统一重建 EXE 和 NSIS 安装包，并增加安装后主程序哈希一致性门禁。

## 协作与 Git

- 默认在当前主工作区顺序完成小型修改，不为少量任务创建 worktree。
- 只有用户明确要求多 Agent，且任务可独立并行、没有前后依赖和文件所有权冲突时，才使用子 Agent 或 worktree。
- 删除或覆盖前检查工作区状态，保护用户已有和无关修改。
- 本地提交使用清晰的中文提交信息。
- 未经用户当次明确授权，不执行 `git push`、创建远端分支或 PR。
- 程序代码只推送到程序仓库；个人命令数据只推送到数据仓库，不把测试夹具写入个人数据。
