<#
.SYNOPSIS
    kugou-api —— KuGouMusicApi 服务的启停管理（start / stop / restart / status / logs）。

.DESCRIPTION
    对应 Unix 侧的 `scripts/kugou-api`。

    **为什么需要它**：`kugou-tui.ps1` 只负责「探活 → 没起就拉起来 → 进播放器」，
    它不会停服务、也不会告诉你服务现在是什么状态。少了这个脚本，Windows 上想停掉
    后台的 node 只能去任务管理器里按名字猜着杀——而猜错就会把别的 Node 项目一起干掉。

    PID 文件的格式（`<PID> <端口>`，空格分隔）与 Unix 侧完全一致，两个平台看到的是
    同一套状态；启动器 `kugou-tui.ps1` 起的实例，这里也认得、停得掉。

    网易云实例（`netease`）同样由启动器拉起，这里不负责 `start` 它；但 `stop` /
    `status` / `logs` 认得它，所以启动器拉起的网易云也停得掉、看得到。

    为什么要两个实例：酷狗两个平台是两套独立的鉴权体系，平台由服务端 `platform`
    决定（`lite` = 概念版），而一个 Node 进程只能加载一份配置。所以要同时用两个
    音源，就得起两个进程、各占一个端口。

    注意：两个平台的登录态不通用。切到概念版音源后需要按 L 重新扫码。

.PARAMETER Command
    `start` / `stop` / `restart` / `status` / `logs` / `help`。默认 `start`。

.PARAMETER Instance
    只对 `logs` 有效：跟哪个实例的日志，`standard`（默认）、`lite` 或 `netease`。

.EXAMPLE
    .\scripts\kugou-api.ps1 status

.EXAMPLE
    .\scripts\kugou-api.ps1 restart

.EXAMPLE
    .\scripts\kugou-api.ps1 logs lite
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('start', 'stop', 'restart', 'status', 'logs', 'help')]
    [string]$Command = 'start',

    [Parameter(Position = 1)]
    [ValidateSet('standard', 'lite', 'netease')]
    [string]$Instance = 'standard'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# ==================================================================
# 路径：必须和 Rust 侧的 dirs、以及 kugou-tui.ps1 对齐
# ==================================================================
function Resolve-BaseDir {
    param([string]$EnvValue, [string]$SpecialFolder, [string]$Fallback = '.')
    if ($EnvValue) { return $EnvValue }
    $fromDotNet = [Environment]::GetFolderPath($SpecialFolder)
    if ($fromDotNet) { return $fromDotNet }
    return $Fallback
}

$configDir = if ($env:KUGOU_TUI_CONFIG_DIR) {
    $env:KUGOU_TUI_CONFIG_DIR
}
else {
    Join-Path (Resolve-BaseDir $env:APPDATA 'ApplicationData') 'kugou-tui'
}
$configFile = Join-Path $configDir 'api.env'

$cacheDir = Join-Path (Resolve-BaseDir $env:LOCALAPPDATA 'LocalApplicationData') 'kugou-tui'
$logDir = if ($env:KUGOU_API_LOG_DIR) { $env:KUGOU_API_LOG_DIR } else { $cacheDir }

$homeDir = Resolve-BaseDir $env:USERPROFILE 'UserProfile'

# ==================================================================
# 参数：环境变量 > api.env > 默认值
#
# api.env 每行一个 `KEY=VALUE`，键名就是下面这些环境变量名。只认这些键，未知键
# 警告并忽略——不加限制地执行一个用户文件，等于允许它改写脚本里的任何变量。
# ==================================================================
$knownKeys = @(
    'KUGOU_API_DIR', 'KUGOU_API_LOG_DIR', 'KUGOU_API_HOST',
    'KUGOU_STANDARD_PORT', 'KUGOU_LITE_PORT'
)

$config = @{}
if (Test-Path -LiteralPath $configFile) {
    foreach ($line in Get-Content -LiteralPath $configFile) {
        $trimmed = $line.Trim()
        if (-not $trimmed -or $trimmed.StartsWith('#')) { continue }
        $split = $trimmed.IndexOf('=')
        if ($split -lt 1) { continue }
        $key = $trimmed.Substring(0, $split).Trim()
        $value = $trimmed.Substring($split + 1).Trim().Trim('"').Trim("'")
        if ($knownKeys -contains $key) {
            $config[$key] = $value
        }
        else {
            Write-Host "配置 $configFile 里有未知键「$key」，已忽略" -ForegroundColor Yellow
        }
    }
}

function Pick-Value {
    param([string]$Name, [string]$Default)
    $fromEnv = [Environment]::GetEnvironmentVariable($Name)
    if ($fromEnv) { return $fromEnv }
    if ($config.ContainsKey($Name) -and $config[$Name]) { return $config[$Name] }
    return $Default
}

$apiDir = Pick-Value 'KUGOU_API_DIR' (Join-Path $homeDir 'KuGouMusicApi')
$apiHost = Pick-Value 'KUGOU_API_HOST' '127.0.0.1'
$standardPort = [int](Pick-Value 'KUGOU_STANDARD_PORT' '3000')
$litePort = [int](Pick-Value 'KUGOU_LITE_PORT' '3001')

# 两个实例：名字 / 端口 / platform（标准版不传 platform，服务端默认就是手机版）
$instances = @(
    [pscustomobject]@{ Name = 'standard'; Port = $standardPort; Platform = '' },
    [pscustomobject]@{ Name = 'lite'; Port = $litePort; Platform = 'lite' }
)

# 网易云不是酷狗实例，由启动器 kugou-tui.ps1 自己拉起（目录、平台都和酷狗不是一套），
# 所以这里不负责 start 它。但 stop / status / logs 必须认得它：它和酷狗写同一套 PID
# 文件，认不得就只能放任它占着端口（以前更糟——它沿用 standard 实例名，stop 会照着
# api-standard.pid 误杀到酷狗标准版）。
$neteasePort = [int](Pick-Value 'NETEASE_PORT' '3002')
$externalInstances = @(
    [pscustomobject]@{ Name = 'netease'; Port = $neteasePort }
)

# ==================================================================
# 小工具
# ==================================================================

# 探活走 `/`：express.static 直接吐静态页，是纯本地操作。
function Test-ApiAlive {
    param([int]$Port)
    try {
        Invoke-WebRequest -Uri "http://${apiHost}:$Port/" -TimeoutSec 2 -UseBasicParsing -ErrorAction Stop | Out-Null
        return $true
    }
    catch {
        return $false
    }
}

# PID 文件里存「PID 端口」两个字段，与 Unix 侧一致。
#
# 只存 PID 是不够的：改了端口重启（或只改配置没重启）时，旧进程还活着，
# 单看进程是否存在会通过，于是 status 拿**新**端口报一个「运行中」——
# 而那个进程其实监听在旧端口上。
function Read-PidRecord {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    # 空文件要挡住：`Get-Content -Raw` 对空文件返回 $null，直接 .Trim() 会抛异常。
    $raw = Get-Content -LiteralPath $Path -Raw -ErrorAction SilentlyContinue
    if (-not $raw) { return $null }
    $parts = $raw.Trim() -split '\s+'
    if ($parts.Count -lt 1 -or $parts[0] -notmatch '^\d+$') { return $null }
    $record = [pscustomobject]@{ ProcessId = [int]$parts[0]; Port = 0 }
    if ($parts.Count -ge 2 -and $parts[1] -match '^\d+$') { $record.Port = [int]$parts[1] }
    return $record
}

function Test-AliveProcess {
    param([int]$ProcessId)
    if ($ProcessId -le 0) { return $false }
    return [bool](Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)
}

# 监听该端口的进程 PID；查不到就返回 0。
# `Get-NetTCPConnection` 是 Windows 8+ / Server 2012+ 自带的，不需要额外工具。
function Get-PortOwner {
    param([int]$Port)
    $connection = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($connection) { return [int]$connection.OwningProcess }
    return 0
}

# 外部实例（网易云）的端口不写死：它跟着当前音源的 api_base 走，启动器已经写进了
# PID 文件。读不到（还没起过、或文件已删）才退回配置里的默认端口。
function Get-ExternalPort {
    param([pscustomobject]$Instance)
    $record = Read-PidRecord (Join-Path $logDir "api-$($Instance.Name).pid")
    if ($record -and $record.Port) { return $record.Port }
    return $Instance.Port
}

function Write-Die {
    param([string]$Message)
    Write-Host $Message -ForegroundColor Red
    exit 1
}

# ==================================================================
# 启动前预检
# ==================================================================

function Assert-ApiDir {
    if (-not (Test-Path -LiteralPath $apiDir)) {
        Write-Host "找不到 KuGouMusicApi：$apiDir" -ForegroundColor Red
        Write-Host '用项目里的安装脚本拉一份（它会自动 clone + npm install）：'
        Write-Host "  .\scripts\kugou-api-install.ps1"
        exit 1
    }
}

# 依赖缺失就自动装——这就是这个服务现有的构建命令。
# 以前只打印一句「请先 npm install」，用户得自己复制粘贴；脚本既然知道该怎么装，
# 就该直接装。
function Assert-Deps {
    if (Test-Path -LiteralPath (Join-Path $apiDir 'node_modules')) { return }
    if (-not (Test-Path -LiteralPath (Join-Path $apiDir 'package.json'))) {
        Write-Die "「$apiDir」里没有 package.json，看着不是 KuGouMusicApi。检查 KUGOU_API_DIR。"
    }
    Write-Host '缺少依赖，正在安装（npm install --omit=dev）…' -ForegroundColor Cyan
    Push-Location $apiDir
    try {
        & npm install --omit=dev
        if ($LASTEXITCODE -ne 0) {
            Write-Host '依赖安装失败' -ForegroundColor Red
            Write-Host "手动重试：cd `"$apiDir`" ; npm install --omit=dev"
            exit 1
        }
    }
    finally {
        Pop-Location
    }
}

function Assert-LogDir {
    if (-not (Test-Path -LiteralPath $logDir)) {
        New-Item -ItemType Directory -Force -Path $logDir | Out-Null
    }
}

# ==================================================================
# 启停
# ==================================================================

function Start-One {
    param([pscustomobject]$Instance)
    $name = $Instance.Name
    $port = $Instance.Port
    $platform = $Instance.Platform
    $pidPath = Join-Path $logDir "api-$name.pid"
    $log = Join-Path $logDir "api-$name.log"
    # stderr 只能另开一个文件：`Start-Process` 明确拒绝把两个流重定向到同一个路径
    $errLog = "$log.err"
    $platformNote = if ($platform) { "，平台 $platform" } else { '' }

    $record = Read-PidRecord $pidPath
    if ($record -and (Test-AliveProcess $record.ProcessId)) {
        if ($record.Port -and $record.Port -ne $port) {
            # 进程活着，但当初是给别的端口起的——多半是改了端口没重启。
            # 既不能报「已在运行」（新端口上没有服务），也不能硬起第二个
            # （旧进程还占着它自己的端口，用户会以为配置没生效）。
            Write-Host "$name 的旧实例仍在运行（PID $($record.ProcessId)，端口 $($record.Port)），但本次配置的端口是 $port" -ForegroundColor Yellow
            Write-Host "  先停掉它再起：.\scripts\kugou-api.ps1 restart"
            return $false
        }
        Write-Host "  $name :$port 已在运行（PID $($record.ProcessId)），跳过" -ForegroundColor Yellow
        return $true
    }

    # 端口被别的进程占着：不区分的话会当成「已在运行」，用户以为服务起了，
    # 实际监听的是别的东西。
    if (Test-ApiAlive $port) {
        $owner = Get-PortOwner $port
        Write-Host "端口 :$port 已被占用（PID $(if ($owner) { $owner } else { '未知' })），无法启动 $name" -ForegroundColor Red
        Write-Host "  换个端口起：`$env:KUGOU_STANDARD_PORT=3100 ; .\scripts\kugou-api.ps1 start"
        Write-Host "  或先停掉占用者：Stop-Process -Id $(if ($owner) { $owner } else { '<PID>' }) -Force"
        return $false
    }

    $node = Get-Command node -ErrorAction SilentlyContinue
    if (-not $node) {
        Write-Die '找不到 node。装一个 Node.js（>= 18）：https://nodejs.org'
    }

    # 端口**同时**用环境变量和命令行参数给，因为两个服务认的不是同一个东西：
    # KuGouMusicApi 认 `--port=`，NeteaseCloudMusicApi 只认 `PORT` 环境变量。
    # 两个都给对两种服务都无害。
    $nodeArgs = @('app.js', "--port=$port")
    if ($platform) { $nodeArgs += "--platform=$platform" }

    $previousPort = $env:PORT
    $env:PORT = "$port"
    $startArgs = @{
        FilePath               = $node.Source
        ArgumentList           = $nodeArgs
        WorkingDirectory       = $apiDir
        RedirectStandardOutput = $log
        RedirectStandardError  = $errLog
        PassThru               = $true
    }
    # -WindowStyle 只在 Windows 上存在（别的平台上 pwsh 会直接报「不支持该参数」）。
    if ($env:OS -eq 'Windows_NT') { $startArgs['WindowStyle'] = 'Hidden' }

    $proc = $null
    try {
        $proc = Start-Process @startArgs
    }
    finally {
        $env:PORT = $previousPort
    }

    # 与 Unix 侧同一个格式：`<PID> <端口>`
    Set-Content -LiteralPath $pidPath -Value "$($proc.Id) $port" -NoNewline -Encoding ascii

    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        Start-Sleep -Milliseconds 500
        if (Test-ApiAlive $port) { break }
    }

    if (-not (Test-ApiAlive $port)) {
        Write-Host "  $name :$port 启动失败（PID $($proc.Id)）" -ForegroundColor Red
        foreach ($file in $log, $errLog) {
            if (Test-Path -LiteralPath $file) {
                Write-Host "    --- $file ---" -ForegroundColor DarkGray
                Get-Content -Tail 10 $file | ForEach-Object { Write-Host "    $_" }
            }
        }
        Remove-Item -LiteralPath $pidPath -Force -ErrorAction SilentlyContinue
        return $false
    }

    Write-Host "  $name :$port 已就绪  PID $($proc.Id)" -ForegroundColor Green
    Write-Host "      地址 http://${apiHost}:$port/   日志 $log"
    return $true
}

function Stop-One {
    param([pscustomobject]$Instance)
    $name = $Instance.Name
    $port = $Instance.Port
    $pidPath = Join-Path $logDir "api-$name.pid"
    $record = Read-PidRecord $pidPath
    $stopped = $false

    if ($record -and (Test-AliveProcess $record.ProcessId)) {
        Stop-Process -Id $record.ProcessId -Force -ErrorAction SilentlyContinue
        for ($attempt = 0; $attempt -lt 20; $attempt++) {
            if (-not (Test-AliveProcess $record.ProcessId)) { break }
            Start-Sleep -Milliseconds 250
        }
        Write-Host "  $name 已停止（PID $($record.ProcessId)）"
        $stopped = $true
    }
    elseif (Test-ApiAlive $port) {
        # PID 文件丢了但端口还活着：按端口找回来，不要漏掉
        $owner = Get-PortOwner $port
        if ($owner) {
            Stop-Process -Id $owner -Force -ErrorAction SilentlyContinue
            Write-Host "  $name :$port 已停止（PID $owner，PID 文件缺失，按端口找回）"
            $stopped = $true
        }
        else {
            Write-Host "  $name :$port 端口有人在监听，但查不到 PID，未处理" -ForegroundColor Yellow
        }
    }

    if (-not $stopped) { Write-Host "  $name :$port 未运行" }
    Remove-Item -LiteralPath $pidPath -Force -ErrorAction SilentlyContinue
}

function Show-Status {
    param([pscustomobject]$Instance)
    $name = $Instance.Name
    $port = $Instance.Port
    $pidPath = Join-Path $logDir "api-$name.pid"
    $log = Join-Path $logDir "api-$name.log"
    $record = Read-PidRecord $pidPath

    if ($record -and (Test-AliveProcess $record.ProcessId)) {
        if ($record.Port -and $record.Port -ne $port) {
            Write-Host "  ● $name :$port 旧实例仍在运行（PID $($record.ProcessId)，端口 $($record.Port)）——配置的端口变了，需 restart" -ForegroundColor Yellow
            Write-Host "           旧地址 http://${apiHost}:$($record.Port)/"
        }
        else {
            Write-Host "  ● $name :$port 运行中  PID $($record.ProcessId)" -ForegroundColor Green
            Write-Host "           地址 http://${apiHost}:$port/"
            Write-Host "           日志 $log"
        }
    }
    elseif (Test-ApiAlive $port) {
        $owner = Get-PortOwner $port
        Write-Host "  ● $name :$port 端口有响应，但不是本脚本起的（PID $(if ($owner) { $owner } else { '未知' })）" -ForegroundColor Yellow
        Write-Host "           地址 http://${apiHost}:$port/"
    }
    else {
        Write-Host "  ○ $name :$port 未运行" -ForegroundColor Red
        Write-Host "           日志 $log"
    }
}

# ==================================================================
# 命令分发
# ==================================================================

switch ($Command) {
    'start' {
        Assert-ApiDir
        Assert-LogDir
        Assert-Deps

        Write-Host "启动 KuGouMusicApi（$apiDir）"
        $failed = $false
        foreach ($item in $instances) {
            if (-not (Start-One $item)) { $failed = $true }
        }
        Write-Host "标准版 :$standardPort ／ 概念版 :$litePort"
        if ($failed) { exit 1 }
    }

    'stop' {
        Write-Host '停止 KuGouMusicApi'
        foreach ($item in $instances) { Stop-One $item }
        # 网易云由启动器拉起，但它和酷狗写同一套 PID 文件，stop 就该把它也停掉，
        # 而不是留着它继续占端口。
        foreach ($item in $externalInstances) {
            Stop-One ([pscustomobject]@{ Name = $item.Name; Port = (Get-ExternalPort $item) })
        }
    }

    'restart' {
        Write-Host '重启 KuGouMusicApi'
        foreach ($item in $instances) { Stop-One $item }
        # 网易云不在重启范围内：restart 的语义是重启 KuGouMusicApi，而这里不负责
        # 拉起网易云（那是启动器的事）。停掉它反而会留下一个没人再起的服务。
        # 端口释放需要一点时间，不等的话下一轮会把「还没退干净」误判成端口占用
        Start-Sleep -Seconds 1
        Assert-ApiDir
        Assert-LogDir
        Assert-Deps
        $failed = $false
        foreach ($item in $instances) {
            if (-not (Start-One $item)) { $failed = $true }
        }
        Write-Host "标准版 :$standardPort ／ 概念版 :$litePort"
        if ($failed) { exit 1 }
    }

    'status' {
        Write-Host 'KuGouMusicApi 状态'
        foreach ($item in $instances) { Show-Status $item }
        foreach ($item in $externalInstances) {
            Show-Status ([pscustomobject]@{ Name = $item.Name; Port = (Get-ExternalPort $item) })
        }
    }

    'logs' {
        $log = Join-Path $logDir "api-$Instance.log"
        if (-not (Test-Path -LiteralPath $log)) {
            Write-Die "日志还不存在：$log（这个实例还没起过？先 kugou-api.ps1 start）"
        }
        Write-Host "跟随 $log（Ctrl-C 退出）"
        Get-Content -LiteralPath $log -Tail 40 -Wait
    }

    'help' {
        Get-Help $PSCommandPath -Detailed
    }
}
