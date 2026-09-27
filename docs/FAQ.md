# 常见问题

## 常见问题

**Q：概念版音源怎么配？**
A：平台由 KuGouMusicApi 服务端的 `platform` 环境变量决定（`lite` = 概念版），
而一个 Node 进程只能加载一份配置，所以**概念版需要单独起一个实例**：

```bash
cd KuGouMusicApi
platform=lite PORT=3001 node app.js
```

或者用仓库脚本一次拉起两个：`kugou-api start`。

**Q：为什么切到「酷狗概念版」还是只能听试听？**
A：最可能的原因是**没在概念版上登录**。两个平台的 token 不通用（上游文档原话：
「不同版本的平台的 token 是不通用的」）——标准版扫出来的登录态，拿到概念版去用
服务端不认，会员信息会是空的，于是退化成试听片段。

切到概念版后按 `L` **重新扫一次码**即可。可以用这个命令确认：

```bash
curl -s -H "Cookie: $(rg '^cookie' ~/.config/kugou-tui/config.toml | sed 's/^cookie = "//; s/"$//')" \
  "http://127.0.0.1:3001/user/vip/detail" | jq '.data'
```

返回 `status: 0` 就说明这个实例还没登录。

**Q：怎么知道 KuGouMusicApi 到底通没通？**
A：看侧边栏「连接」区块的**标题**——`已连通` / `未连通` / `未验证`。
它由**真实请求的结果**驱动，不是启动时猜的：`未验证` 表示这一刻还没发过任何请求，
`未连通` 表示最近一次请求是连接被拒或超时。

显示 `未连通` 就是服务没在跑（或地址不对）：用 `--print-config` 看它连的是哪个地址，
确认服务在监听；服务在别的端口就 `kugou-tui -a http://127.0.0.1:3001`。

> 注意：「需要登录」「页码越界」这类**业务**错误码算**已连通**——服务回了话，
> 只是拒绝了这次请求。只有连不上才算未连通。
>
> 另外，程序**不会**在启动时宣称「已连接」：那时一个请求都还没发过。启动那行只写
> `音源 <音源名> · <地址>（已登录）`，第一次请求成功之后才会改口。

**Q：不登录能听歌吗？**
A：能。浏览歌单广场 / 歌手 / 排行榜、播放，都不需要登录。只有搜索和云端歌单需要。

**Q：搜索没反应 / 提示需要登录？**
A：`/search` 缺认证会返回 `error_code: 152`。按 `L` 扫码即可。
注意 KuGouMusicApi 业务失败时会把错误码放在响应体里，而 HTTP 状态码可能是 `502`，
所以看到"502"不要以为是服务挂了。

**Q：取播放地址失败，提示"本次请求需要验证"？**
A：通常是 `dfid` 缺失。程序首次启动会自动获取，若配置里没有，删掉配置让它重新获取一次。
另外，用 MV 的 hash 或 `hash_multitrack` 去取音频地址也会报这个——本程序用的是
`audio_info.hash_128`，正常路径不会触发。

**Q：一首歌等很久才开始放？**
A：**已经做了边下边播**：取到直链后先攒够开头（128 KB，约 8 秒音频）就开播，
剩下的在后台继续下，同时落盘缓存。所以等待是「攒开头」而不是「下完整首」，
慢速连接下也不会退回「下完才播」。

想更快还可以做三件事：**降音质**（设置页改——`320` 约 10 MB、`flac` 约 30 MB、
`high`（Hi-Res）实测 65 MB，这是文件大小的物理限制，不是程序慢）；直链支持 Range 时
程序会**分最多 4 块并发下载**；当前这首开播后**后台预取队列里的下一首**，切歌基本秒开。
第二次播放同一首直接命中缓存，零等待。

> 仍然有个边界：流式缓冲的读指针跑到还没下到的位置时会**阻塞等数据**，表现是声音
> 停一下再继续，而不是提前结束。网络抖动大时能听出来。

**Q：进度条在走、状态是「播放中」，但一点声音都没有？**
A：先按 `,` 打开设置页看最后一项「输出设备」——那里显示的是**实际打开的那张卡**。
十有八九不是你在听的那张：

```bash
ls -l /proc/$(pgrep -x kugou-tui)/fd | grep snd   # 有输出 = 直连硬件，绕过了 PipeWire
pactl list sink-inputs                            # 播放时看不到自己 = 确实是绕过去了
```

根因几乎都在系统的 ALSA 配置：`/etc/asound.conf` 或 `~/.asoundrc` 把 `default` 写死
成了某一张卡（形如 `pcm.!default { type hw card 2 }`），而**卡号会变**。让 `default`
回到 PipeWire（`type pipewire`，或干脆删掉那个文件）即可。详见
[配置 → 输出设备](CONFIGURATION.md#输出设备)。

**Q：歌词不显示 / 对不上？**
A：歌词来自接口，没有就是没有。若只是时间对不上，用 `[` / `]` 以 100 ms 为步长微调，
偏移量会记进配置。

**Q：打开一个几百首的歌单，为什么要等几秒？**
A：歌单接口每页**硬限 30 首**——`pagesize` 传 100 甚至 500，服务端也只回 30 条，
而且响应里的 `count` 只是回显当页条数、不是总数。所以客户端只能逐页翻完。
实测一个 385 首的歌单需要 13 次请求，几秒钟。榜单和歌手歌曲同理。
搜索结果与歌单广场则按 `page_size`（默认 30）只取一页。

**Q：歌单显示的数量和实际对不上？**
A：歌单元数据里的 `percount`、`songcount` 这类字段实测恒为 `0`，不可用于展示。
界面上的"N 首"是**实际取到的条目数**，可信。

**Q：缓存占太多空间？**
A：默认上限 512 MiB，超了按 LRU 从旧到新回收。改 `cache_limit_mib`，
`--cache-limit 0` 表示不限制。

**Q：方向键 / 数字键没反应？**
A：先看是不是停在搜索框的输入态——此时字母键会插入文本。按 `Esc` 退出输入态再试。
若确认不是，用 `KUGOU_TUI_DEBUG=1 kugou-tui` 启动，日志会记录每个按键映射成了什么动作，
据此判断"按键没到程序"还是"被映射成了空动作"。

**Q：鼠标滚轮在终端里滚动了页面而不是列表？**
A：程序开启了鼠标捕获。若你的终端做了额外的鼠标转发（例如某些 tmux 配置），
可能会冲突；用终端自身的设置关掉即可。

---

## 平台相关

**Q：Windows 上怎么装？为什么 `./scripts/kugou-api-install` 跑不了？**
A：那几个脚本是 bash，PowerShell 里跑不了。Windows 用对应的 `.ps1`：

```powershell
.\scripts\kugou-api-install.ps1
.\scripts\kugou-tui.ps1
```

完整步骤（含工具链前置、终端要求、平台差异表）见
[INSTALL.md 的「在 Windows 上构建与运行」](INSTALL.md#在-windows-上构建与运行)。

**Q：Windows 上构建报 `linker link.exe not found` / `cmake not found` / `nasm not found`？**
A：前者是没装 MSVC 工具链——装 Visual Studio 生成工具并勾「使用 C++ 的桌面开发」。
后两者**不应该出现**：Windows 上 TLS 走系统自带的 SChannel 而不是 rustls，
`aws-lc-sys` 那坨 C 代码根本不参与编译。真遇到了说明你编的不是这个仓库当前的代码，
或者手动把 `Cargo.toml` 里的 TLS 后端改回了 `rustls`。

**Q：Windows 上托盘图标 / `playerctl` 怎么没有？**
A：那两个是 D-Bus 接口（MPRIS、StatusNotifierItem），Windows 没有 session bus，
相关代码**不参与编译**。不是坏了，是那边没有这套东西——见
[INSTALL.md 的功能差异表](INSTALL.md#6-与-linux-的功能差异)。`--print-config` 会
如实打印「系统托盘 : 不可用」。

**Q：Windows 上封面为什么是马赛克一样的色块？**
A：那是**半块字符画**，不是 bug。程序检测到终端不支持 Kitty / iTerm2 图形协议就
自动降级，而 Windows Terminal 两者都不支持。想省掉这份开销可以开 `lite_mode`。

**Q：图标变成一堆方块 / 问号？**
A：终端字体没有对应的字形。程序会先探测有没有 Nerd Font，没有就退回 ASCII 图标；
但 **Windows 上没有 fontconfig，探测一律落空**，装了 Nerd Font 也不会自动认出来——
设 `KUGOU_TUI_NERD_FONT=1` 手动打开（Linux / macOS 上也可以用这个变量覆盖探测结果）。

**Q：macOS 支持吗？**
A：**代码支持，CI 覆盖，但没有在真机长期用过**。macOS 走的是与 Linux 同一套
`cfg(unix)` 分支，音频走 CoreAudio，构建只需 Xcode 命令行工具。已知差异：

- **没有 MPRIS / 系统托盘**（macOS 默认没有 D-Bus），启动时日志里会有一条 WARN；
- **没有最小化窗口**（走的是 niri 的 compositor IPC）；
- 配置在 `~/Library/Application Support/kugou-tui/`，缓存在 `~/Library/Caches/kugou-tui/`
  （**不是** `~/.config` 与 `~/.cache`）；
- Terminal.app 下封面退半块字符画，iTerm2 能走图形协议。

细节见 [INSTALL.md 的「在 macOS 上构建与运行」](INSTALL.md#在-macos-上构建与运行)。

---
