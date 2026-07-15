# CommandShelf

CommandShelf 是一个面向个人使用的 Windows 常用命令看板，用来保存容易忘记的 Linux、Docker、Git、SSH 等命令，也可以临时记录尚未整理的文字和链接。

应用采用本地优先设计：所有数据保存在用户选择的本地 Git 仓库中，浏览和编辑不依赖网络；只有用户明确点击“拉取”或“推送”时才访问远端。

## 功能

- 按分类浏览、新增和编辑命令，记录说明、用法、参数、参考输出和风险提示。
- 复制正式命令正文时累计使用次数，用户可主动按复制次数排序。
- 通过固定“临时收集”入口保存文字或链接，并支持时间流、行内编辑和确认删除。
- 调用本机 Codex CLI 生成结构化命令草稿；草稿经用户确认后才写入正式数据。
- 使用系统 Git 执行显式拉取和普通推送；同步时会安全提交受管本地修改，并接入已校验的远端更新。
- 两台电脑修改同一数据后，应用会自动合并新增内容和互不重叠的字段；真实冲突直接在本机、合并结果、远端三栏窗口中选择或编辑。
- 文件保存、Git 和 Codex 等阻塞操作在 Tauri 后台线程执行，避免占用桌面事件线程。

CommandShelf 不会执行、试运行或验证任何命令，也不内置 GitHub 登录、Token 管理或其他 AI 提供方。

## 安装

当前 Windows x64 安装包：

[下载 CommandShelf 0.1.0 安装包](release/CommandShelf_0.1.0_x64-setup.exe)

安装包 SHA-256：

`B24551D9466736C90558AE89D6ADCE40A301904A3C94B91403FB1F6E5F467565`

运行前提：

- Windows 10 或 Windows 11，系统具备 WebView2。
- 需要连接数据仓库时安装 Git for Windows。
- 只有使用“问 Codex”时才需要安装并登录 Codex CLI。

完整的安装、仓库准备和日常使用步骤见 [安装与使用说明](docs/安装与使用说明.md)。

## 数据仓库

程序仓库和个人数据仓库相互独立：

- 程序仓库：`git@github.com:C-Q-D/command-shelf.git`
- 数据仓库：`git@github.com:C-Q-D/command-shelf-data.git`

数据仓库根目录只由应用管理以下两个文件：

- `commands.json`：分类、正式命令及复制次数。
- `inbox.json`：临时收集记录。

两个文件当前都使用 `schemaVersion: 1`。应用不会暂存或提交数据仓库中的其他文件；卸载应用也不会删除数据仓库。

## 项目结构

```text
frontend/index.html                 无构建步骤的正式单文件前端
src-tauri/src                      Rust 后端、持久化、Git 和 Codex CLI
src-tauri/tauri.conf.json          Tauri 窗口与 NSIS 打包配置
scripts/复制无刷新回归.mjs         复制计数局部更新回归
scripts/临时收集回归.mjs           临时收集端到端回归
scripts/同步冲突界面回归.mjs       三栏冲突窗口和同步契约回归
scripts/release-candidate.ps1      发布门禁和候选构建
scripts/生成安装包.ps1             一键生成、落位并验证安装包
docs/安装与使用说明.md             面向用户的完整说明
release/                            当前可交付安装包
```

本地编译缓存、运行期夹具和发布证据统一写入 `.local`，不会提交到 Git。

## 开发环境

- Rust 2021；`Cargo.toml` 声明 `rust-version = 1.77.2`，当前发布实际使用并验证 Rust/Cargo 1.96.1。
- Tauri 2、MSVC C++ Build Tools 和 WebView2。
- Node.js 仅用于前端与桌面回归脚本，前端本身没有 Node 构建流程。
- Git for Windows 用于集成测试和数据仓库同步。

常用验证命令：

```powershell
cargo fmt --manifest-path src-tauri\Cargo.toml --check
cargo test --manifest-path src-tauri\Cargo.toml --all-targets
cargo clippy --manifest-path src-tauri\Cargo.toml --all-targets -- -D warnings
node --check scripts\desktop-smoke.mjs
node scripts\复制无刷新回归.mjs
node scripts\临时收集回归.mjs
node scripts\同步冲突界面回归.mjs
```

## 生成安装包

先提交当前源码，随后执行：

```powershell
pwsh -ExecutionPolicy Bypass -File scripts\生成安装包.ps1
```

脚本会自动完成专项回归、Rust 门禁、Windows x64 NSIS 构建、安装包复制、独立 EXE 启动冒烟、SHA-256 核对和发布说明更新。脚本不会安装应用、提交 Git、推送远端或清理 `.local` 缓存。

当前发布候选已经通过：

- Rust 74 项测试。
- `cargo fmt` 和 Clippy 零警告。
- 前端语法、复制无刷新、临时收集和同步冲突界面专项回归。
- Windows x64 NSIS 构建、PE 架构与体积门禁。
- 独立 EXE 窗口响应和 WebView2 启动冒烟。

安装、升级、冲突窗口实际选择、同步期间窗口拖动和卸载数据保留仍需在实际安装环境中手工验收。

## 维护约束

项目保持轻量、单机和本地优先。除非有明确需求，不加入后台同步、静默替用户决定真实冲突、强制推送、命令执行、搜索、应用内 GitHub 登录、Token 管理、多人协作或其他 AI 提供方。

后续由 AI 维护时，先阅读 [AGENTS.md](AGENTS.md)。
