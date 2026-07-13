#requires -Version 7.0

<#
.SYNOPSIS
构建并校验 CommandShelf Windows x64 发布候选。

.DESCRIPTION
文件职责：以失败即停的方式执行源码门禁、NSIS 构建、产物架构与体积检查，并生成可追溯的 JSON 证据。
主要内容：Rust 与前端静态检查、Windows x64 Release 构建、EXE/安装包唯一性检查、SHA-256 与工具版本归档。
重要约束：脚本不会安装应用、修改用户配置、访问远端仓库或清理调用方已有目录；性能与安装验收由后续独占步骤执行。

.PARAMETER TargetDirectory
Cargo 构建输出目录。相对路径按仓库根目录解析；脚本会创建目录但不会删除其中的既有内容。

.PARAMETER EvidenceDirectory
发布证据输出目录。相对路径按仓库根目录解析；成功后写入 release-candidate.json。
#>
[CmdletBinding()]
param(
    [Parameter()]
    [string]$TargetDirectory = (Join-Path $env:TEMP "command-shelf-release-target"),

    [Parameter()]
    [string]$EvidenceDirectory = (Join-Path $env:TEMP "command-shelf-release-evidence")
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

<#
.SYNOPSIS
将调用方路径转换为绝对目录并确保其存在。

.PARAMETER Path
调用方提供的绝对路径或相对于仓库根目录的路径。

.PARAMETER RepositoryRoot
当前 CommandShelf 仓库根目录。

.OUTPUTS
已创建且规范化的绝对目录路径。
#>
function Resolve-OutputDirectory {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [string]$RepositoryRoot
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "输出目录不能为空。"
    }

    $candidate = if ([System.IO.Path]::IsPathRooted($Path)) {
        $Path
    }
    else {
        Join-Path $RepositoryRoot $Path
    }

    $absolutePath = [System.IO.Path]::GetFullPath($candidate)
    [void](New-Item -ItemType Directory -Path $absolutePath -Force)
    return $absolutePath
}

<#
.SYNOPSIS
执行一个原生命令并在非零退出码时立即终止发布流程。

.PARAMETER Label
面向用户显示的门禁或构建步骤名称。

.PARAMETER FilePath
直接执行的程序名或绝对路径；不会经由 Shell 拼接。

.PARAMETER Arguments
逐项传递给原生程序的参数，避免命令注入和空格路径歧义。

.OUTPUTS
无返回值；命令失败时抛出包含步骤名和退出码的异常。
#>
function Invoke-CheckedCommand {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$Label,

        [Parameter(Mandatory)]
        [string]$FilePath,

        [Parameter()]
        [string[]]$Arguments = @()
    )

    Write-Host "`n==> $Label" -ForegroundColor Cyan
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Label 失败，退出码：$LASTEXITCODE。"
    }
}

<#
.SYNOPSIS
执行只读版本查询并返回单行文本，供发布证据记录实际工具链。

.PARAMETER FilePath
需要查询版本的程序名。

.PARAMETER Arguments
版本查询参数。

.OUTPUTS
去除首尾空白后的完整版本文本。
#>
function Get-CommandVersion {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$FilePath,

        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    $output = & $FilePath @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "无法读取 $FilePath 的版本，退出码：$LASTEXITCODE。"
    }

    return (($output | ForEach-Object { $_.ToString() }) -join "`n").Trim()
}

<#
.SYNOPSIS
从候选集合中取得唯一文件，避免把旧产物或错误架构误记为本次发布结果。

.PARAMETER Candidates
当前构建目录中匹配到的文件集合。

.PARAMETER Description
发生缺失或重复时用于错误说明的产物名称。

.OUTPUTS
唯一匹配的 FileInfo。
#>
function Get-UniqueArtifact {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [AllowEmptyCollection()]
        [System.IO.FileInfo[]]$Candidates,

        [Parameter(Mandatory)]
        [string]$Description
    )

    if ($Candidates.Count -ne 1) {
        $paths = if ($Candidates.Count -eq 0) {
            "未找到"
        }
        else {
            ($Candidates.FullName -join "；")
        }
        throw "$Description 必须且只能有一个候选，当前数量为 $($Candidates.Count)：$paths。"
    }

    return $Candidates[0]
}

<#
.SYNOPSIS
验证 Windows 可执行文件的 DOS、PE 签名与 Machine 字段确实为 AMD64。

.PARAMETER Path
待检查的应用 EXE 绝对路径。

.OUTPUTS
无返回值；格式损坏或不是 x64 时抛出异常。
#>
function Assert-PeX64 {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $stream = [System.IO.File]::OpenRead($Path)
    $reader = [System.IO.BinaryReader]::new($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) {
            throw "应用 EXE 缺少 MZ 签名：$Path。"
        }

        $stream.Position = 0x3C
        $peOffset = $reader.ReadInt32()
        if ($peOffset -lt 0x40 -or $peOffset -gt ($stream.Length - 6)) {
            throw "应用 EXE 的 PE 头偏移无效：$Path。"
        }

        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) {
            throw "应用 EXE 缺少 PE 签名：$Path。"
        }

        $machine = $reader.ReadUInt16()
        if ($machine -ne 0x8664) {
            throw ("应用 EXE 不是 Windows x64，Machine=0x{0:X4}：{1}。" -f $machine, $Path)
        }
    }
    finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}

<#
.SYNOPSIS
校验文件不超过发布门槛，并返回便于写入证据的 MiB 数值。

.PARAMETER Artifact
待检查的发布产物。

.PARAMETER MaximumMiB
允许的最大二进制兆字节数。

.PARAMETER Description
错误信息使用的产物名称。

.OUTPUTS
四舍五入到三位小数的实际 MiB。
#>
function Assert-ArtifactSize {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [System.IO.FileInfo]$Artifact,

        [Parameter(Mandatory)]
        [double]$MaximumMiB,

        [Parameter(Mandatory)]
        [string]$Description
    )

    $sizeMiB = $Artifact.Length / 1MB
    if ($sizeMiB -gt $MaximumMiB) {
        throw ("{0} 体积 {1:N3} MiB 超过 {2:N3} MiB 门槛：{3}。" -f $Description, $sizeMiB, $MaximumMiB, $Artifact.FullName)
    }

    return [Math]::Round($sizeMiB, 3)
}

<#
.SYNOPSIS
确认发布候选来自没有未提交、暂存或未跟踪文件的确定 Git 工作树。

.PARAMETER RepositoryRoot
需要检查的 CommandShelf 仓库根目录。

.OUTPUTS
无返回值；发现任何未记录源码时立即终止，避免把产物错误归因给 HEAD。
#>
function Assert-CleanGitWorktree {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$RepositoryRoot
    )

    & git -C $RepositoryRoot diff --quiet --exit-code
    if ($LASTEXITCODE -ne 0) {
        throw "发布工作树包含未提交修改，请先保存到本地 Git。"
    }

    & git -C $RepositoryRoot diff --cached --quiet --exit-code
    if ($LASTEXITCODE -ne 0) {
        throw "发布工作树包含已暂存但未提交的修改，请先完成本地提交。"
    }

    $untrackedFiles = @(& git -C $RepositoryRoot ls-files --others --exclude-standard)
    if ($LASTEXITCODE -ne 0) {
        throw "无法检查发布工作树中的未跟踪文件。"
    }
    if ($untrackedFiles.Count -gt 0) {
        throw "发布工作树包含未跟踪文件：$($untrackedFiles -join '；')。"
    }
}

$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$resolvedTargetDirectory = Resolve-OutputDirectory -Path $TargetDirectory -RepositoryRoot $repositoryRoot
$resolvedEvidenceDirectory = Resolve-OutputDirectory -Path $EvidenceDirectory -RepositoryRoot $repositoryRoot
$manifestPath = Join-Path $repositoryRoot "src-tauri\Cargo.toml"
$tauriConfigPath = Join-Path $repositoryRoot "src-tauri\tauri.conf.json"
$frontendPath = Join-Path $repositoryRoot "frontend\index.html"
$desktopSmokePath = Join-Path $repositoryRoot "scripts\desktop-smoke.mjs"
$targetTriple = "x86_64-pc-windows-msvc"
$previousCargoTargetDirectory = $env:CARGO_TARGET_DIR
$locationPushed = $false

try {
    # 固定工作目录可阻止 Tauri CLI 在多 worktree 上级目录误选兄弟项目；finally 负责恢复调用方位置。
    Push-Location -LiteralPath $repositoryRoot
    $locationPushed = $true

    # 所有 Cargo 步骤共享调用方指定目录，避免测试和正式构建重复编译同一依赖。
    $env:CARGO_TARGET_DIR = $resolvedTargetDirectory
    Assert-CleanGitWorktree -RepositoryRoot $repositoryRoot
    $gitCommitBefore = (& git -C $repositoryRoot rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "无法读取构建前 Git 提交。"
    }

    Invoke-CheckedCommand -Label "Rust 格式检查" -FilePath "cargo" -Arguments @("fmt", "--manifest-path", $manifestPath, "--check")
    Invoke-CheckedCommand -Label "Rust 全目标测试" -FilePath "cargo" -Arguments @("test", "--manifest-path", $manifestPath, "--all-targets")
    Invoke-CheckedCommand -Label "Rust Clippy 零警告检查" -FilePath "cargo" -Arguments @("clippy", "--manifest-path", $manifestPath, "--all-targets", "--", "-D", "warnings")
    Invoke-CheckedCommand -Label "桌面验收脚本语法检查" -FilePath "node" -Arguments @("--check", $desktopSmokePath)

    # 直接编译 HTML 中每个非外链脚本；不执行页面代码，也不需要额外前端工具链。
    $inlineScriptCheck = @'
const fs = require("node:fs");
const path = process.argv.at(-1);
const html = fs.readFileSync(path, "utf8");
const scripts = [...html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/gi)];
if (scripts.length === 0) throw new Error("未找到可检查的内联脚本");
scripts.forEach((match, index) => {
  new Function(match[1]);
  process.stdout.write(`inline-script-${index + 1}: ok\n`);
});
'@
    Invoke-CheckedCommand -Label "前端内联脚本语法检查" -FilePath "node" -Arguments @("-e", $inlineScriptCheck, $frontendPath)

    Invoke-CheckedCommand -Label "Windows x64 NSIS 发布构建" -FilePath "cargo" -Arguments @(
        "tauri", "build",
        "--target", $targetTriple,
        "--bundles", "nsis",
        "--ci"
    )

    $releaseDirectory = Join-Path $resolvedTargetDirectory "$targetTriple\release"
    $applicationCandidates = @(
        Get-ChildItem -LiteralPath $releaseDirectory -File -Filter "command-shelf.exe" -ErrorAction SilentlyContinue
    )
    $installerDirectory = Join-Path $releaseDirectory "bundle\nsis"
    $tauriConfig = Get-Content -Raw -LiteralPath $tauriConfigPath | ConvertFrom-Json
    $installerFileName = "$($tauriConfig.productName)_$($tauriConfig.version)_x64-setup.exe"
    $installerCandidates = @(
        Get-ChildItem -LiteralPath $installerDirectory -File -Filter $installerFileName -ErrorAction SilentlyContinue
    )
    $application = Get-UniqueArtifact -Candidates $applicationCandidates -Description "CommandShelf 应用 EXE"
    $installer = Get-UniqueArtifact -Candidates $installerCandidates -Description "CommandShelf NSIS 安装包"

    Assert-PeX64 -Path $application.FullName
    $applicationSizeMiB = Assert-ArtifactSize -Artifact $application -MaximumMiB 20 -Description "应用 EXE"
    $installerSizeMiB = Assert-ArtifactSize -Artifact $installer -MaximumMiB 10 -Description "NSIS 安装包"

    $gitCommit = (& git -C $repositoryRoot rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "无法读取当前 Git 提交，发布证据未生成。"
    }
    if ($gitCommit -ne $gitCommitBefore) {
        throw "构建期间 Git HEAD 已变化，拒绝生成归属不明的发布证据。"
    }
    Assert-CleanGitWorktree -RepositoryRoot $repositoryRoot

    # 证据只记录本次成功构建的确定产物；路径保留绝对值，便于本机安装验收直接使用。
    $evidence = [ordered]@{
        schemaVersion = 1
        generatedAtUtc = [DateTime]::UtcNow.ToString("o")
        repositoryRoot = $repositoryRoot
        gitCommit = $gitCommit
        workingTreeClean = $true
        targetTriple = $targetTriple
        gates = @(
            "cargo fmt --check",
            "cargo test --all-targets",
            "cargo clippy --all-targets -- -D warnings",
            "desktop-smoke.mjs 语法",
            "frontend/index.html 内联脚本语法",
            "cargo tauri build --bundles nsis",
            "Git 工作树干净且构建前后 HEAD 一致"
        )
        tools = [ordered]@{
            rustc = Get-CommandVersion -FilePath "rustc" -Arguments @("--version")
            cargo = Get-CommandVersion -FilePath "cargo" -Arguments @("--version")
            node = Get-CommandVersion -FilePath "node" -Arguments @("--version")
            git = Get-CommandVersion -FilePath "git" -Arguments @("--version")
        }
        artifacts = [ordered]@{
            application = [ordered]@{
                path = $application.FullName
                sizeBytes = $application.Length
                sizeMiB = $applicationSizeMiB
                maximumMiB = 20
                sha256 = (Get-FileHash -LiteralPath $application.FullName -Algorithm SHA256).Hash
                peMachine = "AMD64 (0x8664)"
            }
            installer = [ordered]@{
                path = $installer.FullName
                sizeBytes = $installer.Length
                sizeMiB = $installerSizeMiB
                maximumMiB = 10
                sha256 = (Get-FileHash -LiteralPath $installer.FullName -Algorithm SHA256).Hash
            }
        }
    }

    $evidencePath = Join-Path $resolvedEvidenceDirectory "release-candidate.json"
    $evidence | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $evidencePath -Encoding utf8NoBOM
    Write-Host "`n发布候选已通过：$evidencePath" -ForegroundColor Green
    $evidence
}
finally {
    # 工作目录和环境变量都属于调用方进程状态，必须在成功和异常两条路径恢复。
    if ($locationPushed) {
        Pop-Location
    }
    if ($null -eq $previousCargoTargetDirectory) {
        Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    }
    else {
        $env:CARGO_TARGET_DIR = $previousCargoTargetDirectory
    }
}
