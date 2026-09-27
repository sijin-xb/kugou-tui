<#
.SYNOPSIS
    kugou-tui —— Windows 构建 + 打包。

.DESCRIPTION
    对应 Unix 侧的 `scripts/make-release-tarball`，只是产物换成 zip。

    为什么要单独写一个：仓库里其余脚本（`release`、`make-release-tarball`、
    `kugou-tui` 启动器）都是 bash，Windows 上默认跑不了。构建本身两条命令就够
    （`cargo build --release`），真正容易漏的是**打包**——发行包里必须同时带上
    那三个 bash 脚本和 `docs/`，否则拿到包的人配不起接口服务。手工拼目录漏东西
    是常态（0.3.7 那版就漏了），所以固化成一个脚本。

    三个 bash 脚本在 Windows 上不是给 PowerShell 用的，而是给 Git Bash / WSL 用的
    ——那两种环境里 `kugou-api` 之类照常能跑。包里带上它们不花什么代价，
    少了却会让一部分用户直接卡住。

    **在 PowerShell 5.1（Windows 自带的那版）上也能跑**，不需要额外装 pwsh 7。
    所以这里刻意避开 `$IsWindows`（6.0 才有），改用 `$env:OS`。

.PARAMETER Target
    目标三元组。省略时取 `rustc` 报的宿主三元组（Windows 上一般是
    `x86_64-pc-windows-msvc`）。交叉编译时显式传，例如
    `-Target aarch64-pc-windows-msvc`。

.PARAMETER SkipBuild
    跳过 `cargo build`，直接拿现成的产物打包。

.EXAMPLE
    .\scripts\build-windows.ps1

.EXAMPLE
    .\scripts\build-windows.ps1 -Target aarch64-pc-windows-msvc

.EXAMPLE
    # 只打包、不重新编译（CI 里已经构建过时用）
    .\scripts\build-windows.ps1 -SkipBuild
#>
[CmdletBinding()]
param(
    [string]$Target,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 可执行文件后缀。用 $env:OS 而不是 $IsWindows：后者是 PowerShell 6+ 才有的自动变量，
# 而 Windows 自带的是 5.1——构建脚本逼用户先装一个 pwsh 7 说不过去。
$exeSuffix = if ($env:OS -eq 'Windows_NT') { '.exe' } else { '' }

# 仓库根目录：脚本在 scripts\ 下，往上一级。
# 不依赖当前工作目录——用户十有八九是在别处开的终端。
$repoDir = Split-Path -Parent $PSScriptRoot
Push-Location $repoDir
try {
    # ---------- 1. 前置检查 ----------
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw '找不到 cargo。先装 Rust 工具链：https://rustup.rs'
    }

    # 版本号从 Cargo.toml 读，避免和它不一致（发版时只改一个地方）。
    $manifest = Get-Content -Raw (Join-Path $repoDir 'Cargo.toml')
    $version = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
    if (-not $version) { throw '读不到 Cargo.toml 里的版本号' }

    if (-not $Target) {
        $Target = (rustc -vV | Select-String '^host:\s*(.+)$').Matches.Groups[1].Value.Trim()
    }
    if (-not $Target) { throw '拿不到目标三元组，用 -Target 显式传一个' }

    Write-Host "版本：$version" -ForegroundColor Cyan
    Write-Host "目标：$Target" -ForegroundColor Cyan

    # ---------- 2. 构建 ----------
    if (-not $SkipBuild) {
        cargo build --release --target $Target
        if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败' }
    }

    # 产物路径。
    #
    # 两条都要看：cargo 只在**显式传了 `--target`** 时才把产物放进
    # `target\<三元组>\release\`；不传就是 `target\release\`。
    # 本脚本自己构建时一律带 `--target`（走上面那条），但 `-SkipBuild` 时用户
    # 很可能是先跑的 `cargo build --release`（走下面那条）——只认一条会让人
    # 对着一个确实存在的 exe 收到「找不到」。
    $candidates = @(
        (Join-Path $repoDir 'target' | Join-Path -ChildPath $Target |
            Join-Path -ChildPath 'release' | Join-Path -ChildPath "kugou-tui$exeSuffix"),
        (Join-Path $repoDir 'target' | Join-Path -ChildPath 'release' |
            Join-Path -ChildPath "kugou-tui$exeSuffix")
    )
    $bin = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $bin) {
        throw ("找不到可执行文件，试过：`n  " + ($candidates -join "`n  ") +
            "`n先跑：cargo build --release --target $Target")
    }
    if ($bin -ne $candidates[0]) {
        Write-Host "注意：用的是 target\release\ 下的产物（没带 --target 构建的那份），" -ForegroundColor Yellow
        Write-Host "      假定它就是 $Target 的产物。" -ForegroundColor Yellow
    }

    # ---------- 3. 打包 ----------
    $name  = "kugou-tui-$version-$Target"
    $dist  = Join-Path $repoDir 'dist'
    $stage = Join-Path $dist $name
    $zip   = Join-Path $dist "$name.zip"

    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    $stageScripts = Join-Path $stage 'scripts'
    $stageDocs = Join-Path $stage 'docs'
    New-Item -ItemType Directory -Force -Path $stageScripts | Out-Null
    New-Item -ItemType Directory -Force -Path $stageDocs | Out-Null

    Copy-Item $bin (Join-Path $stage "kugou-tui$exeSuffix")
    # 三个 bash 脚本：Git Bash / WSL 下仍然要用，见文件头说明。
    $srcScripts = Join-Path $repoDir 'scripts'
    foreach ($script in 'kugou-api', 'kugou-api-install', 'kugou-tui') {
        Copy-Item (Join-Path $srcScripts $script) $stageScripts
    }
    foreach ($file in 'LICENSE', 'README.md', 'CHANGELOG.md', 'KEYBINDINGS.md') {
        Copy-Item (Join-Path $repoDir $file) $stage
    }
    Copy-Item (Join-Path (Join-Path $repoDir 'docs') '*.md') $stageDocs

    # Compress-Archive 不认已存在的目标，先删。
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
    Remove-Item -Recurse -Force $stage

    # ---------- 4. 汇报 ----------
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    $size = [math]::Round((Get-Item $zip).Length / 1MB, 2)

    Write-Host ''
    Write-Host "产出：dist\$name.zip（$size MiB）" -ForegroundColor Green
    Write-Host "SHA256：$hash"
    Write-Host ''
    Write-Host '内容：'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
    try {
        foreach ($entry in $archive.Entries) { Write-Host "  $($entry.FullName)" }
    }
    finally {
        $archive.Dispose()
    }
}
finally {
    Pop-Location
}
