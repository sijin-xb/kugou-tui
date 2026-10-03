# 配置

## 命令行参数

命令行 > 环境变量 > 配置文件 > 内置默认值。

| 参数 | 环境变量 | 说明 |
|---|---|---|
| `-a`, `--api-base <URL>` | `KUGOU_API_BASE` | KuGouMusicApi 地址，默认 `http://127.0.0.1:3000` |
| `-c`, `--cookie <COOKIE>` | `KUGOU_COOKIE` | 登录凭据，形如 `token=xxx; userid=xxx` |
| `-s`, `--search <KEYWORDS>` | — | 启动后立刻搜索该关键词 |
| `--volume <0-100>` | — | 初始音量 |
| `--cache-dir <DIR>` | — | 音频缓存目录 |
| `--cache-limit <MiB>` | — | 缓存上限，`0` 表示不限制 |
| `--tick-ms <MS>` | — | 刷新间隔，50–5000，调大可进一步降低 CPU |
| `--page-size <N>` | — | 搜索结果与歌单广场的每页条目数，5–200 |
| `--proxy <URL>` | `KUGOU_PROXY` | 访问 API 服务时用的 HTTP 代理 |
| `--sodam-cookie <COOKIE>` | `SODAM_COOKIE` | 汽水音乐的登录态，形如 `sessionid_ss=…; sessionid=…` |
| `--sodam-device-id <ID>` | `SODAM_DEVICE_ID` | 汽水的设备指纹，必须与签名凭证里的设备一致 |
| `--sodam-iid <ID>` | `SODAM_IID` | 汽水的 install id |
| `--sodam-x-helios <VALUE>` | `SODAM_X_HELIOS` | 汽水应用签名头；**VIP 整曲与无损音质靠它**，缺了只剩试听 |
| `--sodam-x-medusa <VALUE>` | `SODAM_X_MEDUSA` | 汽水应用签名头（与上面成对使用） |
| `--basic-color` | — | 使用 16 色固定色板，适配老终端 |
| `--no-tray` | — | 不注册系统托盘图标（也可用配置里的 `tray = false` 长期关闭） |
| `--no-api-start` | `KUGOU_API_AUTO_START=0` | 不自动拉起本机接口服务（服务由你自己管理，端口上没服务时直接报错） |
| `--api-start` | — | 只准备并启动本机接口服务就退出，不进界面；这里拉起的**留在后台**，用 `--api-stop` 停 |
| `--api-stop` | — | 停止本程序拉起过的本机接口服务 |
| `--print-config` | — | 打印最终生效的配置、缓存与日志路径后退出 |

```bash
kugou-tui --print-config        # 查看当前生效的配置与路径
kugou-tui --tick-ms 1000        # 省电模式：空闲 CPU 接近零
kugou-tui -s "海阔天空"          # 启动即搜索（需要登录）
```

### 不走命令行参数的环境变量

下面几个只认环境变量，因为它们要么是排查用的开关，要么影响的是「配置文件放哪」——
后者不可能写在配置文件里。

| 变量 | 作用 |
|---|---|
| `KUGOU_TUI_CONFIG_DIR` | **整体覆盖配置根目录**。设成某个路径后，配置读写成 `<该路径>/config.toml`，不再用 `~/.config/kugou-tui`。便携安装时有用；测试也靠它把落盘隔离到临时目录。缓存目录不受影响，仍可用 `--cache-dir` 单独指定 |
| `KUGOU_TUI_NERD_FONT` | `1` 强制按「装了 Nerd Font」渲染图标，`0` 强制按「没装」渲染 ASCII。留空表示自动探测（Linux 下查 `fc-list`；**Windows 没有 fontconfig，自动探测一律当作没装**） |
| `KUGOU_TUI_DEBUG` | `1` 打开 DEBUG 级日志（按键、位置同步这类每帧日志默认关着，否则会把日志淹掉） |
| `KUGOU_TUI_MEM_TRACE` | `1` 每 5 秒把 RSS 与当前曲目写进日志，排查「听久了内存涨」用 |
| `KUGOU_API_DIR` | 本机 KuGouMusicApi 的目录。设了之后**不再自动查找与下载**，指错位置直接报错 |
| `KUGOU_API_AUTO_START` | `0` / `false` / `no` / `off` 关闭自动拉起接口服务 |

```powershell
# Windows：便携目录 + 打开 Nerd Font 图标
$env:KUGOU_TUI_CONFIG_DIR = "D:\apps\kugou-tui\config"
$env:KUGOU_TUI_NERD_FONT  = "1"
.\kugou-tui.exe
```

## 配置文件

首次运行自动生成。Linux / macOS 下是 `~/.config/kugou-tui/config.toml`，
Windows 下是 `%APPDATA%\kugou-tui\config.toml`（Unix 上权限 `0600`，因为可能含 cookie）。
缓存与日志跟着各自的缓存目录走：Linux 是 `~/.cache/kugou-tui/`，
Windows 是 `%LOCALAPPDATA%\kugou-tui\`。

不确定实际读的是哪个文件时，用 `--print-config` 看（它把配置、日志、缓存三个路径
连同最终生效的值一起打出来）。

```toml
api_base = "http://127.0.0.1:3000"
cookie = "token=xxx; userid=xxx"   # 登录态，可手动填
dfid = "..."                        # 设备指纹，首次启动自动获取
volume = 0.7                        # 0.0–1.0
audio_device = "..."                # 输出设备名，留空 = 跟随系统默认
playback_mode = "sequential"        # sequential | repeat_all | repeat_one | shuffle
cache_dir = "/home/you/.cache/kugou-tui"
cache_limit_mib = 512               # 0 = 不限制
lyric_offset_ms = 0                 # 歌词整体偏移，正=延后
lyric_anim_ms = 200                 # 歌词换行淡入淡出的时长上限，0 = 关闭
tick_ms = 200                       # 界面刷新间隔（毫秒）
page_size = 30                      # 搜索结果与歌单广场的每页条目数
proxy = "http://127.0.0.1:7890"     # 可选
quality = "128"
basic_color = false
theme = "default"                   # 见下方「主题」
download_dir = "~/Music"            # 单曲下载保存到这里，留空也行
qr_aspect = 2.0                     # 终端字符「高:宽」比，见下方说明
lite_mode = false                   # 简易模式，见下方说明
cover_fill = "crop"                 # 首页大封面怎么铺满，见下方说明
tray = true                         # 系统托盘，见下方说明
api_auto_start = true               # 服务没起时自动拉起，见下方说明
api_dir = ""                        # 本机 KuGouMusicApi 目录，留空 = 自动查找

# ---- 各音源的连接与身份，以及当前选中的那个 ----
# 四个音源各自独立：登录态与设备标识**不能跨音源复用**。
# 详见「使用指南」的音源章节。
[sources]
active = "kugou"                   # kugou | kugou_concept | netease | sodam

[sources.kugou]
enabled = true
api_base = "http://127.0.0.1:3000"  # 本机 KuGouMusicApi（标准版）
cookie = "token=…; userid=…"        # 该音源自己的登录态
device_id = "…"                      # 该音源自己的 dfid
priority = 0                         # 数字小的排前面

[sources.kugou_concept]
enabled = true
api_base = "http://127.0.0.1:3001"  # 本机 KuGouMusicApi（platform=lite）
priority = 10

[sources.netease]
enabled = true
api_base = "http://127.0.0.1:3002"  # 本机 NeteaseCloudMusicApi
priority = 20

[sources.sodam]
enabled = true
api_base = "https://api.qishui.com" # 汽水**直连公网**，本机不需要服务
cookie = "sessionid_ss=…; sessionid=…"
device_id = "…"                      # 与下面 sodam_app 里的设备保持一致
priority = 30

# 汽水的应用级签名凭证：决定「VIP 整曲与无损能不能拿到」。
# 三者齐了才有整曲；缺了会退化成 30/60 秒试听（现象很像「会员没生效」）。
# 抓包方式见「使用指南」的汽水章节。
[sources.sodam_app]
device_id = "…"                      # 与 [sources.sodam].device_id 一致
iid = "…"                            # install id
x_helios = "…"                       # 请求头 x-helios（会过期）
x_medusa = "…"                       # 请求头 x-medusa（会过期）
```

### 接口服务的自动拉起

接口服务（KuGouMusicApi）是 Node.js 写的，`cargo install` 带不了它，所以由程序自己在
**运行时**补齐：启动时探一次端口，有服务就复用（退出时不碰它），没有才动手。查找顺序是
「配置的 `api_dir` → `/usr/share/kugou-tui/api/kugou`（发行包）→
`~/.local/share/kugou-tui/api/kugou` → `~/KuGouMusicApi`」，都没有就下载钉住的提交并
`npm install --omit=dev`。

| 场景 | 怎么做 |
|---|---|
| 服务常驻在别的机器上 / 交给 systemd | `api_auto_start = false`（或 `--no-api-start`） |
| 想提前装好、以后秒开 | `kugou-tui --api-start`（拉起的服务留后台，`--api-stop` 停） |
| 服务目录在别处 | `api_dir = "/opt/KuGouMusicApi"` 或 `KUGOU_API_DIR=...` |

**前提**：机器上要有 Node.js（≥ 12）与 npm。默认行为是「自己拉起的服务随退出停止」，
不留常驻进程。

### 封面铺满（`cover_fill`）

封面区是「多少列 × 多少行」，换算成像素后几乎永远不是正方形，而专辑封面大多是正方形
——「铺满」「不变形」「不裁剪」三者只能同时满足两个：

| 取值 | 铺满 | 变形 | 裁剪 | 说明 |
|---|---|---|---|---|
| `crop`（默认） | 是 | 否 | 是 | 居中裁剪，等价 CSS 的 `object-fit: cover` |
| `stretch` | 是 | 是 | 否 | 直接拉到区域大小，方图会被横向拉宽 |
| `fit` | 否 | 否 | 否 | 图完整，但框比图宽时左右露出底色 |

运行时按 `,` 打开设置页可直接切换，改完立即生效并落盘。

### 歌词动画（`lyric_anim_ms`）

换行时淡入淡出的时长上限（默认 `200`，`0` = 关闭）。

终端没有子单元格定位，一个字符格就是一行，位置动不了——所以「上滑」只能整行跳，真正能做的
过渡维度是**颜色**：换行瞬间旧行仍亮、新行未亮，随后交叉淡化；其余行的明暗层次跟着平滑重排。

实际时长按行距自适应，取 `min(本值, 该行到下一行的间隔 × 0.55)`——快歌的行只有几百毫秒，
按上限走会显得拖沓。三种情况**强制关闭**过渡：

| 情况 | 为什么 |
|---|---|
| `lite_mode = true` | 它的卖点就是少重绘 |
| `basic_color = true` | 16 色没有中间色阶，淡入会退化成「过半时整块硬翻」，比不动画更怪 |
| 一次跨 3 行以上 | 那是拖进度条或点歌词行跳转，逐行淡过去又慢又晕 |

### 输出设备（`audio_device`）

默认**跟随系统默认**（该项留空）。设置页最后一项显示的是**实际打开的那张卡**，可以在
「系统默认」和枚举到的设备之间切换。

它存在的理由是 Linux 上的一种典型故障：进度条在走、状态是「播放中」，但一点声音都没有
——声音被送到了另一张卡（比如没人接音箱的板载口），而且是**直连硬件**（绕过 PipeWire），
`pactl list sink-inputs` 里连这个程序都看不到。

根因通常不在程序里：ALSA 的 `default` 被 `/etc/asound.conf` 或 `~/.asoundrc` 写死成了某张卡
（`pcm.!default { type hw card 2 }`）。**卡号会变**（插拔 USB 声卡、换启动顺序都会让
「card 2」指向别的东西）。修法是让 `default` 回到 PipeWire（`type pipewire`），或直接删掉
那个文件。排查三板斧：

```bash
ls -l /proc/<pid>/fd | grep snd      # 看到 /dev/snd/pcmCxD0p = 直连硬件（绕过 PipeWire）
pactl list sink-inputs               # 播放时应该能看到自己的流
aplay -D default /dev/zero -f cd     # 打不开（busy / 无此设备）就是 ALSA 配置有问题
```

候选列表是筛过的：ALSA 会把自己定义的所有 PCM 都报成设备（本机实测 52 项，多数是 `lavrate` /
`samplerate` / `jack` / `oss` 这类插件），这里只留「能给出默认输出配置」且不是 `null` 的，
再按名字去重。选中一个打不开的设备不会把播放弄哑——打不开就继续用原来那张，只在状态栏提示。

### 其余几项

- **`qr_aspect`** —— 登录二维码的形状矫正。终端字符高约为宽的两倍时，半块字符画出来正好是
  正方形。觉得二维码被拉长或压扁，按 `L` 让二维码出现、量一个字符的高宽比填进来：
  `< 1.5` 用全块字符，`>= 1.5` 用半块字符。
- **`theme`** —— `default`（冷蓝）、`graphite`（石墨）、`sunset`、`forest`、`neon`、
  `dracula`。写错或删掉会回落到 `default`。按 `,` 可在设置页切换。
- **`quality`** —— `128`（默认）、`320`、`flac`、`high`、`super`、`viper_clear`、
  `viper_atmos`、`viper_tape`。后三个是「蝰蛇音效」，**仅部分歌曲支持**，拿不到时服务端
  返回空地址，界面会给提示。
- **`lite_mode`** —— 关掉三样最吃资源的（**听歌本身不受影响**）：不下载 / 解码封面、不算实时
  频谱、刷新降到 5fps 且不做动画提速。实测常驻内存空闲 14.2 MiB / 播放中 16.9 MiB，开启后能
  再降一截。分场景数字见 [DESIGN.md](DESIGN.md#低资源占用)。
- **`tray`** —— 系统托盘，默认开启：注册成 `org.kde.StatusNotifierItem` 后，Quickshell /
  waybar / KDE 会显示图标，右键弹出菜单（播放 / 暂停、上一首、下一首，niri 下还多一项
  「最小化 / 显示窗口」）。没有图形会话、没有 session bus、或状态栏没提供
  `StatusNotifierWatcher` 时自动跳过（各记一行日志）。**改动重启后生效**。非 Unix 平台上
  相关代码不参与编译，默认值自动为 `false`。

音量、播放模式、歌词偏移会在退出时自动写回。

### 自定义键位

用 `[keymap]` 段把**动作名**映射到**按键**。动作名是 `Action` 变体的 snake_case：

```toml
[keymap]
reload = "f"                 # 刷新当前列表（默认 R）改到 f
help = "f1"
quit = "Z"
cycle_artist_filter = "ctrl+f"
```

**未列出的动作沿用默认键位**，所以老配置文件不用改，只写想改的那几条。

| 按键写法 | 例子 |
|---|---|
| 单个字符（**区分大小写**） | `q`、`Q`、`/`、`[` |
| 具名键 | `space`、`enter`、`esc`、`tab`、`backtab`、`up`、`down`、`left`、`right`、`home`、`end`、`pgup`、`pgdn`、`backspace`、`delete`、`insert` |
| 功能键 | `f1` – `f12` |
| 带修饰键 | `ctrl+n`、`alt+1`、`shift+tab` |

可用的动作名（按用途分组）：

```
# 全局
quit  force_quit  help  toggle_sidebar  switch_source
# 导航
move_up  move_down  move_top  move_bottom  page_up  page_down
focus_next  focus_prev  submit  cancel  context_menu
backspace  delete  cursor_left  cursor_right  cursor_home  cursor_end
# 播放
play_pause  next  prev  seek_forward  seek_backward
volume_up  volume_down  toggle_mute
cycle_playback_mode  cycle_quality
toggle_lyric_panel  lyric_delay  lyric_advance
# 业务
open_search  open_settings  reload  load_more_search  download_current
queue_append  queue_play_next  add_all_to_queue  remove_from_queue  clear_queue
clear_cache  toggle_sort_order  open_ranks  open_cloud
login  claim_vip  cycle_artist_filter
add_to_cloud  remove_from_cloud  sync_to_cloud  delete_cloud_playlist  new_cloud_playlist
set_default_source  raise_source_priority  lower_source_priority
# 窗口（仅 niri）
toggle_window
```

> `toggle_window` 是**唯一没有默认键位**的动作：平时从托盘菜单里点（右键 → 「最小化 /
> 显示窗口」）。它只在 niri 下生效（判据是 `NIRI_SOCKET` 是否存在）——Wayland 的 xdg-shell
> 没有「最小化」请求，终端程序只能走 compositor 的 IPC。
>
> 带参数的动作（切换标签页、跳到指定进度、输入字符）不在清单里——它们的值来自运行时，
> 配置文件里写不出完整语义。

**非法条目只跳过、不中断启动**，并在日志里记一条 WARN
（`~/.cache/kugou-tui/kugou-tui.log`）：

| 情况 | 日志 |
|---|---|
| 动作名不存在 | `键位配置：未知动作 "xxx"（键 "yyy"），已忽略` |
| 按键名解析不了 | `键位配置：无法解析按键 "yyy"（动作 xxx），已忽略` |
| 一个键绑了两个动作 | `键位配置：… 同时绑定了 …，以 … 为准` |

启动成功时会有一行 `已加载 N 条自定义键位`，用它核对生效条数。

### 会话持久化

退出时把**播放队列 + 当前曲目 + 播放位置**存到 `~/.cache/kugou-tui/session.json`，下次启动
自动恢复并提示「已恢复上次会话：N 首 · 按 Space 继续播放」。

> 这个路径是**固定的**，`--cache-dir` / `cache_dir` 只改音频缓存目录，不会把会话文件一起搬走
> （日志同理）。想连会话一起挪，只能改 `XDG_CACHE_HOME`。

**刻意不自动播放**（一开程序就出声很吓人）。恢复后按 `Space` 继续，并且**从上次的位置接着播**，
不是从头。这个位置只对恢复的那首歌生效：先去播了别的就作废——否则下次停在任意一首上按播放都会
从上次的位置开始。

同一个文件里还记着 `vip_claimed_day`（上次领取概念版每日 VIP 的日期），有它才能做到「一天只领
一次」（那个接口带风控）。删掉这个文件会让程序下次启动重新领一次，通常无害。
