<#
.SYNOPSIS
    kugou-api-install —— 拉取并配置 kugou-tui 依赖的第三方接口服务。

.DESCRIPTION
    对应 Unix 侧的 `scripts/kugou-api-install`，但**只做安装，不做启动**：
    启动交给 `kugou-tui.ps1`——它每次开播前都会探活、没起就自己拉起来。
    这样「怎么起服务」只有一处实现，不会两边逻辑漂移。

    本项目不含任何接口实现，数据全部来自第三方服务：
      kugou   → https://github.com/MakcRe/KuGouMusicApi
      netease → https://github.com/neteasecloudmusicapienhanced/api-enhanced

.PARAMETER Source
    要装哪个音源：`kugou`（默认）或 `netease`。

.PARAMETER Dir
    安装目录。默认 `$env:USERPROFILE\KuGouMusicApi`（网易云是
    `NeteaseCloudMusicApi`），也可以用环境变量 `KUGOU_API_DIR` / `NETEASE_API_DIR`
    指定——那两个变量 `kugou-tui.ps1` 也认，两边保持一致。

.EXAMPLE
    .\scripts\kugou-api-install.ps1
    .\scripts\kugou-api-install.ps1 netease

.EXAMPLE
    .\scripts\kugou-api-install.ps1 -Dir D:\apps\KuGouMusicApi
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('kugou', 'netease')]
    [string]$Source = 'kugou',

    [string]$Dir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 仓库地址与固定提交。
#
# `kugou` 钉死在一个验证过的提交上：上游是活跃仓库，接口字段会变，跟 master
# 可能某天就解析不出歌名或歌词。这个 SHA 必须和 `scripts/kugou-api-install`
# （bash 版）里的 `PINNED[kugou]` 一致——**一处钉、一处跟 master 是最坏的组合**。
#
# 网易云那份原 Binaryify/NeteaseCloudMusicApi 已因版权问题停止维护（仓库只剩占位
# 文件），这里用社区持续维护的分支版本；它没有稳定提交可钉，跟默认分支。
$repos = @{
    kugou   = 'https://github.com/MakcRe/KuGouMusicApi.git'
    netease = 'https://github.com/neteasecloudmusicapienhanced/api-enhanced.git'
}
$pinned = @{
    kugou   = 'a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e'
    netease = ''
}
$dirNames = @{
    kugou   = 'KuGouMusicApi'
    netease = 'NeteaseCloudMusicApi'
}
$portLabels = @{
    kugou   = '3000（标准版）/ 3001（概念版）'
    netease = '3002'
}

if (-not $Dir) {
    $fromEnv = if ($Source -eq 'netease') { $env:NETEASE_API_DIR } else { $env:KUGOU_API_DIR }
    if ($fromEnv) {
        $Dir = $fromEnv
    }
    else {
        $homeDir = if ($env:USERPROFILE) {
            $env:USERPROFILE
        }
        else {
            [Environment]::GetFolderPath('UserProfile')
        }
        $Dir = Join-Path $homeDir $dirNames[$Source]
    }
}

$repo = $repos[$Source]
$pin = $pinned[$Source]
$entry = Join-Path $Dir 'app.js'

Write-Host "== $Source ==" -ForegroundColor Cyan
Write-Host "  目录：$Dir"

foreach ($tool in 'git', 'node', 'npm') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Write-Host "  找不到 $tool。git 用 Git for Windows，node/npm 用 https://nodejs.org" -ForegroundColor Red
        exit 1
    }
}

# 判断「装好了没有」要看入口文件，不能只看目录在不在——
# clone 中断、只拉到 README 之类的情况都会留下一个空壳目录。
if (Test-Path $entry) {
    Write-Host '  已安装，跳过 clone 与依赖安装。' -ForegroundColor Green

    # 已经装好不代表装在对的提交上：之前跟 master 装的人会停在一个未经验证的
    # 版本上。这里只提示、不自动切——用户可能正依赖当前版本的行为。
    $gitDir = Join-Path $Dir '.git'
    if ($pin -and (Test-Path $gitDir)) {
        $head = (& git -C $Dir rev-parse HEAD 2>$null | Select-Object -First 1)
        if ($head -and $head.Trim() -ne $pin) {
            Write-Host "  注意：当前停在 $($head.Trim().Substring(0, 7))，脚本钉的是 $($pin.Substring(0, 7))" -ForegroundColor Yellow
            Write-Host '  若遇到「解析不出歌名/歌词」，先切过去试试：'
            Write-Host "    git -C `"$Dir`" fetch --depth 1 origin $pin"
            Write-Host "    git -C `"$Dir`" checkout FETCH_HEAD"
        }
    }
}
elseif ((Test-Path $Dir) -and (Get-ChildItem -Force $Dir | Select-Object -First 1)) {
    Write-Host "  目录已存在但没有 app.js，疑似安装不完整：$Dir" -ForegroundColor Red
    Write-Host '  确认无误后手动删掉该目录再重试。'
    exit 1
}
else {
    # 空目录（或不存在）：清掉再建，避免 git 报「目标已存在」。
    if (Test-Path $Dir) { Remove-Item -Force -Recurse $Dir }

    Write-Host "  克隆 $repo"
    Write-Host "  → $Dir"
    if ($pin) { Write-Host "  固定到 $($pin.Substring(0, 7))" }

    # 不用 `git clone --depth 1`：它只拿默认分支的 tip，钉不住提交。
    # `git fetch --depth 1 origin <sha>` 只拉那一个提交，既准确又不比全量慢
    # （GitHub 支持按 SHA 取）。
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $fetchRef = if ($pin) { $pin } else { 'HEAD' }
    & git -C $Dir init -q
    & git -C $Dir remote add origin $repo
    & git -C $Dir fetch -q --depth 1 origin $fetchRef
    if ($LASTEXITCODE -ne 0) {
        Write-Host '  克隆失败。' -ForegroundColor Red
        exit 1
    }
    & git -C $Dir checkout -q FETCH_HEAD
    if ($LASTEXITCODE -ne 0) {
        Write-Host '  检出失败。' -ForegroundColor Red
        exit 1
    }

    Write-Host '  安装依赖（首次约需一两分钟）…'
    # `--omit=dev`：服务运行时是 `node app.js`，devDependencies 全是开发工具
    # （nodemon / typescript / pkg / prettier…），一个都用不到。实测装全量是 311 个包、
    # 只装生产依赖是 120 个——差的那一半纯粹是白等、白占磁盘。
    Push-Location $Dir
    try {
        & npm install --omit=dev
        if ($LASTEXITCODE -ne 0) {
            Write-Host '  依赖安装失败。' -ForegroundColor Red
            exit 1
        }
    }
    finally {
        Pop-Location
    }
}

Write-Host ''
Write-Host '装好了。端口：' -NoNewline
Write-Host $portLabels[$Source] -ForegroundColor Green
Write-Host ''
Write-Host '下一步不用手动起服务——直接跑播放器，它会自己探活并在需要时拉起：'
Write-Host "  .\scripts\kugou-tui.ps1"
