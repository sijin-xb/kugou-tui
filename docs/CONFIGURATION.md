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
| `--basic-color` | — | 使用 16 色固定色板，适配老终端 |
| `--no-tray` | — | 不注册系统托盘图标（也可用配置文件里的 `tray = false` 长期关闭） |
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
| `KUGOU_TUI_CONFIG_DIR` | **整体覆盖配置根目录**。设成某个路径后，配置读写成 `<该路径>/config.toml`，不再用 `~/.config/kugou-tui`。便携安装（程序与配置一起放 U 盘）时有用；测试也靠它把落盘隔离到临时目录。缓存目录不受影响，仍可用 `--cache-dir` 单独指定 |
| `KUGOU_TUI_NERD_FONT` | `1` 强制按「装了 Nerd Font」渲染图标，`0` 强制按「没装」渲染 ASCII。留空表示自动探测（Linux 下查 `fc-list`；**Windows 没有 fontconfig，自动探测一律当作没装**，装了 Nerd Font 就得靠这个变量打开） |
| `KUGOU_TUI_DEBUG` | `1` 打开 DEBUG 级日志（按键、位置同步这类每帧日志默认关着，否则会把日志淹掉） |

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
```

`cover_fill` 决定**首页那块大封面**怎么填满它的区域。封面区是「多少列 × 多少行」，
换算成像素后几乎永远不是正方形，而专辑封面大多是正方形——**框和图的形状不一致时，
「铺满」「不变形」「不裁剪」三者只能同时满足两个**，必须挑一个放弃：

| 取值 | 铺满 | 变形 | 裁剪 | 说明 |
|---|---|---|---|---|
| `crop`（默认） | 是 | 否 | 是 | 居中裁剪。等价 CSS 的 `object-fit: cover` |
| `stretch` | 是 | 是 | 否 | 直接拉到区域大小，方图会被横向拉宽 |
| `fit` | 否 | 否 | 否 | 图完整，但框比图宽时左右露出底色 |

默认 `crop`：专辑封面基本居中构图，裁掉一点边框通常看不出来，而「框里空着一块」
是一眼就能看见的。觉得裁得太多就改成 `fit`。

运行时按 `,` 打开设置页，「封面铺满」可以直接切换，改完立即生效并落盘
——想比较三种效果不用重启。

### 歌词动画

`lyric_anim_ms` 是**换行时淡入淡出的时长上限**（默认 `200`，`0` = 关闭）。

终端没有子单元格定位：一个字符格就是一行，**位置动不了**（`Paragraph` 的滚动偏移
是整数，没有小数滚动，字体尺寸也固定）。所以「上滑」在终端里只能整行跳，真正能做
的过渡维度是**颜色**。这一项控制的就是那条颜色过渡：

- 换行瞬间旧行仍是亮的、新行还没亮；随后旧行淡出、新行淡入，中间那几帧两行都是
  半亮的——这就是「交叉淡化」，也是它和硬切的区别；
- 其余行的明暗层次（离当前行越远越暗）会跟着一起平滑重排，而不是整块瞬间跳变。

实际时长会按行距自适应，取 `min(本值, 该行到下一行的间隔 × 0.55)`：快歌的行只有
几百毫秒，按上限走会出现「上一次过渡还没走完就该换下一行」——看着不是顺滑，是拖沓。

下面三种情况会**强制关闭**过渡，与 `lyric_anim_ms` 的取值无关：

| 情况 | 为什么 |
|---|---|
| `lite_mode = true` | 它的卖点就是少重绘 |
| `basic_color = true`（16 色） | 没有中间色阶可取，`mix` 只能在两端二选一，淡入会退化成「过半时整块硬翻」——比不动画更怪 |
| 一次跨 3 行以上 | 那是拖动进度条或点歌词行跳转，逐行淡过去又慢又晕，直接吸附到目标行 |

不想看任何动画就把 `lyric_anim_ms` 设成 `0`，或按 `,` 进设置页把「歌词动画」
切到「关闭」。动画期间界面按 30fps 重绘（复用的是逐字推进本来就有的提速逻辑，
没有为它新增计时器）。

### 输出设备

`audio_device` 决定声音送到哪张卡，默认是**跟随系统默认**（该项留空即可）。

设置页最后一项「输出设备」显示的是**实际打开的那张卡**，并可以在「系统默认」和
枚举到的设备之间切换。它存在的理由是 Linux 上的一种典型故障：

```text
进度条在走、状态是「播放中」，但一点声音都没有
  └─ 声音被送到了另一张卡（比如没人接音箱的板载口）
  └─ 而且是直连硬件（绕过了 PipeWire），pactl list sink-inputs 里连这个程序都看不到
```

根因通常不在程序里：ALSA 的 `default` 被 `/etc/asound.conf` 或
`~/.asoundrc` 写死成了某一张卡（`pcm.!default { type hw card 2 }`）。**卡号会变**
——插拔 USB 声卡、换启动顺序都会让「card 2」指向别的东西，写死数字迟早出事。
修法是让 `default` 回到 PipeWire（`type pipewire`），或者直接删掉那个文件。

排查三板斧：

```bash
ls -l /proc/<pid>/fd | grep snd      # 看到 /dev/snd/pcmCxD0p = 直连硬件（绕过 PipeWire）
pactl list sink-inputs               # 播放时应该能看到自己的流，看不到就说明绕过了
aplay -D default /dev/zero -f cd     # 打不开（busy / 无此设备）就是 ALSA 配置有问题
```

不想动系统配置时，也可以在设置页里直接指定设备——但它同样是绕过 PipeWire 直连
硬件的，那张卡被别的程序占着就会打开失败。

候选列表是**筛过的**：ALSA 会把自己定义的所有 PCM 都报成设备（本机实测 52 项，
大部分是 `lavrate` / `samplerate` / `jack` / `oss` 这类插件），这里只保留「能给出
默认输出配置」且不是 `null`（Discard all samples）的，再按名字去重。选中一个打不开
的设备不会把播放弄哑——打不开就继续用原来那张，只在状态栏提示一句。

「系统默认」显示的是 `default` 那张卡的自述名，例如
`Default ALSA Output (currently PipeWire Media Server)`——它能直接告诉你声音交给了谁。

`qr_aspect` 用来矫正登录二维码的形状：终端字符的高通常是宽的 2 倍，此时用
半块字符（一个字符承载两行模块）画出来正好是正方形。**如果你觉得二维码被
拉长或压扁**，按 `L` 让二维码出现，量一个字符的高宽比，把它填到这一行：
`< 1.5` 改用全块字符（一模块占一字符一行），`>= 1.5` 用半块字符。

主题取值：`default`（冷蓝）、`graphite`（石墨，近乎无彩）、`sunset`（日落）、
`forest`（森林）、`neon`（霓虹）、`dracula`（暗紫）。写错或删掉这行会回落到
`default`。运行时按 `,` 打开设置页可直接切换，改完立即写入这个文件。

`quality` 的合法取值：`128`（默认）、`320`、`flac`、`high`、`super`、
`viper_clear`、`viper_atmos`、`viper_tape`。后三个是酷狗的「蝰蛇音效」系列，
**仅部分歌曲支持**，拿不到时服务端返回空地址，界面会给出提示。

`lite_mode` 开启后会关掉三样最吃资源的，换更低的占用（**听歌本身不受影响**）：

- 不下载 / 解码封面（图片解码 + 图形协议是最占内存的一块）
- 不算实时频谱
- 界面刷新降到 5fps，并且**不做动画提速**（逐字歌词推进与可视化频谱都退回 5fps）

实测常驻内存（VmRSS，release 构建）：空闲 14.2 MiB、播放中 16.9 MiB，开启后能再降
一截（降的主要是封面那几十到几百 KB 的解码位图）。在低配机器或电池供电时有用。
设置页可直接开关。分场景的完整数字见 [DESIGN.md](DESIGN.md#低资源占用)。

`tray` 控制**系统托盘**，默认开启：注册成 `org.kde.StatusNotifierItem` 之后，
Quickshell / waybar / KDE 之类的状态栏会显示一个图标，**右键弹出菜单**
（播放 / 暂停、上一首、下一首；能控制窗口时还会多一项「最小化 / 显示窗口」——
那一项**仅 niri 下出现**），鼠标悬停显示当前曲目。下面三种情况会自动跳过
（各只记一行日志，不影响播放）：

- 没有图形会话（既无 `WAYLAND_DISPLAY` 也无 `DISPLAY`，比如纯 tty、SSH 未转发）
- 没有 session bus
- 状态栏没有提供 `org.kde.StatusNotifierWatcher`

> **非 Unix 平台（Windows）上没有这一套**：托盘是 D-Bus 接口，那边相关代码
> 不参与编译，`tray` 的默认值也自动是 `false`。macOS 上相关代码会编译，但默认
> 没有 session bus，同样是静默跳过。

改成 `false` 或启动时加 `--no-tray` 即可完全关闭。**改动重启后生效**：KDE 风格的
watcher 只在进程启动 / 退出时同步托盘项，运行中没法可靠地增删。

音量、播放模式、歌词偏移会在退出时自动写回。

### 自定义键位

用 `[keymap]` 段把**动作名**映射到**按键**。动作名是 `Action` 变体的 snake_case：

```toml
[keymap]
reload = "f"                 # 刷新当前列表（默认 R）改到 f
help = "f1"                  # 帮助面板
quit = "Z"
cycle_artist_filter = "ctrl+f"
```

**未列出的动作沿用默认键位**，所以老配置文件不用改，只写想改的那几条。

按键名怎么写的规则：

| 形式 | 例子 |
|---|---|
| 单个字符（**区分大小写**） | `q`、`Q`、`/`、`[` |
| 具名键 | `space`、`enter`、`esc`、`tab`、`backtab`、`up`、`down`、`left`、`right`、`home`、`end`、`pgup`、`pgdn`、`backspace`、`delete`、`insert` |
| 功能键 | `f1` – `f12` |
| 带修饰键 | `ctrl+n`、`alt+1`、`shift+tab` |

可用的动作名（60 个，按用途分组）：

```
# 全局
quit  force_quit  help  toggle_sidebar  switch_source
# 导航
move_up  move_down  move_top  move_bottom  page_up  page_down
focus_next  focus_prev  submit  cancel
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

> `toggle_window` 是**唯一没有默认键位**的动作：最小化/显示窗口平时从**托盘菜单**
> 里点（右键 → 「最小化 / 显示窗口」），想用键盘就得自己在 `[keymap]` 里绑一条，
> 例如 `toggle_window = "M"`。它在 niri 之外**不生效**——判据是环境变量
> `NIRI_SOCKET` 是否存在，终端程序没法自己最小化窗口（Wayland 的 xdg-shell
> 没有这个请求），只能走 compositor 的 IPC。

**非法条目只跳过、不中断启动**，并在日志里记一条 WARN
（`~/.cache/kugou-tui/kugou-tui.log`）——键位错了只是不顺手，不该让程序起不来：

| 情况 | 日志 |
|---|---|
| 动作名不存在 | `键位配置：未知动作 "xxx"（键 "yyy"），已忽略` |
| 按键名解析不了 | `键位配置：无法解析按键 "yyy"（动作 xxx），已忽略` |
| 一个键绑了两个动作 | `键位配置：… 同时绑定了 …，以 … 为准` |

启动成功时会有一行 `已加载 N 条自定义键位`，用它核对生效条数。

> 带参数的动作（切换标签页、跳转到指定进度、输入字符）不在清单里——它们的值来自
> 运行时，配置文件里写不出完整语义。

### 会话持久化

退出时会把**播放队列 + 当前曲目 + 播放位置**存到
`~/.cache/kugou-tui/session.json`，下次启动自动恢复，界面提示
「已恢复上次会话：N 首 · 按 Space 继续播放」。

> 这个路径是**固定的**，`--cache-dir` / `cache_dir` 只改音频缓存目录，不会把
> 会话文件一起搬走（日志同理）。想连会话一起挪，只能改 `XDG_CACHE_HOME`。

**刻意不自动播放**——一开程序就出声很吓人，也可能在不该出声的场合。
恢复后按 `Space` 继续，而且**从上次的位置接着播**（界面提示「接着上次播：MM:SS」），
不是从头开始。

只对恢复的那首歌生效：如果你先去播了别的，那个位置就作废了——否则下次停在
任意一首上按播放都会从上次的位置开始。

它和配置分开存：配置是你手改的长期设置，会话是程序自己写的瞬时状态，
混在一个文件里会互相覆盖。

同一个文件里还记着 `vip_claimed_day`——上一次领取概念版每日 VIP 的日期。
有它才能做到「一天只领一次」（那个接口带风控，官方文档也写着「尽量别频繁调用」）。
删掉这个文件会让程序下次启动重新领一次，通常无害。

---
