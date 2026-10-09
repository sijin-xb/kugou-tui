# 常见问题

## 接口服务

**Q：启动说找不到 node？**
A：那只会在 `--api node` 回退路径上发生——它跑的是第三方服务 KuGouMusicApi，是 Node.js
写的。装一个 Node.js（≥ 12，带 npm）再启动即可，程序会自己把服务准备好并拉起，不需要你
clone 任何仓库。**默认的内嵌后端（`--api native`）不需要 Node.js**，如果你不打算用回退，
加 `--api native` 就行；想连引导代码一起去掉，用 `--no-default-features` 构建。

**Q：第一次启动卡了半分钟，正常吗？**
A：只有 `--api node` 会这样。首次运行要下载 KuGouMusicApi（钉住的提交）并
`npm install --omit=dev`，实测约 25 秒。之后每次启动探到端口就直接复用。想提前做完：
`kugou-tui --api node --api-start`。内嵌后端没有这一步，启动即可用。

**Q：服务起不来 / 端口被占？**
A：看日志 `~/.cache/kugou-tui/api-standard.log`（概念版是 `api-lite.log`）。端口被别的程序
占着时程序不会去抢，会直接报错——`kugou-tui --print-config` 看它探的是哪个地址，或者用
`-a http://127.0.0.1:3100` 换一个。要完全自己管服务就设 `api_auto_start = false`
（或 `--no-api-start`）。

**Q：概念版音源怎么配？**
A：平台由服务端的 `platform` 环境变量决定（`lite` = 概念版），而一个 Node 进程只能加载一份
配置，所以**概念版要单独起一个实例**：

```bash
cd KuGouMusicApi && platform=lite PORT=3001 node app.js
# 或用仓库脚本一次拉起两个：kugou-api start
```

> 用 `cargo install` 装的程序**不需要手动做这件事**：切到概念版时会自动补起那个实例。

**Q：为什么切到「酷狗概念版」还是只能听试听？**
A：最可能是**没在概念版上登录**。两个平台的 token 不通用（上游文档原话：「不同版本的平台的
token 是不通用的」），标准版扫出来的登录态拿到概念版去用，服务端不认，会员信息是空的，于是
退化成试听。切到概念版后按 `L` **重新扫一次码**。确认命令：

```bash
curl -s -H "Cookie: $(rg '^cookie' ~/.config/kugou-tui/config.toml | sed 's/^cookie = "//; s/"$//')" \
  "http://127.0.0.1:3001/user/vip/detail" | jq '.data'
```

返回 `status: 0` 就说明这个实例还没登录。

**Q：怎么知道接口服务通没通？**
A：看侧边栏「连接」区块的标题——`已连通` / `未连通` / `未验证`。它由**真实请求的结果**驱动，
不是启动时猜的：`未验证` 表示这一刻还没发过请求，`未连通` 表示最近一次请求是连接被拒或超时。
显示 `未连通` 就是服务没跑或地址不对（用 `--print-config` 看它连的是哪个地址）。

> 「需要登录」「页码越界」这类**业务**错误码算**已连通**——服务回了话，只是拒绝了这次请求。

## 播放

**Q：不登录能听歌吗？**
A：能。浏览歌单广场 / 歌手 / 排行榜、播放都不需要登录，只有搜索和云端歌单需要。

**Q：搜索没反应 / 提示需要登录？**
A：`/search` 缺认证会返回 `error_code: 152`，按 `L` 扫码即可。注意 KuGouMusicApi 业务失败时
会把错误码放在响应体里，而 HTTP 状态码可能是 `502`——看到 502 不要以为服务挂了。

**Q：取播放地址失败，提示「本次请求需要验证」？**
A：通常是 `dfid` 缺失。程序首次启动会自动获取；若配置里没有，删掉配置让它重新取一次。
用 MV 的 hash 或 `hash_multitrack` 去取音频地址也会报这个，但正常路径用的是
`audio_info.hash_128`，不会触发。

**Q：一首歌等很久才开始放？**
A：已经做了**边下边播**：取到直链后先攒够开头（128 KB，约 8 秒音频）就开播，剩下的后台继续
下并落盘。所以等待是「攒开头」而不是「下完整首」。想更快：**降音质**（`320` 约 10 MB、
`flac` 约 30 MB、`high` 约 65 MB——这是文件大小的物理限制）；直链支持 Range 时会**分最多
4 块并发下载**；当前这首开播后会**预取队列下一首**；第二次播放同一首直接命中缓存。

> 边界：流式缓冲的读指针跑到还没下到的位置时会**阻塞等数据**，表现是声音停一下再继续
> （而不是提前结束）。网络抖动大时能听出来。

**Q：进度条在走、状态是「播放中」，但一点声音都没有？**
A：按 `,` 打开设置页看最后一项「输出设备」——那里显示的是**实际打开的那张卡**，十有八九不是
你在听的那张：

```bash
ls -l /proc/$(pgrep -x kugou-tui)/fd | grep snd   # 有输出 = 直连硬件，绕过了 PipeWire
pactl list sink-inputs                            # 播放时看不到自己 = 确实绕过去了
```

根因几乎都在系统的 ALSA 配置：`/etc/asound.conf` 或 `~/.asoundrc` 把 `default` 写死成了某张卡
（形如 `pcm.!default { type hw card 2 }`），而**卡号会变**。让 `default` 回到 PipeWire
（`type pipewire`，或删掉那个文件）即可。详见 [CONFIGURATION.md](CONFIGURATION.md#输出设备audio_device)。

**Q：歌词不显示 / 对不上？**
A：歌词来自接口，没有就是没有。只是时间对不上就用 `[` / `]` 以 100 ms 为步长微调，偏移量会
记进配置。

**Q：听久了内存只涨不落？**
A：0.4.7 修过一轮（mmap 阈值定得太高，封面位图落在阈值下走了 brk arena，free 后页不还给 OS）。
若仍觉得偏高，用 `KUGOU_TUI_MEM_TRACE=1 kugou-tui` 启动，日志里每 5 秒一行 RSS 连同「流式
任务数 / 队列长度 / 命中区容量」——它能区分「有东西被长期持有」和「页还没还」。另外
`lite_mode` 会关掉封面解码，那是最占内存的一块。

## 界面与数据

**Q：方向键 / 数字键没反应？**
A：先看是不是停在搜索框的输入态——此时字母键会插入文本，按 `Esc` 退出输入态再试。若确认
不是，用 `KUGOU_TUI_DEBUG=1 kugou-tui` 启动，日志会记录每个按键映射成了什么动作，据此判断
「按键没到程序」还是「被映射成了空动作」。

**Q：鼠标滚轮滚动了页面而不是列表？**
A：程序开启了鼠标捕获。若终端做了额外的鼠标转发（某些 tmux 配置），可能冲突，用终端自身的
设置关掉即可。

**Q：打开一个几百首的歌单，为什么要等几秒？**
A：歌单接口每页**硬限 30 首**——`pagesize` 传 100 甚至 500，服务端也只回 30 条，而且响应里的
`count` 只是回显当页条数、不是总数。所以客户端只能逐页翻完：实测 385 首的歌单需要 13 次请求。
榜单和歌手歌曲同理。搜索结果与歌单广场则按 `page_size`（默认 30）只取一页。

**Q：歌单显示的数量和实际对不上？**
A：歌单元数据里的 `percount`、`songcount` 实测恒为 `0`，不可用于展示。界面上的「N 首」是
**实际取到的条目数**，可信。

**Q：缓存占太多空间？**
A：默认上限 512 MiB，超了按 LRU 从旧到新回收。改 `cache_limit_mib`，`--cache-limit 0` 表示
不限制。

## 平台相关

**Q：Windows 上怎么装？为什么 `./scripts/kugou-api-install` 跑不了？**
A：那几个脚本是 bash，PowerShell 里跑不了。Windows 用对应的 `.ps1`：

```powershell
.\scripts\kugou-api-install.ps1
.\scripts\kugou-tui.ps1
```

完整步骤（工具链前置、终端要求、平台差异表）见
[INSTALL.md](INSTALL.md#在-windows-上构建与运行)。

**Q：Windows 上构建报 `linker link.exe not found` / `cmake not found` / `nasm not found`？**
A：前者是没装 MSVC 工具链（装 Visual Studio 生成工具并勾「使用 C++ 的桌面开发」）。后两者
**不应该出现**：Windows 上 TLS 走系统自带的 SChannel 而不是 rustls，`aws-lc-sys` 那坨 C 代码
根本不参与编译——真遇到了说明你编的不是这个仓库当前的代码。

**Q：Windows 上托盘图标 / `playerctl` 怎么没有？封面怎么是马赛克色块？**
A：都不是故障。托盘与 MPRIS 是 D-Bus 接口，Windows 没有 session bus，相关代码**不参与编译**
（`--print-config` 会打印「系统托盘 : 不可用」）。封面是**半块字符画**——程序检测到终端不支持
Kitty / iTerm2 图形协议就自动降级，而 Windows Terminal 两者都不支持（想省掉这份开销可以开
`lite_mode`）。见 [INSTALL.md 的功能差异表](INSTALL.md#6-与-linux-的功能差异)。

**Q：图标变成一堆方块 / 问号？**
A：终端字体没有对应字形。程序会先探测有没有 Nerd Font，没有就退回 ASCII 图标；但 **Windows
上没有 fontconfig，探测一律落空**，装了 Nerd Font 也不会自动认出来——设
`KUGOU_TUI_NERD_FONT=1` 手动打开（Linux / macOS 上也可以用这个变量覆盖探测结果）。

**Q：macOS 支持吗？**
A：**代码支持，CI 覆盖，但没有在真机长期用过**。走的是与 Linux 同一套 `cfg(unix)` 分支，音频
走 CoreAudio，构建只需 Xcode 命令行工具。已知差异：没有 MPRIS / 系统托盘、没有最小化窗口
（走 niri 的 compositor IPC）、配置在 `~/Library/Application Support/kugou-tui/`、缓存在
`~/Library/Caches/kugou-tui/`（**不是** `~/.config` 与 `~/.cache`）、Terminal.app 下封面退半块
字符画（iTerm2 能走图形协议）。细节见 [INSTALL.md](INSTALL.md#在-macos-上构建与运行)。
