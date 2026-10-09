<#
.SYNOPSIS
    kugou-tui —— Windows 启动器：需要本机接口服务时确保它在跑，然后进入播放器。

.DESCRIPTION
    对应 Unix 侧的 `scripts/kugou-tui`。

    为什么需要它：kugou-tui 只是客户端，数据全来自 KuGouMusicApi。服务没起时界面能开
    但什么都搜不到，容易让人以为程序坏了。这里把「检查 → 拉起 → 等待就绪」收进一条命令；
    服务已经在跑时它只是一次本地探测，开销可忽略。

    但它**只在需要服务时才做这些**。默认后端 `native` 把酷狗接口实现在进程内，本机既
    不需要 KuGouMusicApi、也不需要 Node.js——启动器认 `api_backend`（以及 `--api` /
    `KUGOU_API_BACKEND`），是 `native` 就直接进播放器，不探端口、不拉进程。

.PARAMETER 其余参数
    全部原样透传给播放器：`-s 海阔天空`、`-a http://127.0.0.1:3001`、`--api node`、
    `--volume 50`……

    刻意**不写 `param()` 块**：一旦声明了参数，PowerShell 就会把不认识的 `-s` 当成
    写错的参数名直接报「找不到匹配的参数」。没有 `param()` 时所有参数都进 `$args`，
    正好是要的行为。

.PARAMETER DryRun
    只打印「将要做什么」，不启动任何进程、也不进播放器。排查「为什么它说服务没起」时用。

.EXAMPLE
    .\scripts\kugou-tui.ps1

.EXAMPLE
    .\scripts\kugou-tui.ps1 -s 海阔天空

.EXAMPLE
    .\scripts\kugou-tui.ps1 --dry-run
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# 关掉 `Invoke-WebRequest` 的进度条。它默认会往控制台刷「Reading web response
# stream…」，而这是个 TUI 播放器的启动器——探活那点响应体不值得占一行输出。
$ProgressPreference = 'SilentlyContinue'

# ==================================================================
# 参数：摘掉脚本自己的开关，其余原样透传（见 .PARAMETER 说明）
# ==================================================================
$dryRun = $false
$playerArgs = @()

# 顺带记下 `--api`（或 `KUGOU_API_BACKEND`）请求的后端：它决定本机到底还需不需要
# Node 服务。参数本身仍然原样透传给播放器，这里只是**顺便看一眼**。
$requestedBackend = if ($env:KUGOU_API_BACKEND) { $env:KUGOU_API_BACKEND } else { '' }
$expectBackend = $false
foreach ($argument in $args) {
    if ($expectBackend) {
        $requestedBackend = $argument
        $expectBackend = $false
        $playerArgs += $argument
        continue
    }
    if ($argument -eq '-DryRun' -or $argument -eq '--dry-run') {
        $dryRun = $true
    }
    elseif ($argument -eq '--api') {
        $expectBackend = $true
        $playerArgs += $argument
    }
    elseif ($argument -like '--api=*') {
        $requestedBackend = $argument.Substring(6)
        $playerArgs += $argument
    }
    else {
        $playerArgs += $argument
    }
}

$repoDir = Split-Path -Parent $PSScriptRoot
$appDirName = 'kugou-tui'
# 可执行文件后缀。用 $env:OS 而不是 $IsWindows：后者是 PowerShell 6+ 才有的自动变量，
# 而 Windows 自带的是 5.1。
$exeSuffix = if ($env:OS -eq 'Windows_NT') { '.exe' } else { '' }

# ==================================================================
# 路径：必须和 Rust 侧的 dirs 对齐，否则会去错地方找配置
#
# 环境变量优先（Windows 上就是 `dirs` 拿到的那几个），拿不到再问 .NET 的特殊
# 文件夹——那边在 Windows 上给出同样的路径，在别的平台上给出各自的正统位置。
# 不写成「只用 %APPDATA%」是为了**能在 Linux 上拿 pwsh 跑起来验证**：
# 变量缺失时 `Join-Path` 会直接抛「Path 为 null」，而不是安静地用一个错路径。
# ==================================================================
function Resolve-BaseDir {
    param(
        [string]$EnvValue,
        [string]$SpecialFolder,
        [string]$Fallback = '.'
    )
    if ($EnvValue) { return $EnvValue }
    $fromDotNet = [Environment]::GetFolderPath($SpecialFolder)
    if ($fromDotNet) { return $fromDotNet }
    return $Fallback
}

# `dirs::config_dir()` 在 Windows 上是 %APPDATA%（Roaming）。
$configDir = if ($env:KUGOU_TUI_CONFIG_DIR) {
    $env:KUGOU_TUI_CONFIG_DIR
}
else {
    Join-Path (Resolve-BaseDir $env:APPDATA 'ApplicationData') $appDirName
}
$configFile = Join-Path $configDir 'config.toml'

# `dirs::cache_dir()` 在 Windows 上是 %LOCALAPPDATA%。
$cacheDir = Join-Path (
    Resolve-BaseDir $env:LOCALAPPDATA 'LocalApplicationData' ([System.IO.Path]::GetTempPath())
) $appDirName
$apiLog = if ($env:KUGOU_API_LOG) { $env:KUGOU_API_LOG } else { Join-Path $cacheDir 'api.log' }

$homeDir = Resolve-BaseDir $env:USERPROFILE 'UserProfile' ([System.IO.Path]::GetTempPath())

# ==================================================================
# 配置解析
#
# 只认 `key = "value"` 这一种写法（serde 序列化出来的就是这种），够用了；
# 不为了读两个字符串去引一个 TOML 解析器。
# ==================================================================
$configText = if (Test-Path $configFile) { Get-Content -Raw $configFile } else { '' }

# 取某个键的字符串值。用 `(?m)^\s*key\s*=` 锚行首，免得 `api_base` 命中
# `sources.kugou.api_base` 之类；取**第一处**，与配置文件里顶层键在前的顺序一致。
function Get-ConfigValue {
    param([string]$Text, [string]$Key)
    if (-not $Text -or -not $Key) { return '' }
    $match = [regex]::Match($Text, '(?m)^\s*' + [regex]::Escape($Key) + '\s*=\s*"([^"]*)"')
    if ($match.Success) { return $match.Groups[1].Value }
    return ''
}

# 取 `[sources.<kind>]` 段里的 `api_base`。
#
# 为什么不用顶层那个：顶层 `api_base` 可能是脏的。用 `--api-base` 临时指向别处后退出，
# 它会被写进配置文件，而选中音源没变——于是「顶层写着 :3000、实际用的是概念版 :3001」。
# 启动器若信了顶层值，就会把服务起在错误的端口上，探活永远过不去。
function Get-SourceApiBase {
    param([string]$Text, [string]$Kind)
    if (-not $Text -or -not $Kind) { return '' }
    # (?ms)：^ 和 $ 认行边界、. 跨行。切出该段直到下一个 `[` 开头的行或文末。
    $section = [regex]::Match(
        $Text,
        '(?ms)^\[sources\.' + [regex]::Escape($Kind) + '\]\s*$(.*?)(?=^\[|\z)'
    )
    if (-not $section.Success) { return '' }
    return Get-ConfigValue -Text $section.Groups[1].Value -Key 'api_base'
}

# ==================================================================
# 定位二进制：环境变量 → 仓库构建产物 → PATH
# ==================================================================
$bin = ''
if ($env:KUGOU_TUI_BIN) {
    $bin = $env:KUGOU_TUI_BIN
}
else {
    # 两条都要看：cargo 只在显式传 `--target` 时才把产物放进 `target\<三元组>\release\`。
    $binCandidates = @(
        (Join-Path $repoDir 'target' | Join-Path -ChildPath 'release' |
            Join-Path -ChildPath "kugou-tui$exeSuffix"),
        (Join-Path $repoDir 'target' | Join-Path -ChildPath 'x86_64-pc-windows-msvc' |
            Join-Path -ChildPath 'release' | Join-Path -ChildPath "kugou-tui$exeSuffix")
    )
    $bin = $binCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $bin) {
        $found = Get-Command "kugou-tui$exeSuffix" -ErrorAction SilentlyContinue
        if ($found) { $bin = $found.Source }
    }
}

if (-not $bin) {
    Write-Host "找不到可执行文件。试过：" -ForegroundColor Red
    foreach ($candidate in $binCandidates) { Write-Host "  $candidate" }
    Write-Host '从源码跑的话先编译：cargo build --release'
    Write-Host '已经装过的话，用 KUGOU_TUI_BIN 指向 kugou-tui 的实际路径。'
    exit 1
}

# 路径存在也要确认一次。`KUGOU_TUI_BIN` 指错（或指到一个已删的目录）时，
# 之前会一路走到最后才由 CreateProcess 抛一句 Win32 错误，看不出是哪里配错了。
if (-not (Test-Path -LiteralPath $bin -PathType Leaf)) {
    Write-Host "KUGOU_TUI_BIN 指向的文件不存在：$bin" -ForegroundColor Red
    exit 1
}

# ==================================================================
# 当前音源 → 服务目录 / 端口 / platform
# ==================================================================
$activeSource = Get-ConfigValue -Text $configText -Key 'active'
if (-not $activeSource) { $activeSource = 'kugou' }

# 汽水直连公网（api.qishui.com），本机既没有、也不该有它的服务进程。
# `$needsService` 为假时，下面整套探活/拉起/等就绪都要跳过——否则用户选了汽水，
# 启动器却去把 KuGouMusicApi 拉起来，提示写着「未运行，正在启动…」，而它根本用不上。
$isSodam = $activeSource -eq 'sodam'
$isNetease = $activeSource -eq 'netease'
$needsService = -not $isSodam
$serviceName = if ($isSodam) { '汽水公网接口' } elseif ($isNetease) { 'NeteaseCloudMusicApi' } else { 'KuGouMusicApi' }
$defaultApiDirName = if ($isNetease) { 'NeteaseCloudMusicApi' } else { 'KuGouMusicApi' }

# 后端是 `native` 时酷狗接口实现在**进程内**，本机根本不需要 KuGouMusicApi。
# 早先这里只看音源，于是后端明明是 native，启动器照样探端口、探不到就 `node app.js`
# ——刚装好的纯 Rust 版本，第一次启动就在后台拉了一个 Node 进程。
#
# 取值顺序与 Rust 侧一致：`--api` / `KUGOU_API_BACKEND` > 配置 > 默认 `native`。
# 与 `ApiBackend::effective_for()` 对齐：`native` 只覆盖酷狗两个平台，网易云与汽水
# 的接口不在 `MusicApi` 里，它们照旧要本机服务。
$serviceSkipReason = ''
if (-not $requestedBackend) {
    $requestedBackend = Get-ConfigValue -Text $configText -Key 'api_backend'
}
if (-not $requestedBackend) { $requestedBackend = 'native' }

if ($needsService -and $requestedBackend -eq 'native') {
    if ($activeSource -in @('kugou', 'kugou_concept')) {
        $needsService = $false
        $serviceSkipReason = '内嵌后端（--api native）在进程内实现酷狗接口，不需要 Node.js'
    }
}
if (-not $needsService -and -not $serviceSkipReason) {
    $serviceSkipReason = '该音源直连公网，本机没有服务'
}

# 实例名（standard / lite）——与 `kugou-api`（bash 版与 kugou-api.ps1）的实例名、
# PID 文件命名保持一致。这样启动器拉起来的服务，`kugou-api.ps1 status` 看得见、
# `kugou-api.ps1 stop` 停得掉；否则就只能靠任务管理器手杀 node。
$instanceName = if ($isSodam) { 'sodam' } elseif ($activeSource -eq 'kugou_concept') { 'lite' } else { 'standard' }
$pidFile = Join-Path $cacheDir "api-$instanceName.pid"

# 服务目录按**当前音源**选择。写死 KuGouMusicApi 会导致：当前音源是网易云时，
# 启动器拿酷狗的代码去监听 :3002，而真正的网易云服务已经占用了这个端口 ——
# 于是 EADDRINUSE，程序直接起不来。
$apiDir = ''
if ($isNetease -and $env:NETEASE_API_DIR) {
    $apiDir = $env:NETEASE_API_DIR
}
elseif ((-not $isNetease) -and $env:KUGOU_API_DIR) {
    $apiDir = $env:KUGOU_API_DIR
}
else {
    $apiDir = Join-Path $homeDir $defaultApiDirName
}

# 健康检查用的地址。**不要**用它去覆盖客户端的地址：客户端自己会从配置里解析
# （含音源切换），启动器强行传 --api-base 会让音源选择失效——用户就被迫每次
# 手动按 v 切两次才回到概念版。所以下面只在显式设了 KUGOU_API_BASE 时才交给它，
# 而那本来就由播放器自己从同名环境变量读（见 src/cli.rs 的 `env = "KUGOU_API_BASE"`）。
$checkBase = ''
if ($env:KUGOU_API_BASE) {
    $checkBase = $env:KUGOU_API_BASE
}
else {
    $checkBase = Get-SourceApiBase -Text $configText -Kind $activeSource
    if (-not $checkBase) {
        # 退回顶层值（老配置可能还没有 sources 段）
        $checkBase = Get-ConfigValue -Text $configText -Key 'api_base'
    }
    if (-not $checkBase) { $checkBase = 'http://127.0.0.1:3000' }
}

$portMatch = [regex]::Match($checkBase, ':(\d{1,5})')
$apiPort = if ($portMatch.Success) { [int]$portMatch.Groups[1].Value } else { 3000 }

# 平台（`lite` = 概念版）。不能省：酷狗两个平台是两套鉴权体系，服务端靠 `platform`
# 区分；不带参数拉起等于永远起的是标准版，当前音源若是概念版（:3001），探活就一直失败。
$apiPlatform = if ($activeSource -eq 'kugou_concept') { 'lite' } else { '' }
$platformNote = if ($apiPlatform) { "，平台 $apiPlatform" } else { '' }

# 探活走 `/`：express.static 直接吐静态页，是纯本地操作。
# **不校验响应内容**——早先要求首页含「酷狗」二字，于是网易云的首页明明在跑却被判成
# 未运行，接着又去启动一遍，撞上端口占用。探活就该只探活。
function Test-ApiAlive {
    param([string]$Base)
    try {
        Invoke-WebRequest -Uri "$Base/" -TimeoutSec 2 -UseBasicParsing -ErrorAction Stop | Out-Null
        return $true
    }
    catch {
        return $false
    }
}

if ($dryRun) {
    Write-Host '--- dry-run：只打印决策，不启动任何东西 ---' -ForegroundColor Cyan
    Write-Host "配置        : $configFile"
    Write-Host "当前音源    : $activeSource"
    Write-Host "播放器      : $bin"
    Write-Host "透传参数    : $($playerArgs -join ' ')"
    Write-Host "接口后端    : $requestedBackend"
    if (-not $needsService) {
        # 说清「不需要」而不是打一串用不上的端口/目录：那会让人以为还得去准备点东西。
        Write-Host "接口服务    : 不需要（$serviceSkipReason）"
    }
    else {
        Write-Host "实例        : $instanceName（PID 文件 $pidFile）"
        Write-Host "探测地址    : $checkBase（端口 $apiPort$platformNote）"
        Write-Host "服务目录    : $apiDir"
        Write-Host "服务日志    : $apiLog"
        Write-Host "服务在跑吗  : $(if (Test-ApiAlive $checkBase) { '在跑' } else { '没起' })"
    }
    exit 0
}

# ==================================================================
# 服务没起就拉起来
# ==================================================================
if ($needsService -and -not (Test-ApiAlive $checkBase)) {
    if (-not (Test-Path $apiDir)) {
        Write-Host "$serviceName 未运行，且目录不存在：$apiDir" -ForegroundColor Yellow
        Write-Host '先执行：'
        Write-Host "  .\scripts\kugou-api-install.ps1 $activeSource"
        exit 1
    }

    $node = Get-Command node -ErrorAction SilentlyContinue
    if (-not $node) {
        Write-Host '找不到 node。装一个 Node.js：https://nodejs.org' -ForegroundColor Red
        exit 1
    }

    Write-Host "$serviceName 未运行，正在启动…（端口 $apiPort$platformNote）" -ForegroundColor Cyan
    $logDir = Split-Path -Parent $apiLog
    if ($logDir -and -not (Test-Path $logDir)) {
        New-Item -ItemType Directory -Force -Path $logDir | Out-Null
    }
    # stderr 只能另开一个文件：`Start-Process` 明确拒绝把两个流重定向到同一个路径
    # （「RedirectStandardOutput and RedirectStandardError are same」）。出问题时
    # 两个都打出来，用户不用猜日志在哪。
    $errLog = "$apiLog.err"

    # 端口**同时**用环境变量和命令行参数给，因为两个服务认的不是同一个东西：
    #
    # * KuGouMusicApi 认 `--port=` 参数；
    # * NeteaseCloudMusicApi 只认 `PORT` 环境变量——传 `--port=3002` 会被忽略，
    #   于是它起在默认的 3000，而脚本探的是 3002，等 20 秒后报「服务启动失败」。
    #
    # 两个都给对两种服务都无害：认参数的用参数，认环境的用环境。
    $nodeArgs = @('app.js', "--port=$apiPort")
    if ($apiPlatform) { $nodeArgs += "--platform=$apiPlatform" }

    $previousPort = $env:PORT
    $env:PORT = "$apiPort"
    $startArgs = @{
        FilePath               = $node.Source
        ArgumentList           = $nodeArgs
        WorkingDirectory       = $apiDir
        RedirectStandardOutput = $apiLog
        RedirectStandardError  = $errLog
        PassThru               = $true
    }
    # -WindowStyle 只在 Windows 上存在（Linux 版 pwsh 直接报「不支持该参数」）。
    # 隐藏窗口是为了别在用户桌面弹一个黑框；服务本身要活到播放器退出之后，
    # 所以不能在本进程里等它。
    if ($env:OS -eq 'Windows_NT') { $startArgs['WindowStyle'] = 'Hidden' }

    try {
        $proc = Start-Process @startArgs
    }
    finally {
        $env:PORT = $previousPort
    }

    # 记下 PID 与端口，格式与 Unix 侧一致（`<PID> <端口>`，空格分隔）。
    # 只记 PID 不够：端口改了以后单看进程是否还活着，会以为「还是那个实例」。
    #
    # 先确保缓存目录在：上面建的是**日志**的目录，而 KUGOU_API_LOG 指到别处时
    # 两者不是同一个，PID 文件会因为父目录不存在而写失败。
    if (-not (Test-Path -LiteralPath $cacheDir)) {
        New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
    }
    Set-Content -LiteralPath $pidFile -Value "$($proc.Id) $apiPort" -NoNewline -Encoding ascii

    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        Start-Sleep -Milliseconds 500
        if (Test-ApiAlive $checkBase) { break }
    }

    if (-not (Test-ApiAlive $checkBase)) {
        Write-Host '服务启动失败，日志尾部：' -ForegroundColor Red
        foreach ($log in $apiLog, $errLog) {
            if (Test-Path $log) {
                Write-Host "--- $log ---" -ForegroundColor DarkGray
                Get-Content -Tail 15 $log | ForEach-Object { Write-Host "  $_" }
            }
        }
        Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
        Write-Host ''
        Write-Host "排查：$PSCommandPath --dry-run 会打印它到底在探哪个地址。"
        exit 1
    }
    Write-Host "服务已就绪：$checkBase（PID $($proc.Id)，停止用 kugou-api.ps1 stop）" -ForegroundColor Green
}

# ==================================================================
# 进播放器
# ==================================================================
& $bin @playerArgs
exit $LASTEXITCODE
