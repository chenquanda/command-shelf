#requires -Version 7.0

<#
.SYNOPSIS
一键生成并落位 CommandShelf Windows x64 安装包。

.DESCRIPTION
文件职责：把临时收集回归、发布候选构建、安装包复制、桌面启动冒烟和发布哈希更新串成一次命令。
主要内容：调用现有 release-candidate.ps1 生成可追溯候选，把通过验证的 NSIS 安装包复制到 release，
启动同批次独立 EXE 检查窗口与 WebView2，并更新项目说明中的当前安装包 SHA-256。
重要约束：默认构建仍要求 Git 工作树可归因于确定提交；脚本不会安装应用、提交 Git、推送远端或清理 .local 缓存。

.PARAMETER ReuseExistingCandidate
跳过耗时构建，复用 `.local/release-evidence/release-candidate.json` 指向的现有候选。
仅用于验证本脚本的复制、冒烟和文档更新阶段；日常正式发布不要使用。

.EXAMPLE
pwsh -ExecutionPolicy Bypass -File scripts\生成安装包.ps1

.EXAMPLE
pwsh -ExecutionPolicy Bypass -File scripts\生成安装包.ps1 -ReuseExistingCandidate
#>
[CmdletBinding()]
param(
    [Parameter()]
    [switch]$ReuseExistingCandidate
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

<#
.SYNOPSIS
执行原生命令，并在非零退出码时终止整个安装包生成流程。

.PARAMETER Label
面向用户显示的步骤名称。

.PARAMETER FilePath
直接执行的程序名或绝对路径。

.PARAMETER Arguments
逐项传递给原生命令的参数，避免 Shell 字符串拼接。
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
读取并校验发布候选证据，确保安装包和独立 EXE 均存在。

.PARAMETER EvidencePath
release-candidate.ps1 生成的 JSON 证据绝对路径。

.OUTPUTS
已解析并通过基本结构校验的发布候选对象。
#>
function Get-ReleaseEvidence {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$EvidencePath
    )

    if (-not (Test-Path -LiteralPath $EvidencePath -PathType Leaf)) {
        throw "未找到发布候选证据：$EvidencePath。请先执行正式构建。"
    }

    $evidence = Get-Content -Raw -LiteralPath $EvidencePath | ConvertFrom-Json
    foreach ($propertyPath in @(
        $evidence.artifacts.application.path,
        $evidence.artifacts.installer.path
    )) {
        if ([string]::IsNullOrWhiteSpace($propertyPath) -or -not (Test-Path -LiteralPath $propertyPath -PathType Leaf)) {
            throw "发布候选证据指向的产物不存在：$propertyPath。"
        }
    }

    return $evidence
}

<#
.SYNOPSIS
启动同批次独立 EXE，验证主窗口响应和 WebView2 子进程创建。

.PARAMETER ApplicationPath
发布候选独立 EXE 的绝对路径。

.OUTPUTS
包含窗口标题、响应状态和新增 WebView2 进程数量的冒烟证据。
#>
function Test-DesktopStartup {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ApplicationPath
    )

    $edgeBefore = @(
        Get-Process -Name "msedgewebview2" -ErrorAction SilentlyContinue |
            Select-Object -ExpandProperty Id
    )
    $process = Start-Process -FilePath $ApplicationPath -PassThru -WindowStyle Hidden
    try {
        $deadline = (Get-Date).AddSeconds(15)
        $newEdge = @()
        do {
            Start-Sleep -Milliseconds 250
            $process.Refresh()
            $newEdge = @(
                Get-Process -Name "msedgewebview2" -ErrorAction SilentlyContinue |
                    Where-Object { $_.Id -notin $edgeBefore }
            )
        } while (
            (Get-Date) -lt $deadline -and
            -not $process.HasExited -and
            ($process.MainWindowHandle -eq 0 -or $newEdge.Count -eq 0)
        )

        $result = [pscustomobject]@{
            processExited = $process.HasExited
            responding = if ($process.HasExited) { $false } else { $process.Responding }
            mainWindowHandle = if ($process.HasExited) { 0 } else { [int64]$process.MainWindowHandle }
            mainWindowTitle = if ($process.HasExited) { "" } else { $process.MainWindowTitle }
            newWebView2Processes = $newEdge.Count
        }

        if (
            $result.processExited -or
            -not $result.responding -or
            $result.mainWindowHandle -eq 0 -or
            $result.mainWindowTitle -ne "CommandShelf" -or
            $result.newWebView2Processes -eq 0
        ) {
            throw "桌面启动冒烟失败：$($result | ConvertTo-Json -Compress)。"
        }

        return $result
    }
    finally {
        # 只终止本脚本启动的候选进程，不影响用户已经安装或正在使用的 CommandShelf。
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        }
    }
}

<#
.SYNOPSIS
把说明文档中的第一处“安装包 SHA-256”更新为本次实际哈希。

.PARAMETER Path
需要更新的 UTF-8 文档绝对路径。

.PARAMETER Hash
本次 release 目录安装包的 64 位大写 SHA-256。
#>
function Update-InstallerHashDocument {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [ValidatePattern("^[A-F0-9]{64}$")]
        [string]$Hash
    )

    $content = [System.IO.File]::ReadAllText($Path)
    # 限制匹配范围在标题后的 32 个非十六进制字符内，避免误改文档中的其他校验值。
    $pattern = [regex]::new("(?s)(安装包 SHA-256[^A-F0-9]{0,32})[A-F0-9]{64}")
    if (-not $pattern.IsMatch($content)) {
        throw "文档中未找到可更新的安装包 SHA-256：$Path。"
    }
    $updated = $pattern.Replace(
        $content,
        [System.Text.RegularExpressions.MatchEvaluator]{
            param($match)
            return $match.Groups[1].Value + $Hash
        },
        1
    )
    [System.IO.File]::WriteAllText($Path, $updated, [System.Text.UTF8Encoding]::new($false))
}

$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$releaseCandidateScript = Join-Path $PSScriptRoot "release-candidate.ps1"
$inboxRegressionScript = Join-Path $PSScriptRoot "临时收集回归.mjs"
$evidencePath = Join-Path $repositoryRoot ".local\release-evidence\release-candidate.json"
$releaseDirectory = Join-Path $repositoryRoot "release"
$destinationInstaller = Join-Path $releaseDirectory "CommandShelf_0.1.0_x64-setup.exe"
$documentsWithInstallerHash = @(
    (Join-Path $repositoryRoot "AGENTS.md"),
    (Join-Path $repositoryRoot "项目说明.md"),
    (Join-Path $repositoryRoot "docs\安装与使用说明.md")
)

Push-Location -LiteralPath $repositoryRoot
try {
    if (-not $ReuseExistingCandidate) {
        Invoke-CheckedCommand -Label "临时收集专项回归" -FilePath "node" -Arguments @($inboxRegressionScript)
        Invoke-CheckedCommand -Label "正式发布候选构建" -FilePath "pwsh" -Arguments @(
            "-NoProfile",
            "-ExecutionPolicy", "Bypass",
            "-File", $releaseCandidateScript
        )
    }
    else {
        Write-Host "`n==> 复用现有发布候选，仅验证构建后的自动落位流程" -ForegroundColor Yellow
    }

    $evidence = Get-ReleaseEvidence -EvidencePath $evidencePath
    [void](New-Item -ItemType Directory -Path $releaseDirectory -Force)
    Copy-Item -LiteralPath $evidence.artifacts.installer.path -Destination $destinationInstaller -Force

    $destinationHash = (Get-FileHash -LiteralPath $destinationInstaller -Algorithm SHA256).Hash
    if ($destinationHash -ne $evidence.artifacts.installer.sha256) {
        throw "release 安装包与发布候选哈希不一致，复制结果已拒绝。"
    }

    Write-Host "`n==> 独立 EXE 启动冒烟" -ForegroundColor Cyan
    $startupEvidence = Test-DesktopStartup -ApplicationPath $evidence.artifacts.application.path

    foreach ($documentPath in $documentsWithInstallerHash) {
        Update-InstallerHashDocument -Path $documentPath -Hash $destinationHash
    }

    $installer = Get-Item -LiteralPath $destinationInstaller
    $result = [ordered]@{
        status = "passed"
        sourceCommit = $evidence.gitCommit
        installerPath = $installer.FullName
        installerSizeBytes = $installer.Length
        installerSha256 = $destinationHash
        startup = $startupEvidence
        documentationUpdated = $documentsWithInstallerHash
        reusedExistingCandidate = [bool]$ReuseExistingCandidate
    }

    Write-Host "`n安装包已生成并通过验证：" -ForegroundColor Green
    $result | ConvertTo-Json -Depth 5
}
finally {
    Pop-Location
}
