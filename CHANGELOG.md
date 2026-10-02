# 更新日志

本项目所有值得记录的变更都会写在这里。
格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [0.4.8] - 2026-10-02

### 文档

本轮**只动文档**，代码与 0.4.7 完全一致。之所以要单独发一版，是因为 crates.io 页面渲染的是
**包里那份 README**——文档改了不发版，页面就不会变。

- **全部文档按「这份是给谁看的」重写了一遍**，合计 1844 → 1473 行：

  - README 192 → 162：删掉塞进来的实现论证（Windows 为何不用 CMake / NASM、边下边播的
    4 MiB 窗口、内存测量方法），安装一节改成 cargo 优先；
  - INSTALL 590 → 425：删掉与程序内建功能重复的 fish workflow（探活 / 拉起 / 退出收摊现在
    程序自己做，只差一个 `git pull`），压缩 Windows 与 macOS 段的解释性段落；
  - CONFIGURATION 318 → 256：配置项里展开的「为什么」压成一句，论证留给 DESIGN.md；
  - USER_GUIDE 335 → 230：合并界面布局、桌面集成两节的重复描述；
  - FAQ：补上 0.4.7 引入的四个真实问题——找不到 node、首次启动为什么要等半分钟、服务起不来
    / 端口被占、内存只涨不落怎么查；
  - CONTRIBUTING：代码结构树补齐（`bootstrap.rs` / `mpris.rs` / `tray.rs` / `window.rs` /
    `source/` 等），发版一节补上 crates.io 这一步（`scripts/release` 不含它）；
  - `DESIGN.md` 与 `MAINTENANCE.md` **刻意不动**：它们是「为什么」的归档，详细本身就是价值。

- 文档里的事实做了一轮机器核验：43 条内部链接与锚点、14 处 `kugou-tui --xxx` 用法（对齐
  `cli.rs` 的 17 个长参数）、全部 `KUGOU_*` 环境变量与配置项名，均与代码一致。

## [0.4.7] - 2026-10-02

### 新增

- **接口服务的自动引导**（新增 `src/bootstrap.rs`）。第一次启动会自己把 KuGouMusicApi
  准备好并拉起，用户全程只敲 `kugou-tui`——这也是本版本发布到 crates.io 的前提。

  为什么需要它：`cargo install` 只放一个二进制到 `~/.cargo/bin`，既不带那个 Node.js
  服务，也没有 post-install 钩子可用。此前「装完了却不能用」：用户还得自己 clone
  仓库、装依赖、按平台起两个实例。

  - **探活优先**：端口上已经有服务就**直接复用，退出时不碰它**——它可能是用户自己
    起的，也可能是另一个 kugou-tui 实例起的；
  - 缺服务时才动手，按「配置的 `api_dir` → `/usr/share/kugou-tui/api/kugou`（发行包）
    → `~/.local/share/kugou-tui/api/kugou` → `~/KuGouMusicApi`」找，都没有才下载钉住
    的提交（`a5a9801`）并 `npm install --omit=dev`；
  - 只为**当前音源**拉起一个实例；切到另一个平台时补起（只 spawn，不再走安装——界面
    已经画在屏幕上，被 npm 冻住几十秒不可接受）；
  - **退出版图**：自己拉起的实例随退出停止，不留常驻 node。`--api-start` 是显式要求
    常驻的例外，用 `--api-stop` 停；
  - 安装带文件锁，两个实例同时首次启动不会往同一个 `node_modules` 里写。

  **前提没变：机器上要有 Node.js（>= 12）**。没有 node 时程序会直接说清楚并退出——
  Rust 这一侧变不出 Node 运行时。

- 配置项 `api_auto_start`（默认 `true`）与 `api_dir`；命令行 `--no-api-start` /
  `--api-start` / `--api-stop`；环境变量 `KUGOU_API_AUTO_START` / `KUGOU_API_DIR`。
- crate 发布到 [crates.io](https://crates.io/crates/kugou-tui)，`cargo install kugou-tui`
  可用。

### 修复

- **「按 Space 或 Enter 重试」此前是一句空头支票**（`app/update.rs` 的
  `StreamInterrupted` 分支）。边下边播第二次断流后只打一句提示就返回，没有任何按键
  接得住它：

  - `Space` 走 `toggle_playback()` 的普通起播路径，位置取 `state.resume`（会话恢复用
    的，此时为 `None`），于是**从头播**，不是从断点续；
  - `Enter` 走 `activate()`，按 `(Tab, Focus)` 分派成「播放选中歌曲 / 打开歌单」，与
    断掉的那首无关；
  - 而且 `start_playback` 只在 hash **不同**时才清 `stream_retried`，手动重播同一首
    之后额度不恢复，再断流连自动兜底都没有。

  现在断流后把断点存进 `pending_stream_retry`，按 `Space` 从那里续播并归还自动兜底
  额度；提示改为「按 Space 从断点重试」——`Enter` 的语义是「进入下一层 / 播放选中项」，
  保持可预测，不再写进提示。换歌与主动停止都会作废这个断点。

- **流式播放「自然播完」时没有取消下载任务**（`app/update.rs` 的 `TrackFinished`）。
  那里只写了 `active_stream = None`——放下的只是我们手里那一份 `Arc`，后台下载任务
  自己还持有一份，会继续把整首往内存窗口和 `.part` 文件里灌。切歌与主动停止两条路
  都调了 `cancel()`，唯独最常见的一条漏了。同时把「归还空闲页」的 `malloc_trim`
  挪到这里：它原先只在下一首 `load` 时执行，那时新的分配已经发生，刚还回去的页又
  被占回来了。

- **「听久了内存只涨不落」的根因：mmap 阈值定得太高**（`src/main.rs`）。原先钉在
  1 MiB，而酷狗封面解码出来是 300–900 KB 的位图，**正好落在它下面**，于是走 brk
  arena——free 只把页还给 arena，RSS 停在峰值，要等下一次 `malloc_trim` 才可能收回，
  这就是「没有泄漏但收不回」的来源。改成 256 KiB 后这些块直接走 mmap，`free` 即
  `munmap`，无条件还内核，不再依赖 trim 的时机。低于阈值的分配行为不变。

  依据是 `audio::engine::tests::mmap_threshold_effect_probe` 的对照数据（分配尺寸
  随轮次变化，两段都不 trim）：1 MiB 下 20 轮 +192 → +332 KiB 单调上升后停住，
  256 KiB 下恒定 +0。**固定尺寸测不出这个**——那总能复用同一批 chunk。

- **MPRIS 推送不稳**（`src/mpris.rs`、`src/app/desktop.rs`）。三个独立原因，都不是
  「信号没发出去」那么简单：

  1. **`mpris:trackid` 是一个固定路径**（`/org/kugou_tui/Track/1`）。MPRIS 规范把
     它定义为「曲目的唯一身份」，客户端（DMS / Quickshell / 各类状态栏）靠它判断
     「换歌了没有」。固定不变的结果是：元数据明明变了，按规范实现的客户端仍认为还是
     同一首，标题和封面停在上一首。现在按曲目 hash 派生（脏字符过滤后不足则回落到
     占位路径）。
  2. **信号发送失败被 `let _` 吞掉、连接断了也不重连**。session bus 重启
     （`systemctl --user restart dbus`、注销再登录）或总线名被别的实例抢走之后，旧连接
     上的信号会一直发不出去，而 `connected` 仍是 true——主线程照旧往里写快照，桌面
     组件却永远停在最后一帧，日志里一句都看不到。现在连续三次发不出去即判定连接已坏，
     置 `connected = false` 并在 5 秒后重连。托盘侧早有「每 5 秒对账自愈」，MPRIS 侧
     一直没有，这次补上。
  3. **位置走了 `PropertiesChanged`**。规范里 `Position` 是不通过它通知的属性：它一直
     在动，客户端按自己的时钟推算，只有跳变（seek）才需要被告知——那走 `Seeked`。
     早先发的是 `position_changed`，属于非标准用法；严格些的客户端收到不该出现的属性
     变化反而会重置本地推算，进度条就跳。现在跳变发 `Seeked`，并把它从
     `PropertiesChanged` 的判据里摘掉。

### 诊断

- `KUGOU_TUI_MEM_TRACE=1` 每 5 秒把 RSS 连同「可能累积的东西」的计数写进日志
  （`logger::rss_kib` + `App::tick`）：流式下载登记表、播放队列长度、命中区容量、
  当前曲目。光有 RSS 只能看出涨了，这些计数才能把范围收到某一条路径上。
  与 `KUGOU_TUI_DEBUG` 分开：后者会连按键日志一起打开，反而把趋势淹掉。
- 三个 `#[ignore]` 探针，各自打印每轮 RSS：
  `ui::views::player::tests::cover_swap_rss_probe`（反复换封面）、
  `audio::engine::tests::file_swap_rss_probe`（反复装载本地文件）、
  `audio::engine::tests::mixed_alloc_rss_probe` 与
  `mmap_threshold_effect_probe`（尺寸混杂的分配 vs. 阈值对照）。
  跑法：`cargo test --release <名字> -- --ignored --nocapture`。

  实测：换封面 40 轮、装载 30 轮、混合分配 40 轮，RSS 都在头两轮内落定后**恒定**
  ——换歌路径没有对象泄漏，剩下的就是「页还没还」。

## [0.4.6] - 2026-09-29

### 新增

- **托盘从「注册一次就完」补成完整集成**（`src/tray.rs`，+660 行）。此前托盘的
  菜单、滚轮、状态反馈都没有，且注册失败一次就永久放弃：

  - **滚轮控制**：图标上垂直滚动调音量、水平滚动快进 / 快退。只看 delta 的
    **符号**不按格数缩放——宿主给的绝对值不统一（有的 ±1、有的 ±120），
    按格数放会时大时小。端到端测试补了三条派发断言（上滚 / 下滚 / 右滚），
    零位移断言不派发。
  - **菜单加「退出」**：走正常退出流程（先保存配置与播放进度），与按 `q` 等价。
    此前最小化（niri）之后只能回终端按 `q`。
  - **播放 / 暂停、静音两项跟着状态切换措辞**：播放中显示「暂停」，其余显示
    「播放」；静音时显示「取消静音」。标签变化时菜单 revision +1 并广播
    `LayoutUpdated` + `ItemsPropertiesUpdated`（只含这两个 id）——两个信号都发
    是因为有的宿主只认其中一个。菜单补了分隔线把播放控制与其余项隔开。
  - **图标跟随播放状态**：暂停 / 停止时换**调暗**的同一张图（只压 alpha 到
    45%，颜色不变），并广播 `NewIcon`。加载态算「即将出声」，跟播放一样用亮图
    ——缓冲的两秒图标灰掉看起来反而像卡住。两套图启动时一次预渲染。
  - **注册自愈**：托盘线程从「注册失败即放弃」改为每 5 秒对一次
    `RegisteredStatusNotifierItems` 账，名单里没有自己就（重新）注册。修掉的
    场景：状态栏比播放器后启动、面板重启把 watcher 换了一轮——此前托盘要重启
    播放器才会出现，现在最多 5 秒自动补上。日志只在连接状态**翻转**时记一条，
    面板一直不在时不会每 5 秒刷一行 WARN。
  - **图标改版**：单音符（符头 + 符干）改为连尾双音符（♫），源
    `assets/tray.svg` 重画，内嵌 PNG 从 64px 提到 256px；预渲染尺寸从 22 / 64
    两档扩到 16 / 22 / 24 / 32 / 48 / 64 六档——少给尺寸会让某些宿主拿最接近
    的一档硬放大。

- **首页 / 可视化页的 ↑/↓ 切歌**（`app/update.rs` 的 `MoveUp` / `MoveDown` 分派）。
  这两页没有可导航的列表，方向键原本是空的；它们又正是「看着歌词 / 频谱听歌」的
  页面，切歌是最高频操作，所以划给播放控制。**有列表的页面（搜索 / 歌单 / 歌手 /
  榜单 / 云端 / 队列 / 设置）不改**——抢走列表导航等于抢走核心交互。
  `CHEATSHEET` 补一条说明，漏项能被既有的键位表测试查出来。

- **`audio/resample.rs`：hi-res 母版的抗混叠重采样**。rodio 的
  `SampleRateConverter` 是**无滤波的线性插值**（模块自述 "simple linear
  interpolation"）：44.1k → 48k 这类上采样没问题，但 88.2 / 96 / 176.4 kHz 的
  母版在 48 kHz 设备上是**降采样**——超过 24 kHz 的内容会原样镜像折叠回可听
  频带，高频还多一层约 1 dB 滚降。现在用 rubato 的 sinc 重采样器
  (`sinc_len=256`、`f_cutoff=0.95`、BlackmanHarris2 窗) 替掉线性插值，并把
  `sample_rate()` 声明成设备率，rodio 侧的转换随之成为恒等变换。上采样不包装
  （无混叠问题，不值得付 sinc 的 CPU）。**已知边界**：结尾会多出至多一块
  （约 30 ms）的静音尾——`FixedIn` 的输入块定长，最后一块不足的部分以零填充。
  听不见，也不影响进度条（时长声明来自内层源）。

### 修复

- **进度条只能往前拖，回拖 / 快退 / 歌词回跳全部失效**（`audio/engine.rs::reload_at`
  与 `audio/streaming.rs::reader_from_start`）。病根在解码器：symphonia 的 FLAC
  解码器对**没有 SEEKTABLE** 的文件不能向后跳，demuxer 直接返回 `ForwardOnly`。
  酷狗给的无损档实测正是这种（`ffprobe` 读不到 seektable，原始包扫描也找不到
  `SEEKTABLE` 块），于是「往后拖」全灭。修复是绕开它：用同一份数据源**重建
  解码器**，新解码器从文件头出发，跳到任意位置都是前向跳，必然成功（代价一次
  几十毫秒重建）。流式缓冲那一路额外要求「归零读指针的克隆」——原缓冲的读指针
  停在老解码器的位置上，直接克隆会让新解码器从半截开始，「跳回前面」又退回
  向后 seek。

- **hi-res 母版听着比别的客户端「毛」**——上一条重采样修的就是它。《起风了》
  是 88.2 kHz 母版，`highpass=24k` 后仍有约 −50 dB 的真实内容，而它与
  moekoemusic 拿到的文件**音频 MD5 完全相同**——两边听感不同的病根在降采样
  那一步，不在文件选择。抗混叠有测试钉住：30 kHz 音降采样后必须被压到 −26 dB
  以下，带内 1 kHz 保幅，seek 后滤波器状态必须重建。

- **点歌词总是跳到隔壁句**（`ui/views/player.rs` 的命中区登记）。`hit_zones`
  的 row 是**屏幕行号**，`index_at` 先做 `row - rect.top()`；正文明细区上沿取错
  时，整块命中区会整体偏一行或几行。修复把命中区锚定在**这一帧真正渲染的
  布局**上：行数取「可视行」与「内容剩余行」的小者，上沿取内容区顶 + 实际
  内边距（`offset < first_display` 时内容整体下移的行数）。
  **实测**（pty 直驱 harness，绕开 tmux 对鼠标上报的改写）：暂停后同一屏幕行
  连点 3 次，`time_ms` 三次全同；`row - top + first == display` 对 21 行全部
  自洽；相邻屏幕行的 `display` 步长恒为 1；从上往下 `ms` 单调不减。

- **`flac` 档给 mp4 容器时的静默串味**（`api/catalog.rs`）。mp4 是另一条转码
  链路的产物，母版和 flac / mp3 不同，播它就是「听着和别的客户端不一样」。
  参照 moekoemusic 的 `extName == 'mp4'` 判定，遇到 mp4 候选落到下一档。

- **内存破百 MB 不回落**（`src/main.rs` + `audio/engine.rs`）。glibc 默认用
  **动态** mmap 阈值：一次大分配（封面解码、下载缓冲）会把阈值抬到它的大小，
  之后同样大的块改从 arena 里切——`free` 只把页还进 arena，RSS 从此抬到历史
  峰值不回落。现在把阈值钉死在 1 MiB，大块一律 mmap（`munmap` 即还 OS），
  并在换歌这个天然边界调一次 `malloc_trim`。仅 Unix（Windows 的 MSVC 堆本来
  就积极归还）。

- **托盘相关文档与实现不符**：`tray.rs` 模块注释与 `USER_GUIDE` 还写着
  「不实现菜单」「watcher 只在进程启动 / 退出时同步」，与已实现 DBusMenu 与
  5 秒自愈的代码不符，已改成分键位说明（左键 / 滚轮 / 右键 / 中键 / 图标状态）。

### 测试

本版新增 6 条守护测试，都钉在**移除修复就会立刻失败**的那一层：

| 测试 | 钉住的层 |
|---|---|
| `late_song_clicks_do_not_shift_by_the_offset_gap` | 末屏 `offset` 撞上 `max_offset` 后的点击错位 |
| `hit_zone_top_anchors_to_the_first_content_row` | 命中区上沿 = 内容首行 |
| `rebuilt_decoder_can_seek_backward_from_the_start` | 重建解码器能前向跳到较早位置并继续出样本 |
| `backward_seek_lands_near_the_target_not_at_the_start` | 归零读指针后能跳到目标而非回到开头 |
| `ultrasonic_tone_is_suppressed_not_aliased` | 30 kHz 被抗混叠压到 −26 dB 以下 |
| `in_band_tone_passes_with_its_amplitude` | 带内 1 kHz 保幅（±1.5 dB） |

总计 366 → 370 条（另有 1 条 ignored）。

## [0.4.5] - 2026-09-29

### 修复

- **多声道无损文件只播了前两个声道，听到的不是这首歌**（新增 `audio/downmix.rs`，
  接线在 `audio/engine.rs::build_decoder`）。触发场景：播《东京不太热 (DJ Z新豪版)》
  （`hash=1953202D07B954E785CA5249E1A64B3C`）时，`flac` 档给的是一个 **4.0 声道**的
  FLAC，而那个文件里**歌在后两个声道**：

  | 声道 | dBFS | 与同曲 128 kbps mp3 的相关性 |
  |---|---|---|
  | ch0 FL | −21.0 | +0.35 |
  | ch1 FR | −20.6 | +0.39 |
  | ch2 BL | −11.7 | **+0.90** |
  | ch3 BR | −12.8 | **+0.89** |

  四路全混与 mp3 的相关性是 +0.999——也就是说前两个声道根本不是这首歌。而 rodio 0.22
  的 `ChannelCountConverter` 降声道时是「保留每帧前 N 个样本、其余直接丢弃」
  （`conversions/channels.rs`），于是我们实际只播了 FL/FR，用户听到的是另一段音频，
  表现成「音频版本和其他客户端（moekoemusic）不一致」。

  现在 `build_decoder` 的两个出口统一过 `downmix::to_stereo`：偶数索引声道 → L、
  奇数索引声道 → R 取平均，`channels()` 恒为 2（这样 rodio 侧那次转换就成了空操作）。
  单声道/立体声是恒等变换，不影响绝大多数歌。**实测**（走 `build_decoder` 的真实解码
  路径、素材是酷狗给的原始文件）：下混结果与正确立体声混音的相关系数从 +0.35 升到
  **+0.998 / +0.997**。两条守护测试：`decoded_multichannel_files_are_downmixed_to_stereo`
  用自造的 4 声道 WAV 走一遍 `build_decoder` 并断言 `channels() <= 2`；
  `the_mixer_hears_the_song_only_because_we_downmixed_first` 用 `rodio::mixer::mixer`
  搭一个不依赖声卡的 mixer，**对照**证明「源直接进 mixer 会只剩前两个声道」、
  「先下混再进则听到的是正确混音」——它钉的正是 bug 发生的那一层。两条都是
  把那层拆掉就立刻失败。

  **取平均而不是求和**：求和更贴近这个文件的立体声母版（mp3 的 L ≈ `FL+BL`），
  但只要两个声道相关就会削顶；平均天然安全（`|out| <= max|in|`）。代价是这一对
  声道互不相关（+0.03）而母版是它们的和，所以下混后整体比参照低约 **6 dB**。
  音量用户可调，削顶是实打实的失真——取轻的那一头。详见 `downmix` 模块顶部。

  顺带修掉同根因的另一处：`LevelMeter` 只采第 0 声道做电平/频谱，4 声道输入时
  **频谱可视化画的也是错的音频**；下混层放在它前面之后自动跟着对。

- **`/song/url` 的静默降级没人管**（`api/catalog.rs::downgrade_note`）。实测请求
  `quality=flac` 对没有无损档的歌会回 `status=1` + `extName=mp3` + `bitRate=128000`
  （3,749,296 B），而 `extract_stream_url` 只检查 `status`——界面会一直标着 flac、
  实际放 128 kbps，且永远查不出来。现在按「容器 + 码率」粗排一次档位，实际比要的低时
  给出人话（`音质已降级：请求 flac，服务端实际给了 128 kbps mp3`），经
  `StreamUrl::reason` 透到界面。`super` 与蝰蛇系列不参与比较（接口层没有可对照的
  容器/码率），mp3 不报码率时也不下结论——宁可不说，也不能误报；`mp4` / `m4a`（AAC）
  按「有损」参与比较，所以「设了无损却拿到 AAC」也会报出来（这一条的依据是参照实现
  moekoemusic 专门挡 `extName == 'mp4'`，不是我们自己实测到的）。
  回归测试 `song_stream_url_reports_a_silent_downgrade` 用手写服务端端到端跑一遍。

- **降级提示此前会被整个吞掉**（`app/update.rs`）。`StreamUrl::reason` 只在 `is_trial`
  分支被读，于是「蝰蛇音质没权限、已降到标准档」这条提示从来没显示过。现在非试听分支
  也会把它作为普通提示报出来。

- **候选音质可能跨版本串味**（`api/catalog.rs::parse_quality_candidates`）。取链时会把手上的
  多个 hash 逗号拼接一起发给 `/privilege/lite`，响应因此是**多个 item**；老写法把全部 item
  的 variant 混进一张表、谁先出现谁赢，而服务端并不保证 item 顺序——某个 hash 一旦指向同曲的
  **另一个版本**（不同版 / 翻唱 / 铃声），就可能挑到别的版本的文件。现在分两遍：先只认与主 item
  （按 `hash` 认出）同一个 `album_audio_id` 的条目，拿不到才放开到全部——第二遍必须留着，
  那是「搜索给的 hash 已下架、歌在 `audio_info` 的另一个 hash 下」那条修复路径。
  回归测试三条：`candidates_never_borrow_a_quality_from_another_version`、
  `candidates_identify_the_primary_item_by_hash_not_by_position`、
  `candidates_fall_back_to_other_hashes_when_the_primary_has_nothing`。

### 变更

- **打包脚本在普通 CI 里也跑一遍**（`ci.yml` 的 Linux 与 macOS 两栏）。发行包此前只在
  `release.yml` 里产出，而那是 tag 推出去之后——**tag 不可撤销**，打包脚本要是漏了文件，
  只能发一个坏包或者认了。这个项目真踩过：0.3.7 那版漏了三个脚本，非 Arch 用户解压后
  配不起接口服务。现在任意一次 push 都会打一次发行包并传成 artifact，问题在推 tag 之前
  就暴露；macOS 那一栏顺带覆盖 `make-release-tarball` 里为 BSD 工具（`readlink`、
  `shasum`）写的分支——那几段只有在那边才会被执行到。

- `api/model.rs` 里「`audio_id` 拆成两个字段分别存，取链接时挨个试」那段注释与实现不符
  （只有一个字段，且 `/song/url` 刻意不发送任何 album 系 id）。改成如实描述，
  免得下一个人照着注释去找一段不存在的重试逻辑。

## [0.4.4] - 2026-09-28

### 变更

- **拆 `app/update.rs`**（4728 → 4144 行）。它是全程序唯一改状态的地方，长期膨胀到
  4700 行，改一处要读的上下文太多。已切出三块，**全部是纯搬移、逻辑零改动**，
  搬完归一化可见性后逐字节比对过（`search` / `playback` 的 `diff` 输出为空，
  `navigation` 只差一个尾随空行）：

  | 新模块 | 行数 | 内容 |
  |---|---|---|
  | `app/navigation.rs` | 237 | 切页、焦点、选择移动、`go_back` |
  | `app/playback.rs` | 320 | 起播 / 切歌 / 进度 / 音量 / 静音 / 歌词偏移 |
  | `app/search.rs` | 142 | 提交搜索、加载更多（含 `SEARCH_MAX_PAGES`） |

  **边界比标题重要。**「播放控制」那一节里挤着五个不属于播放的方法，没有跟着搬走：
  `sync_mpris` / `sync_tray`（由 `tick()` 每拍调的输出适配器）、`toggle_window`
  （niri 的 compositor IPC）、`client_for`（全文件共用的客户端构造辅助，6 处调用）、
  `request_lyric`（结果处理在 `handle_loaded` 里，请求与处理同文件才省上下文）。
  判断标准是「调用方在哪、结果谁处理」，不是「它写在哪个标题下面」——宁可模块小一点、
  边界干净，也不为了凑体积把邻居一起搬走。

  **切完之后复查依赖，发现并修掉了一处叶子互依。** `activate()`（Enter 键按
  `(Tab, Focus)` 决定该搜索、该播放还是该打开）原本跟着「导航」切进了
  `navigation.rs`，于是 navigation 去调 search 与 playback，`navigation ↔ search`
  变成双向。`startup_search()`（`--search` 的启动胶水）同理让 search 反向依赖
  navigation。两者搬回 `update.rs` 的分派层后，依赖变成星形：`update` 单向调用三个
  模块，`navigation` / `playback` 只回调 `update`，`search` 谁也不调。
  **搬移等价不代表边界正确——边界错了不报错，只是把耦合从文件里挪到模块之间**，
  所以每切完一块都要重新数一遍模块调用图。

  代价说明白：Rust 的方法私有性按「写 `impl` 块的模块」算，不是按类型——方法搬走后
  另一侧调用会报 `E0624`，得逐个改成 `pub(super)`，且**两个方向都要改**。
  这是拆分的固有成本，不是设计缺陷；编译器会把清单列全。

  剩下的 `cloud.rs` / `settings.rs` 按同样粒度继续，一次一块。规矩与进度记在
  `docs/MAINTENANCE.md` §1.8。

- **拆 `app/update.rs` 第二轮**（4144 → 2958 行），把上面说的 `cloud.rs` /
  `settings.rs` 切完了，另加一块 `desktop.rs`。同样是纯搬移：116 个函数逐个比对
  签名 + 函数体（空白折叠、`pub(super)` 归一化）**全部逐字符存活**，唯一被删的
  是一份重复的实现（见下面「修复」第一条）。

  | 新模块 | 行数 | 内容 |
  |---|---|---|
  | `app/cloud.rs` | 937 | 登录 / 音源切换 / VIP / 云端歌单 / 账号资料 |
  | `app/settings.rs` | 728 | 设置页：候选值 + 显示文本 + 三个交互 |
  | `app/desktop.rs` | 106 | MPRIS / 系统托盘 / 窗口控制 |
  | `app/navigation.rs` | 275 | 另接了数字键与 `v` 键打开音源页 |

  依赖仍是**星形**且没有互相调用的一对：`update` 指向 6 个模块，模块只回指
  `update` / `mod`。一处**故意接受**的跨模块调用：`navigation` 调 `settings`
  的 `move_settings`（设置页的光标移动归设置页，理由记在 MAINTENANCE §1.8）。

- **修四处错位的文档注释**。它们的共同症状是「一段注释紧贴在另一个函数的文档
  前面，被 Rust 并成同一个文档」，于是 `fetch_user_info` 的文档开头讲的是 VIP、
  `handle_digit` 的文档开头讲的是 `v` 键切换音源、`open_quality_picker` 的开头
  讲的是下载歌曲、`describe_song` 的开头讲的是音质标签。四处各自归位。

### 新增

- **`scripts/kugou-api.ps1`：Windows 侧的服务管理**（start / stop / restart / status / logs）。
  此前 Windows 上没有任何办法停掉后台的 node：启动器只做「探活 → 没起就拉起」，
  想停只能去任务管理器按名字猜着杀，而猜错会把别的 Node 项目一起带走。现在两个平台
  共用同一套 PID 文件约定（`<PID> <端口>`，空格分隔，落在缓存目录的 `api-<实例>.pid`），
  启动器拉起的实例，`kugou-api.ps1 stop` 停得掉。

- **`kugou-api logs`**：直接跟随日志（`logs lite` 跟概念版）。此前要看日志得先
  `status` 拿到路径，再自己 `tail`。

- **启动器补 `--dry-run`**（bash 侧；PowerShell 侧本来就有）：只打印解析到的配置、
  音源、实例、探测地址、服务目录与二进制路径，不启动任何东西。排查「为什么它说服务
  没起」时不用再靠猜。

- 启动器在拉起服务前**先查 `node`**。此前没装 Node 要等满 20 秒，再从日志尾部那句
  `ENOENT` 里推断原因。

- 启动器在 macOS 上会检查二进制的 `com.apple.quarantine` 属性，并给出 `xattr -d`
  那条命令。签名缺失的二进制被 Gatekeeper 杀掉时终端只显示 `Killed: 9`，看起来像
  程序自己的 bug。

### 修复

- **网易云的歌单 / 榜单会被静默截断**（`source/netease.rs`）。`playlist_tracks_all`
  用「不满一页 ⇒ 结束」停翻页，而 `song_from_json` 会过滤条目（缺 hash、字段类型
  不对），一页 500 条剩 499 条是常事——只要撞上，整张表就停在那儿，用户看到的是
  一个短了一截的歌单，没有任何报错。酷狗那侧（`api/catalog.rs::collect_all_pages`）
  一直是「只有空页才停」，两侧判据不一致本身就是这个 bug 的信号。

  改成「只有空页才停」，判据抽成自由函数 `should_continue_paging`（`got > 0`，
  签名里**故意没有** `page_size`——页大小一旦进判据，截断就回来了）。

  改它的前提是「越界 offset 到底返回空数组还是报错」，这个之前没法实测（本机只跑着
  酷狗概念版服务 `:3001`）。这次起了 NeteaseCloudMusicApi（`:3002`）实测：
  `/playlist/track/all?id=3778678&limit=500&offset=500`（该歌单 200 首）与
  `offset=999999` 都返回 `{"songs":[],"privileges":[],"code":200}`——**空数组、
  不报错**，也没有把 offset 夹回末页返回重复内容。所以多打一次越界请求是安全的，
  代价只是整表加载末尾多一个请求（这个接口一次就要几秒）。

  回归测试 `paging_stops_only_on_an_empty_page`：把判据退回 `got >= 500` 时它当场
  失败（`1 条也该继续翻页`）。另有一条 `an_out_of_range_page_is_empty_and_
  therefore_ends_paging` 把上面那段**服务端行为的实测结论**钉住——哪天上游改成
  越界就报非 200，`playlist_tracks_all` 的 `?` 会把错误抛上去，那时要重新实测
  并改这条。`docs/MAINTENANCE.md` §5、§7 同步。

- **`;`（打开歌曲右键菜单）改不了键位**（`keymap.rs`）。它在 `resolve_normal` 与
  `CHEATSHEET` 里都有，帮助面板也照常显示，`docs/CONFIGURATION.md` 的键位表里也列着，
  但 `action_from_name` 的动作名表里**没有 `context_menu`**——用户照文档写
  `context_menu = "m"` 时得到的是「未知动作 context_menu」，而那个动作明明存在。
  `CONTRIBUTING.md` 把「记得同步这张表」写成了一条人工步骤，这次就漏了。

  动作名表从 `match` 改成了 `const REBINDABLE`（数据），于是「帮助面板里有默认键位的
  动作，是否都能绑上名字」可以机械检查：新测试拿 `CHEATSHEET` 与 `REBINDABLE`
  两份名单对着数，去掉修复后它会精确报出
  `「;」（打开歌曲右键菜单）→ ContextMenu`。另加一条表本身的自检
  （不重名、全小写 snake_case）——重名会让 `find` 静默取到先出现的那条。
  `docs/CONFIGURATION.md` 的清单与计数（60 → 61）一并跟上。

- **歌词译文会整体串位一行**（`api/lyric.rs`）。`parse_lrc` 与 `attach_translations`
  各写了一份「这一行算不算歌词」的判据：前者看 `parse_krc_words` 的返回值是否为空，
  后者看 `clean_krc_markup` 是否为空。两者在「`<` 之前有字、却没有配对的 `>`」的行上
  分道扬镳（歌里写了 `宝贝<3` 这种），于是 `attach_translations` 算出来的行号多出一项，
  **之后每一行的译文都往后串一位**——用户看到「译文和原文对不上」，没有任何报错。

  现在判据只有一处（`has_lyric_content`），两边都调它。顺带修掉同源的另一个症状：
  `parse_krc_words` 只输出标记**之后**的字符，所以那一行以前会被整行丢掉——
  `<` 之前的字也是正文，现在收下（这些字没有逐字时间，于是退回整行高亮）。

  回归测试 `a_line_with_a_stray_angle_bracket_does_not_shift_translations`：
  去掉修复后它会报 `left: [译1, 译2, 译3]` / `right: [译1, 译3, 译4]`。

- **封面「不变形」模式在一段退化区域上会 panic**（`ui/views/player.rs::fit_box`）。
  `rows.clamp(1, area.height)` 在 `area.height == 0` 时直接炸——`Ord::clamp` 要求
  `min <= max`，而 `clamp(1, 0)` 会 `assert!(min <= max)`。

  现在**没有**可复现的触发路径（实测 `render_home` 的布局在 4~24 行之间最低给封面栏
  4 行，`Min(6)` 优先于 `Length(8)`），但这是布局的巧合而不是这里的保证——
  封面区的布局本身就改过一次（见那段「之前让封面框按图片比例自己算高度」的注释）。
  现在退化区域直接返回空矩形：既不 panic，也不会把框撑到区域外面去
  （硬凑 1×1 会让 `area.width - columns` 当场下溢）。回归测试同时钉住
  「零尺寸不 panic」与「算出来的框永远不超出区域」。

- **Windows 上的 PowerShell 脚本会满屏乱码**（`scripts/*.ps1`）。四个 `.ps1` 都没有
  UTF-8 BOM，而 Windows 自带的 Windows PowerShell 5.1 在文件没有 BOM 时**按当前
  ANSI 代码页解码**——中文 Windows 上是 GBK，于是脚本里那几十条面向用户的中文提示
  （「找不到 node」「已在运行」……）全变成乱码。

  为什么一直没发现：CI 的 Windows 那栏用的是 `shell: pwsh`，也就是 PowerShell 7，
  而 7.x 默认按 UTF-8 读——**CI 绿了不代表用户那边正常**。现在四个文件都加上 BOM
  （只改解码方式，逐字节确认过除开头 3 字节外内容完全没动），并把这条写进
  `docs/MAINTENANCE.md` §1.7。

- **并发翻页的整表顺序是随机的**（`api/catalog.rs`）。`collect_all_pages` 一批页
  同时发出去，然后在 `JoinSet::join_next()` 里直接 `all.extend(songs)`——而
  `join_next()` 给的是**任务完成顺序**，不是页码顺序。于是打开任何超过 6 页
  （>180 首）的歌单 / 榜单 / 歌手页，整表的页序都是乱的。榜单尤其致命：它的
  「顺序」就是榜单内容本身，TOP500 被随机排列等于榜单没了。

  现在一批的结果先按页码收进 `collected`，排好序再拼。回归测试用手写的假服务端
  让页码**必然倒序返回**（第 N 页等 `60ms × (7 - N)`），所以这条不是靠运气过的：
  去掉排序后它稳定地给出 `h1, h7, h6, h5, h4, h3, h2`。

- **同一首歌上会同时跑两条流式下载**（`audio/download.rs`）。连按两次 Enter 就是
  两条 `start_streaming` 落在同一个目标上（`App::active_stream` 要等到攒够开头才
  被握住，中间那段窗口谁也拦不住第二条）。两条流会互相破坏：后开的那次
  `truncate(true)` 把先写的字节从盘上抹掉（而缓冲窗口之外的数据正是靠 `pread`
  这个文件读回来，读回空洞就是噪音），失败 / 取消时的 `remove_file` 还会删掉
  对方的文件，让它改名失败、白下一整首。

  现在 `Downloader` 按临时文件路径登记在跑的流，第二条直接复用第一条的缓冲、
  不再起任务。顺带把整首下载和流式下载的临时文件分开（`x.mp3.part` 与
  `x.mp3.stream.part`）——后台预取下一首时，用户完全可能正好切到那一首。

- **音质档位有两种说法**。`app/update.rs` 里另有一份私有的 `quality_label`，只认
  6 档，比 `app/settings::quality_label` 少了 `viper_atmos` 与 `viper_tape`。于是
  按快捷键切到那两档时状态栏显示原始串 `viper_atmos`，而设置页同一项显示
  「蝰蛇全景声」。现在只有一份实现，并加了一条测试：每个可选音质都必须有中文名，
  不许退回原始串。

- **网易云歌手页的「N 首」是专辑数**。`source/netease.rs` 把 `/top/artists` 的
  `albumSize` 填进了 `song_count`，而它是专辑数，歌曲数是 `musicSize`。类型正确、
  语义错误，界面上每个歌手后面都挂着一个错数字且不会有任何异常。改成 `musicSize`，
  拿不到就不显示（比显示错的强）。

- **分块并发下载会写出内容损坏的缓存文件**（`audio/download.rs`）。分块下载只检查
  `status.is_success()`，于是服务端（或中间 CDN / 代理）忽略 `Range`、回 `200 OK`
  加整首内容时也会照写：四个任务各自 `seek(自己的起点)` 再写整首，互相覆盖，得到一个
  「长度对、内容全错」的文件，还照常改名进缓存——之后每次播放都命中它。

  现在要求三件事同时成立才继续写：状态码必须是 `206`、`Content-Range` 必须能解析、
  解析出的范围必须**逐字节等于**请求的范围（顺带核对总长度与 HEAD 一致）。任何一条
  不满足就退回单连接重下——宁可慢一次，不能坏一个缓存文件。

- **不完整的下载会被当成成功**。完成条件此前是「写入的字节数大于 0」，而 `.part`
  已经 `set_len(total)` 预分配过，文件大小本身就证明不了完整性。现在要求每个块写满
  自己那一段、总和等于总长度，并在改名之前 `sync_all`；单连接路径在服务端给了
  `Content-Length` 时也要求收满。

  回归测试用 std 的 `TcpListener` 起了一个最小的 HTTP 服务端（不引新依赖），覆盖
  正常 206、服务端忽略 Range 回 200、`Content-Range` 与请求不符、传输中断四种情况。
  把三处防护逐个去掉后，「服务端忽略 Range」那条会返回 4 MiB（真实长度 1 MiB）——
  也就是修复前那个坏文件。

- **Windows 的发行 zip 里没有 PowerShell 脚本**。`build-windows.ps1` 只拷了三个 bash
  脚本，而 README 与 `docs/INSTALL.md` 让 Windows 用户跑的正是
  `.\scripts\kugou-tui.ps1` / `.\scripts\kugou-api-install.ps1`——解压后这些文件不存在，
  第一步就卡住。现在按「文档提到过的都必须在包里」为准，六份脚本一起带。

- **三个 bash 脚本在 macOS 上跑不起来**。逐条列出，因为每一处都会直接中断脚本：

  - `declare -A`（`kugou-api`、`kugou-api-install`）需要 bash 4，而 macOS 自带的
    `/bin/bash` 是 3.2——报 `declare: -A: invalid option`，一行都跑不了。改成
    `case` 分派 + 间接展开。
  - `readlink -f`（三个脚本）是 GNU 扩展，macOS 的 `readlink` 报
    `illegal option -- f`。改成逐层解软链（`make-release-tarball` 里早就是这么做的）。
  - 配置与缓存目录在 macOS 上找错了地方：`dirs` 给的是
    `~/Library/Application Support` 与 `~/Library/Caches`，而脚本读的是 `~/.config`
    与 `~/.cache`。后果是启动器永远找不到 `config.toml`，于是永远按默认值（`:3000`、
    标准版）探活——用概念版音源的人只会看到「服务启动失败」。
  - `setsid`（util-linux 专有）与 `ss`（iproute2）在 macOS 上不存在，分别退回
    `nohup` 与 `lsof`。
  - `seq` 换成 `while` 计数循环。

  > 顺带把「配置根目录」统一到 `KUGOU_TUI_CONFIG_DIR`：PowerShell 侧一直认它，
  > bash 侧此前只认 XDG 变量。

- 启动器拉起的服务现在会写 PID 文件，`kugou-api stop` / `status` 因此管得到它。
  此前它是个「孤儿进程」：端口占着，但按 PID 文件停不掉，`status` 只会说
  「端口有响应，但不是本脚本起的」。

- **迟到的异步结果会污染界面**（`app/update.rs`）。所有请求都是 `tokio::spawn`
  出去的，回来顺序不保证，而此前只有「播放」那一侧判了身份（`is_current`、
  歌词/封面比 `hash`）。剩下的路径照单全收，于是：

  - 搜「A」→ 结果还在路上 → 改搜「B」→ A 的结果先到，**把 B 的结果整体覆盖**；
    点 `M` 加载更多时更糟：A 的歌被追加进 B 的列表，而标题还写着 B。
  - 打开歌单 A → 用户改开 B → A 的结果先到，右侧变成「标题写着 B、内容是 A」。
  - 歌手与榜单同理（左列高亮 B、右列是 A 的歌）。
  - 切歌单广场分类 / 歌手地区筛选时，旧分类的结果会顶掉新分类的列表。

  现在每条结果回来都要先自证身份，不一致直接丢弃；判据抽成 `accepts_search_result`
  与 `accepts_open_item` 两个自由函数并加了测试（`App` 构造需要音频引擎与运行时，
  单测里搭不出来，所以判据必须留在能单独测的地方）。

  顺带修掉两个同源的小问题：过期结果不再把正在进行的搜索的「正在搜索…」抹掉
  （清 `busy` 挪到判据之后）；连按 `M` 不再基于同一个 `page` 各发一次请求
  （两次结果都追加就是重复的一页，晚到的那条还会让顺序倒过来）。

  身份字段一律在 `spawn` **之前**写进状态（`open_playlist` / `open_artist` /
  `open_board`），写在结果里就晚了——那条结果永远对不上。判据与清单记在
  `docs/MAINTENANCE.md` §4 的第 11 条。

### 变更

- **只推 tag 就能发版**。`release.yml` 增加第一个 job `prepare`，由它调用新的
  `scripts/release-notes` 从 `CHANGELOG.md` 取出该版本正文、建好 Release；三个平台
  job 都 `needs: prepare`。

  此前 Release 只能由本地 `scripts/release` 创建，工作流只负责**等**它出现——于是
  「只推 tag」是不成立的：tag 一推 CI 就起来，等满 5 分钟也没有 Release，三个 job
  一起失败。现在 `git tag … && git push origin <tag>` 一条路走完。

  `scripts/release-notes` 是从 `scripts/release` 里那段 `awk` 抽出来的：这段正文有
  两个消费方（本地发版与 CI），写成两份必然漂移，而发行说明是发布页的门面。

  > 代价：只推 tag 不会更新 AUR 的 `PKGBUILD` / `.SRCINFO`，也不校验 CHANGELOG
  > 有对应版本节。要同时发 AUR 仍然得跑 `scripts/release`。

- **Linux 发行包也由 CI 产出**。此前只有 Windows 与 macOS 交给 CI，Linux 那份由本地
  `scripts/release` 编好上传——那意味着**开发机不在手边就发不出 Linux 包**：换机器、
  重装，或者只想给一个已经发过的版本补资产，都得先把 Rust 工具链与整棵依赖树重建一遍。
  现在 `release.yml` 增加 `ubuntu-latest` 一栏，推 `v*` tag 时三个平台统一出包，
  产物不依赖任何一台具体的机器。

  本地 `scripts/release` 的打包与上传**保持不变**：两边同名，靠
  `upload-release-asset` 的 `--clobber` 覆盖，谁后到谁生效。因此两边的 sha256
  **不保证相同**（rustc 小版本、目标机器上的 C 工具链都会影响 `aws-lc-sys` 的编译
  结果），本地记下的哈希事后对不上是正常的。

- **CI 的 Linux 栏会传一份二进制 artifact**（`kugou-tui-linux-x86_64`，保留 14 天）。
  此前每次 push 都会 `cargo build --release`，但产物用完即弃——想拿一份能跑的二进制
  只能等发版，或者在本机重编一次（`lto = "fat"` + `codegen-units = 1`，要好几分钟）。

## [0.4.3] - 2026-09-27

### 新增

- **Windows 与 macOS 的发行包**。此前 Release 里只有 Linux 的 tarball——不是漏了，
  是开发机是 Linux，而 Windows 的 `.exe` 必须在 Windows 上编（MSVC 工具链）、
  macOS 的二进制必须在 macOS 上编（Apple SDK）。现在交给 CI：

  - `.github/workflows/release.yml`：推 `v*` tag 时在 `windows-latest` 与
    `macos-latest` 上各编一份、打好包、挂到 GitHub Release 上；
    也可以 `workflow_dispatch` 手动指定 tag，给**已经发过**的版本补资产。
  - `scripts/upload-release-asset`：上传那一步单独抽出来，两个平台共用（Windows
    runner 上的 `shell: bash` 就是 Git Bash）。它处理两件事：**等 Release 出现**
    （本地发版流程是「推 tag → 建 Release」，tag 一推 CI 就起来了，直接上传会撞上
    release not found），以及**幂等**（`--clobber`，补资产/重试时覆盖同名文件）。
  - 两个 job 都会先核对 `--version` 与 tag 一致，checkout 错 ref 时能拦住。

  > Linux 不在这里重复构建：`scripts/release` 已经在本地编好、验好、传上去了。

- **歌词换行的过渡动画**（仿 Apple Music 的逐行切换）。此前换行是**瞬间跳变**：
  `active_line` 一变，配色与滚动偏移同时硬切，没有任何时间维度的缓动。

  做法是把「离当前行多远」从整数距离换成**两个锚点距离场的插值**——换行时保留旧锚点，
  按 `t = ease_out_cubic(已过时长 / 总时长)` 在「到旧行距离」与「到新行距离」之间插值：

  ```text
  d = lerp(|行 - 旧行|, |行 - 新行|, t)
  色 = mix(mix(text_dim, lyric_far, fade(d)), 高亮色, heat)
  ```

  `heat` 一个标量同时表达进入与退出：新行 `0 → 1` 点亮，旧行 `1 → 0` 淡出，其余行
  的明暗层次也跟着平滑重排。**没有新增任何渲染原语**，全部复用既有的 `mix` /
  `fade_ratio`，逐字扫光原样叠加在 `heat` 之上。

  > **为什么不做「上滑」和「缩放」**：终端没有子单元格定位——一个字符格就是一行，
  > `Paragraph` 的滚动偏移是整数（没有小数滚动），字体尺寸也固定。所以位置维度
  > 动不了，能做过渡的只有颜色维度。硬做整行跳只会显得生硬。

  - 时长按行距自适应：`min(配置值, 该行到下一行的间隔 × 0.55)`。快歌的行只有几百
    毫秒，按上限走会出现「上一次过渡还没走完就该换下一行」。
  - 一次跨 3 行以上（拖动进度条、点歌词行跳转）**直接吸附**，不逐行淡过去。
  - 切歌 / 清空歌词会复位过渡状态，新歌第一句不会从上一首的某一行淡过来。
  - 复用既有的 30fps 提速逻辑（`lyric_visible`），**没有为动画新增计时器**。

- **点击歌词行跳到这一句**。命中区机制（`HitZone` / `HitTarget`）现成可用，新增
  `HitTarget::LyricLine` 即可；「显示行 → 歌词行」的映射由渲染层每帧回填，与
  `hit_zones` 同一套约定（只有渲染层知道行几何——译文/音译会让两者不再一一对应）。

  跳转目标要**加回 `lyric_offset_ms`**：当前行是按 `position − lyric_offset_ms` 算的，
  直接跳到 `line.time_ms` 会正好差一句。这一步漏了不会报错，所以单独抽成
  `lyric_seek_target` 并加了测试。

- `lyric_anim_ms` 配置项（默认 `200`，`0` = 关闭），设置页有「歌词动画」一项。
  三种情况会强制关闭，与取值无关：`lite_mode`、16 色模式（没有中间色阶，淡入会
  退化成「过半时整块硬翻」）、一次跨 3 行以上。

### 变更

- `scripts/make-release-tarball` 改成**在 macOS 上也能跑**（CI 的 macOS 那一栏要用它
  产出 `aarch64-apple-darwin` 的包）。两处 GNU 专有的东西换掉了：`readlink -f`
  （BSD 的 readlink 没有 `-f`，改成自己逐层解软链）与 `sha256sum`
  （macOS 那边叫 `shasum -a 256`）。顺带确认它只用 bash 3.2 就有的语法
  ——macOS 自带的 bash 就是 3.2。

- `fade_ratio` 的参数从 `usize` 改成 `f32`（过渡期间距离是插值出来的小数）。
  稳态下传进去的仍是整数值，结果与改造前**逐位相同**——所以既有的配色断言一行未改，
  另加了一条「过渡走完后与稳态逐格相同」的护栏测试盯着这件事。

### 验证

- `cargo test` **313 passed / 0 failed**（本次新增 20 条）
- 新增的测试覆盖：进度端点与 ease-out 单调性、四道否决、行距自适应、最后一行兜底、
  换行中途重定向、显示行映射越界、**交叉淡化两头都对**（旧行变暗 / 新行变亮）、
  过渡中途两行都是半亮、过渡走完与稳态逐格相同、占位提示不登记命中区、
  跳转目标加回偏移（三档偏移 + 下溢）
- `cargo fmt --all --check`、Linux 与 Windows 两个目标的 `clippy -D warnings` 均 exit 0
- `scripts/make-release-tarball` 实跑过（含软链场景），
  `scripts/upload-release-asset` 的「glob 无匹配必须报错」分支实测会退出 1
  ——这条抓出过一个真 bug：`assets=("$pattern")` 加了引号时 bash 不做路径展开，
  于是那个检查永远不触发，CI 会一片绿而 Release 上什么都没有

## [0.4.2] - 2026-09-27

### 新增

- **Windows 支持（10/11，x86_64）**。此前 `docs/INSTALL.md` 直接把 Windows 写成
  「不行」，理由是 `zbus` 在那边要 `async-io`；实际盘下来问题不止那一条，
  按「编译不过 → 编译过了但行为不对 → 行为对了但用起来别扭」三层各修了一批：

  1. **编译不过的**（三处，都是硬错误）：
     - `audio/streaming.rs` 用了 `std::os::unix::fs::FileExt::read_at`（`pread`）。
       Windows 没有 `pread`，包了一层 `read_at`，那边走 `seek_read`
       （借 `ReadFile` 的 OVERLAPPED 做定位读）。副作用是会挪文件游标，
       而那个句柄是只读落盘文件、游标没人用，所以无害——注释里写清了。
     - `logger.rs` 的 stderr 重定向用了 `libc::dup2` + `STDERR_FILENO`，
       而 `libc` 在 Windows 上**没有** `STDERR_FILENO`，`std::os::fd` 整个模块
       也不存在。改成平台分支：Unix 照旧 `dup2`，Windows 用 `SetStdHandle`
       改 `STD_ERROR_HANDLE`。
     - `mpris.rs` / `tray.rs` 是 D-Bus 集成，Windows 上既没有 session bus
       也没有认这两个接口的宿主。连同 `zbus` 依赖一起收敛到 `cfg(unix)`，
       相关字段与同步逻辑同步条件编译。

  2. **编译过了但行为不对的**：
     - **下载目录的 `~` 展开在 Windows 上失效**。`app/settings.rs` 只读 `HOME`，
       而那是 Unix 的约定，Windows 上是 `USERPROFILE`——结果是 `~/Music`
       原样留着，最后落出一个名字就叫 `~` 的目录，用户在自己以为的位置找不到文件。
       改成 `HOME` 优先、拿不到再问 `dirs::home_dir()`（Windows 上走
       `SHGetKnownFolderPath`）。顺带认 `~\Music` 这种反斜杠写法。
     - **配置落盘位置**：`dirs` 在 Windows 上给的是 `%APPDATA%` / `%LOCALAPPDATA%`，
       无需平台分支，但文档里一直只写 XDG 路径，容易让人找错地方。
     - **`--print-config` 谎报托盘状态**：Windows 上会显示「系统托盘 : 启用」，
       而那份代码压根没编进去。现在如实显示「不可用（windows 无 D-Bus）」，
       `tray` 的默认值也改成按平台取（非 Unix 默认关）。

  3. **依赖调整**：
     - TLS 后端按平台分叉：非 Windows 仍是 rustls，**Windows 改用系统自带的
       SChannel**。这不是性能取舍——rustls 会把 `aws-lc-sys` 拖进来，而那是 C 代码，
       Windows 下除了 MSVC 还要额外装 **CMake + NASM** 才编得过。换掉之后
       Windows 的构建前置就只剩「Rust + MSVC 工具链」，证书校验还顺带走了系统根证书库。
     - `zbus`、`libc` 收敛到 `cfg(unix)`；Windows 侧新增 `windows-sys`
       （本来就在依赖树里，只是显式声明 `SetStdHandle` 用得到的那几个 feature）。

- `scripts/build-windows.ps1`：Windows 的构建 + 打包脚本（对应 Unix 侧的
  `make-release-tarball`），产出 `dist\kugou-tui-<版本>-<三元组>.zip`。
  仓库里其余脚本都是 bash，Windows 下默认跑不了，构建本身两条命令就够、
  真正容易漏的是打包（发行包必须同时带上那三个 bash 脚本与 `docs/`）。

- `scripts/kugou-tui.ps1` 与 `scripts/kugou-api-install.ps1`：Windows 侧的启动器
  与服务安装器，对应 bash 的同名脚本。装上后日常使用就是两条命令
  （`kugou-api-install.ps1` 一次 → `kugou-tui.ps1` 开播），不必再手动开一个终端
  起 `node app.js`。几个刻意的取舍：

  - 启动器**不写 `param()` 块**：一旦声明了参数，PowerShell 会把 `-s 海阔天空`
    当成写错的参数名直接报错；没有 `param()` 时全部参数进 `$args`，正好能原样透传。
  - 安装与启动**分开**（bash 侧是安装器顺手把服务拉起来）：这样「怎么起服务」
    只有启动器一处实现，两边不会各自漂移。
  - 只依赖 PowerShell **5.1**（Windows 自带的那版），不要求额外装 pwsh 7；
    平台判断用 `$env:OS` 而不是 `$IsWindows`（后者 6.0 才有）。
  - 带 `--dry-run`，只打印「当前音源 / 探测地址 / 服务目录 / 服务在不在跑」，
    排查「为什么它说服务没起」时不用猜。

- **CI**（`.github/workflows/ci.yml`）：**Linux / Windows / macOS 三个平台**各跑一遍
  `clippy -D warnings + test + build`，Windows 那一栏再跑一次打包脚本。
  此前仓库里没有 CI，而 `CONTRIBUTING.md` 却写着「CI 会用同样的命令」——
  现在这句话成立了。Windows 那一栏是这次的重点：路径展开、配置目录、
  缓存文件命名这些差异只有真跑起来才露出来，光靠 `cargo check` 保证不了。

- **macOS 进入 CI**，于是「macOS 支持吗」有了可验证的答案，而不是一句「理论上可行」。
  代码层面它和 Linux 共用 `cfg(unix)` 分支，本来就没有 Linux 专属的调用残留；
  但**共用不等于验证过**——`dirs` 在那边给的是 `~/Library/...` 而不是 XDG 目录、
  音频走 CoreAudio、字体探测拿不到 `fc-list`，这些只有真跑一遍测试才看得见。
  已知差异（无 MPRIS/托盘、无最小化窗口、配置路径不同）见 `docs/INSTALL.md`
  新增的「在 macOS 上构建与运行」。

- `KUGOU_TUI_CONFIG_DIR`：整体覆盖配置根目录。便携安装（程序与配置一起放 U 盘）
  用得上；也让「配置能活过一次重启」那条测试能在 Windows 上跑——那边 `dirs`
  走的是 Win32 Known Folder，`XDG_CONFIG_HOME` 对它无效，没有这个开关就只能跳过测试。
- `KUGOU_TUI_NERD_FONT`：显式指定有没有 Nerd Font。Windows 没有 fontconfig，
  `fc-list` 探测一律落空，装了 Nerd Font 的用户靠它把图标打开。

### 变更

- **修正失效的 MSRV 声明：`rust-version` 1.86 → 1.90**。声明 1.86 是错的，而且
  错得有害——用 1.86 编会收到一长串「某依赖要求更高 rustc」，而不是一句「你需要 1.90」。
  真实下限由**依赖**顶上去（`quantette` 0.6 要 1.90，经 ratatui-image → icy_sixel 引入；
  ratatui 0.30 要 1.88、rodio 0.22 要 1.87），已用 `cargo +1.90.0 check --locked
  --all-targets` 实测确认。README 徽章、`docs/INSTALL.md`、`CONTRIBUTING.md` 与
  三处源码注释里的旧数字一并改掉。

- **清掉 29 处按真实 MSRV 才暴露的 clippy 建议**。这不是「顺手清理」——那条错误的
  MSRV 声明同时也把 clippy 的 MSRV 感知 lint 全压住了（它按 `rust-version` 判断哪些
  新 API 可用），所以声明一改成 1.90，`clippy -D warnings` 立刻红了。三类：

  | lint | 处数 | 改法 |
  |---|---|---|
  | `collapsible_if` | 23 | `if let Some(x) = a { if b { .. } }` → `if let Some(x) = a && b { .. }`（let-chain，1.88 起可用） |
  | `manual_is_multiple_of` | 3 | `n % 2 == 0` → `n.is_multiple_of(2)` |
  | `chunks_exact_to_as_chunks` | 3 | `bytes.chunks_exact(4)` → `bytes.as_chunks::<4>().0`（元素类型由 `&[u8]` 变成 `&[u8; 4]`） |

  都是 `cargo clippy --fix` 的机器可应用建议，语义等价，改完 293 条测试全过。

- **`cargo fmt --all` 拉平了格式基线**，CI 从此可以跑 `cargo fmt --check`。
  之前仓库有 38 处不是 rustfmt 干净的（多为手写换行与 rustfmt 的取舍不一致），
  而 `CONTRIBUTING.md` 却要求贡献者跑 `cargo fmt --all`——两边对不上。
  这次一并拉平（19 个文件，纯空白），于是那句「CI 会用同样的命令」真正成立。

- `~` 展开的逻辑抽成纯函数（主目录由调用方传入），相关测试不再改写进程级
  `HOME`——测试是并行跑的，那种写法是偶发失败的隐患。
- 音频设备打开失败的提示按平台给排查线索（Windows 上提 Windows 音频服务，
  而不是一直说「检查 PipeWire/ALSA」）。
- `docs/LICENSES.md` 按新的 `Cargo.lock` 重算（依赖 469 → **478** 个包；
  宽松许可 455 → **464**）。新增的 `native-tls` / `openssl` / `openssl-sys` 等
  是 `native-tls` 为「非 Windows、非 macOS」目标声明的 OpenSSL 后端——
  本项目只在 Windows 下用它（走 SChannel），**Linux 构建根本不编译这几个包**，
  文档里已注明这一点，免得看表的人以为产物里多了个 OpenSSL。

### 文档

- README 从「Linux 项目」改成**多平台叙述**：安装段按平台分列（预编译包 / 源码构建），
  并补了一张 Linux / Windows / macOS 的能力对照表。徽章同步（`platform` 与 `rust` 版本）。
- `docs/INSTALL.md`：新增「在 Windows 上构建与运行」（含 PowerShell 脚本用法、
  `--dry-run`、终端要求、功能差异表、改代码时怎么确认没弄坏 Windows）与
  「在 macOS 上构建与运行」两节；环境要求表补上 Linux 的 `alsa-lib` / `pkg-config`
  前置（干净容器上缺了会在 `alsa-sys` 报错，此前没写）与 macOS 一行。
- `docs/FAQ.md`：新增「平台相关」一节，回答 Windows 的脚本/工具链/托盘/封面/字体
  与「macOS 支持吗」。
- `docs/RELEASE.md`：说明 Windows 的 zip 由 `scripts/build-windows.ps1` 产出、
  **当前不由 `scripts/release` 自动附带**（那脚本是 bash、跑在 Linux 上），
  以及挂到 Release 上的手动步骤与将来交给 CI 的做法。
- `docs/CONFIGURATION.md`：**动作名清单漏了 `toggle_window`**（写 59 个，实际 60 个），
  已补上并说明它是唯一没有默认键位的动作、且仅在 niri 下生效；`tray` 一段补上
  非 Unix 平台的表现。
- 修正「最小化窗口 = `m` 键」的写法：`m` 是**静音**，最小化的入口在**托盘菜单**里
  （README / INSTALL / CHANGELOG 三处）。
- `docs/MAINTENANCE.md`：§1.6 补 macOS 一栏与三平台 CI 的说明；新增 §1.7
  记录 Windows 侧三个脚本的坑（`param()` 与 `$args`、`Start-Process` 的
  重定向限制、只用 5.1 语法）。

### 已知限制

- Windows 上没有系统托盘、MPRIS，也没有「最小化窗口」（触发入口在托盘菜单里）：
  前两者是 D-Bus 接口，后者走的是 niri 的 compositor IPC。都是**降级**：
  相关入口不会出现，不会点了没反应。
- Windows 上封面只能走半块字符画：Windows Terminal 不支持 Kitty / iTerm2 的图形协议。
- Windows 侧的 `kugou-tui.ps1` / `kugou-api-install.ps1` 是 bash 版的**对应物而非
  逐行移植**：只覆盖主流程（当前音源 → 服务目录/端口/`platform` → 探活 → 拉起 →
  等就绪 → 进播放器），`kugou-api`（`start`/`stop`/`restart`/`status` 四个子命令）
  没有对应物——启动器已经覆盖了它唯一的日常用途。要停服务就关掉播放器后
  `Stop-Process -Name node`，或用 `Get-NetTCPConnection -LocalPort 3000` 找到 PID。
- **Windows 的 zip 还没进发版流程**：Release 里目前只有 Linux 的 tarball，
  Windows 用户按 `docs/INSTALL.md` 从源码构建。要挂上去得在 Windows 上跑一次
  `build-windows.ps1` 再 `gh release upload`，或让 CI 在打 tag 时上传。
- **macOS 没有在真机长期使用过**：CI 跑的是 `clippy + test + build`，覆盖不到
  音频设备枚举、CoreAudio 实际出声、iTerm2 图形协议这些真机行为。
- **Intel Mac 没有预编译包**：CI 的 `macos-latest` 是 arm64，产出的二进制在 Intel
  机器上跑不了。Intel 用户目前只能从源码构建（`cargo build --release`）。

## [0.4.1] - 2026-09-26

### 修复

- **程序一运行就独占声卡，麦克风和扬声器全被挤哑；退出后声音也回不来**。
  这是一条把整个系统音频搞瘫的 bug，根因是「绕过声音服务器直连硬件」：

  1. **设置页里有个叫「Default Audio Device」的设备，名字看着最像"系统默认"，
     实际是 ALSA 的 `sysdefault`——`plughw:0` 的直连别名**（见
     `/usr/share/alsa/pcm/default.conf`；而真正的默认设备叫
     「Default ALSA Output (currently PipeWire Media Server)」）。选中它之后
     程序直连 USB 声卡，PipeWire 再想打开那张卡就是「设备或资源忙」——
     麦克风、扬声器、其它应用的声音一起没了。
  2. 就算没选它，`open_default_sink()` 在默认设备打不开时还会**遍历设备列表
     抓第一个能开的**——在服务器系统上那同样是抢占硬件。

  现在引擎先探测声音服务器（PipeWire / PulseAudio）是否在场：

  - **在场时，可选设备只剩经服务器路由的三种 PCM**（`default` / `pipewire` /
    `pulse`），直连硬件的（`sysdefault`、`hw:`、`plughw:`、`front:`、
    `surround*:`、`iec958:`、`hdmi:` …）一律不进列表——ALSA 硬件设备是独占语义，
    抓走一个就挤死服务器上的所有其它客户端；
  - **默认设备只开 PCM `default`**，打不开就如实报错，不再回退到裸硬件；
  - 没有声音服务器（headless 的裸 ALSA 系统）时行为不变，直连没有
    「挤死别人」的问题。

  配置里遗留的 `audio_device = "Default Audio Device"` 不会让程序起不来：
  找不到就记一条日志、回退系统默认（界面会显示实际打开的设备）。

- **音频线程卡住时退出会僵住，设备一直不释放**。收尾对音频线程的 `join` 改成
  **有界等待**（3 秒）：正常情况它一个轮询周期（200ms）内就退出；万一卡在驱动
  层面的阻塞操作上，等不到就放弃——进程照常退出，内核回收它持有的全部 fd，
  设备立即释放。`panic`（release 构建 `panic = "abort"`）与被 `kill -9` 时
  同样由内核回收，不需要额外处理。

## [0.4.0] - 2026-09-26

### 修复

- **连续播放 / 频繁切歌后常驻内存涨到一百多 MB，而且不回落**。两处叠加：

  1. 边下边播的字节缓冲**只追加、不回收**，一首歌下到多少就常驻多少（实测 1:1，
     放 64 MiB 的 Hi-Res 就是 66.5 MiB）；
  2. 切歌时**不取消**上一首的下载任务。用户每按一次 `n`，就多一条还在跑的下载在
     后台把整首往内存里灌，谁也不回收——实测连切 5 首、每首 64 MiB，RSS 冲到
     **323.7 MiB**。

  现在缓冲只保留读指针附近 4 MiB 的尾部窗口，窗口之外的字节从落盘文件 `pread`
  读回来（下载本来就是落盘的，数据就在页缓存里）；切歌/停止/退出时主动取消旧任务。
  同样负载下：单曲 66.5 → **12.5 MiB**，连切 5 首 323.7 → **3.0 MiB**，
  同一份对照负载连播 24 首稳定在 12.5 MiB（不再随曲目数增长）。

- **边下边播时播放进度会突然回到开头**。用户看到的是「缓冲一下，然后从头再放一遍」，
  根因有两层，缺一不可地都要修：

  1. **流一放完就重新装载**。后台下载下完时发的是「音频已落盘，可以放了」，
     而这个事件的处理函数会 `audio.load()` 一次——可那时候歌**已经在放了**，
     重新装载等于把播放位置冲回 0。现在这个收尾走独立事件，只清进度、预取、回收缓存，
     绝不碰播放器。
  2. **断流被当成了播完**。rodio 的解码器把任何读错误都吞成 EOF，所以「数据没跟上」
     和「这首放完了」在播放层长得一模一样；按「播完」处理就会触发切歌，单曲循环
     （或队列里只一首）下就是从 0 再放一遍。现在引擎按住那条流的真实状态分档：
     取消（静默）、下载失败（报错，不切歌）、读超时（留住位置续播一次）、下完了
     （才切歌）。

  另外位置要在**曲目结束之前**就记下来：源一结束 `Player::get_pos()` 就报 0，
  那一帧再读就等于把位置抹掉（用户看到进度条突然回到 00:00）。断流与下载失败之后，
  按 Space 会从停住的位置接着放，而不是从头。

- **下载中断会把「半首」留在缓存里**。流式下载原先直接写正式缓存文件名，失败或被
  取消时会留下一个「看起来完整、其实只有前几 MB」的文件；而缓存查找只按「文件在不在」
  判断，下次播放命中它就会在中间莫名结束。更糟的是它还会**反复重启**：文件放完 →
  被判为播完 → 单曲循环从头 → 再放完 —— 用户实际听到的就是十几秒一循环。
  现在先写 `.part`、**下完才改名**，失败/取消时删掉；缓存回收与「清空缓存」也跳过
  `.part`（它是半成品，不是缓存）。

### 变更

- **打开歌单时，首屏改从**末尾那一页**取**。原先不管列表怎么显示都先取第 1 页，
  而默认是倒序显示（`o` 键可切），真正出现在最上面的是**最后一页**的内容——
  于是用户看到的是「先闪一屏歌单中部的歌，几秒后整表到位、画面整体翻一次」。
  实测反馈就是「每次进歌单，第一眼看到的都不是我心里的第一首」。现在倒序显示时
  首屏直接取末页，一进去就是最终列表的头部；正序显示时仍取第 1 页。

  顺带修掉一个连带的判据错误：「这一页不满 ⇒ 这就是全部」**只在第 1 页成立**。
  末页不满是常态（414 首 = 13×30 + 24，末页就 24 首），照搬老判据会让「整表补齐」
  永远不出门，歌单从此只显示那 24 首。曲数拿不到时（有的接口不给）仍退回第 1 页。

### 文档

- 新增 **[docs/MAINTENANCE.md](docs/MAINTENANCE.md)**：模块地图、数据流、风险点与
  已知坑、改完怎么跑验证。`docs/DESIGN.md` 的「边下边播」一节补上了窗口回收、
  断流分档与 `.part` 改名三条设计说明。

## [0.3.10] - 2026-09-25

### 修复

- **歌手列表只显示第一组，A–Z 全表被丢掉**。`/artist/lists` 是按首字母**分组**返回的
  （`data.info` 是 28 个 `{title, singer: [...]}`：热门 + A–Z + `#`），而解析器把组包装
  当成了歌手本身——每一组都解析失败，最后掉进兜底扫描只命中第一组，界面只剩热门那
  60 人。实测应有 **1054** 人。线上日志里因此刷了 19 条「响应未命中任何候选键……
  请核对字段布局」的警告——那条警告本身就在喊「布局没认全」，只是一直没人回头查。

- **点进歌手，一首歌都没有**。`/artist/audios` 的 `data` **本身就是歌曲数组**（歌单是
  `data.songs`、排行榜是 `data.songlist`，只有它是裸数组），而 `extract_songs` 只在
  `data` 里按命名键找，数组本身永远命中不了，于是恒返回空。修复后歌手页正常列出。

  这两个都是拿真实响应逐字段核对「解析器读的候选键 vs 响应里实际存在的键」查出来的，
  与之前网易云 `ar`/`al`/`dt` 那次同型。各配了用真实 JSON 写的单元测试。

- **接口端口上跑着别的服务时，报错看不懂**。服务端回 2xx 但不是 JSON 时，原先直接抛
  serde 的原话 `expected value at line 1 column 1`，完全看不出该去查什么。现在单独成
  `AppError::NonJsonBody`：带上响应体开头（是网页、是空响应、还是一段纯文本，一眼可辨），
  并直接提示「检查该音源的 `api_base`」。实测撞过：配置里 `[sources.kugou] api_base`
  指向 3000，而那个端口上跑着另一个 Web 服务。

- **发行包自足性检查会假报「缺脚本」**。`tar -tzf … | grep -q X` 里 `grep -q` 命中即退出，
  而 tar 是边解析边往 stdout 写的，会因 SIGPIPE 而死（退出码 141）；`set -o pipefail`
  于是把整条管道判为失败——包明明是好的却报缺。改成先把清单落到文件再逐项核对。

- **发版动作清单没跟着 `--no-aur` 变**。带 `--no-aur` 跑仍会列出「更新 AUR」「推 AUR」，
  然后一件都不做。这份清单的用途就是让人在按下回车之前确认「将要发生什么」，列了却不做
  等于在骗人；现在按开关拼装并明示已跳过。

### 新增

- **`scripts/release` —— 一条命令走完发版**。阶段一（本地、可反复跑）：前置检查 →
  回收中间产物 → clippy + 测试 → 构建 → 打发行包；然后**停下来**列出阶段二将要执行的
  每一条外部动作，等确认。阶段二（不可逆）：push main → 打 tag → 推 tag → 更新并验证
  AUR → 建 Release → 下载回来核对 → 推 AUR。网络步骤带退避重试（实测 `git push` 报过
  `TLS unexpected eof`），建 Release 做成幂等。AUR 账号拿不到时用 `--no-aur` 整条跳过。

- **`scripts/clean` —— 回收构建中间产物**。一轮发版会在 `target/package/`、`dist/` 里的
  历史 tarball、AUR 仓库的 `src/` 与 `pkg/` 三处留下东西，实测 10+ MiB/轮。默认只清明确
  可再生的中间产物，**不动** `target/` 整体（会毁掉增量编译）与运行时的歌曲磁盘缓存
  （用户数据，且它自带逐出逻辑）；要清得显式加 `--all` / `--cache`。

### 文档

- 新增 **`docs/RELEASE.md`**：版本号与标签规则、发行版产物应包含哪些内容、目标仓库、
  各步骤的执行顺序与依赖关系、失败后怎么恢复，以及两类回收逻辑的触发时机与清理范围。
  其中「为什么 AUR 更新必须在 push tag 之后」单独写了——那是整条流程里唯一无法重排的依赖。
- `CONTRIBUTING.md` 的发版一节改为指向脚本与规格。原来那份手工步骤正是脚本现在做的事，
  留两份必然漂移。
- 更正 **crates.io 的错误声明**：0.3.9 的条目与 GitHub 发行说明里写了「已发布到 crates.io」，
  那是假的——`cargo install` 只能装上主程序，不带那三个脚本、也不带接口服务，是条「半截」
  的安装路径。已从 CHANGELOG 与发行说明里撤掉（tag 里的原文改不了，由本版覆盖）。
- README 补上「Release 里的预编译包」这条安装路径——它今天就能用，之前只写在
  `docs/INSTALL.md` 里，门面上看不到。
- `docs/INSTALL.md` 预编译包示例里的版本号跟到 0.3.10。

## [0.3.9] - 2026-09-25

### 文档

- `CONTRIBUTING.md` 新增**「发版」**一节，把今天连发几版踩到的坑写成步骤：
  工具脚本要在打 tag **之前**提交、AUR 的 `pkgver`/`sha256sums` 跟着改且
  `.SRCINFO` 必须重新生成、上游提交号钉在**两处**、**发完要下载回来核对**
  （0.3.7 的 tarball 漏脚本就是因为只看本地目录）。
- 修 `CONTRIBUTING.md` 两处过时内容：「release 约 5.4 MiB、常驻约 13 MiB」→ 实测
  7.0 MiB / 14–17 MiB；指向 README「实测修正过的认知」表的引用——那张表其实在
  `docs/DESIGN.md` 的「接口适配」，这个引用在拆分 README **之前**就是坏的。
- 「解析上游响应要宽容」补一条：**同一个上游可能有多套字段布局**，别只认调试时
  看到的那一套（网易云的 `artists`/`album`/`duration` 与 `ar`/`al`/`dt`），
  每种都要有单元测试钉住。
- 修 `docs/LICENSES.md` 里一条**安装后会断的链接**：文档装在
  `/usr/share/doc/kugou-tui/docs/`，而许可按 Arch 惯例在 `/usr/share/licenses/`
  下，`../LICENSE` 必然失效。改成写明路径，两种场合都读得懂。

## [0.3.8] - 2026-09-25

### 修复

- **`kugou-tui-install-api` 装了一堆用不到的开发依赖**。它跑的是 `npm install`
  （全量），而 `kugou-api` 的自动补装用的是 `npm install --omit=dev`——两处不一致。

  服务运行时是 `node app.js`，上游那 10 个 devDependencies 全是开发工具
  （`nodemon` / `typescript` / `pkg` / `prettier` / `ts-node` / `tsdown` / `@types/*`），
  一个都用不到。实测同一台机器：装全量 **311** 个包，只装生产依赖 **120** 个。
  差的那一半纯粹是白等、白占磁盘。现在两处统一用 `--omit=dev`。

### 文档

- `docs/INSTALL.md` 补上**「路径三：预编译二进制」**。之前只有「从源码构建」与
  「AUR」两条，非 Arch 用户拿到 Release 里的 tarball 之后没有任何说明。
  现在写清了解压、把二进制与三个脚本链进 `~/.local/bin`（`scripts/kugou-tui` 与
  二进制同名，链过去要改名）、以及 `kugou-tui-install-api kugou` 这一步。

> 另外，从本版起 Release 的 tarball 里**同时包含三个脚本与全部文档**（之前只有
> 二进制与 README）。非 Arch 用户解压后就能把接口服务配起来，不必再回仓库 clone。

## [0.3.7] - 2026-09-25

### 新增

- **支持「软件包内置的接口服务」**。为 AUR 包的「装完即用」准备：包会把接口服务连同
  生产依赖装到 `/usr/share/kugou-tui/api/<音源>`（只读），三个脚本现在都会优先用它，
  没有才退回 `~` 下自己 clone 的那份——`kugou-api-install` 发现系统目录就直接启动，
  **跳过 clone 与 npm install**。

  之所以能把服务打进包里：它**不往自己目录写任何文件**（源码里没有 `writeFile` /
  `mkdirSync`），实测整个目录 `chmod -R a-w` 之后照样能起、能返回真实数据。
  于是 `/usr` 保持只读、不被 npm 污染，也不需要常驻进程。

  路径可用 `KUGOU_API_SYSTEM_ROOT` 覆盖（给非 `/usr` 前缀的打包与测试用）。

### 修复

- **全新安装时启动器静默退出**。`scripts/kugou-tui` 开了 `set -o pipefail`，而读配置
  用的是 `sed -n ... | head -n 1`；**全新安装时配置文件还不存在**，`sed` 退出非零 →
  管道被判失败 → `set -e` 让整个脚本静默退出（退出码 2，一句提示都没有）。
  用户第一次跑就是黑的，而这条路径正是「装完即用」要走的。

  现在读取前先 `[ -r ]` 判可读。顺带把重复了**三遍**的 `sources.active` 提取合并成
  一个 `read_active_source()`。

## [0.3.6] - 2026-09-25

### 修复

- **启动器脚本装到系统目录后找不到客户端二进制**。`scripts/kugou-tui` 与
  `scripts/kugou-api` 都按「脚本所在目录 `/../target/release/kugou-tui`」定位二进制
  ——软链到 `~/.local/bin` 没问题（`readlink -f` 会解析回仓库），但脚本被**拷贝到**
  `/usr/bin` 时那条路径会变成 `/usr/target/...`，必然不存在。这是发行版打包一定会踩的
  坑（准备 AUR 包时发现：装完 `kugou-tui` 直接报「找不到可执行文件」）。

  现在两处都加了 `PATH` 回退：仓库里的构建产物优先，找不到就用 `PATH` 上的
  `kugou-tui`。`KUGOU_TUI_BIN` / `KUGOU_API_BIN` 仍然可以显式指定；都没找到时的报错
  信息也说清了「从源码跑要先编译」与「装过包该设哪个变量」。

## [0.3.5] - 2026-09-25

### 修复

- **网易云下打开歌单里的歌曲一直 404**。歌单列表能正常显示，点进去却报
  `接口 /playlist/track/all/new 返回状态码 404`。

  原因是歌单「首屏」那条路径**绕过了音源分派**：它为了「先给一页让界面立刻有内容」，
  直接调了 `ApiClient` 上酷狗的分页方法（`user_playlist_tracks` /
  `playlist_tracks`，参数是 `listid` + `page` + `pagesize`，端点为
  `/playlist/track/all/new`）——那是酷狗的端点，网易云服务根本没有。而后台补全
  那一段是走分派的（`active_source.*_tracks_all`），偏偏首屏失败会直接 `return`，
  **根本走不到补全**，所以症状就是「一直 404」。

  现在分页也走分派（新增 `SourceKind::playlist_tracks_page` 与 `PlaylistRef`，
  区分酷狗的「自己的歌单 / 公开歌单」两套端点；网易云两者是同一个端点）。

  顺带把两处重复的翻页实现合并成一个 `playlist_tracks_all`，并删掉只为「首屏补盖
  来源章」而存在的 `SourceKind::stamp`——首屏走分派之后，盖章由分派层统一负责，
  那个补丁没有存在理由了。

- **网易云歌单页与歌手页里的歌没有歌手、没有专辑、时长显示 `00:00`**。搜索页却
  一切正常，所以不容易发现。

  网易云用**两套字段名**描述同一首歌：`/search` 给 `artists` / `album` / `duration`，
  而 `/playlist/track/all`（歌单、榜单）与 `/artists`（歌手热歌）给 `ar` / `al` / `dt`。
  解析只认了前者。现在两套都认，并加了单元测试钉住两种布局。

- **网易云云端歌单永远报「尚未登录」**。扫码登录后界面显示「登录成功」，歌单页却
  一直说未登录——两边看到的状态不一致。

  根因在服务端下发的凭据格式上：`NeteaseCloudMusicApi`（api-enhanced）的
  `server.js` 用 `/;\s+|(?<!\s)\s+$/g` 切分 `Cookie` 头，**只认「分号 + 空格」**
  这一种分隔；而它在 `/login/qr/check` 成功时返回的 `cookie` 字段是**整段
  `Set-Cookie`**（含 `Max-Age` / `Expires` / `Path` 属性，多个 cookie 之间用 `;;`
  连接）。`;;` 处不切分，于是 `Path=/openapi/clientlog;;MUSIC_U=00CC…` 被当成
  **一个** `k=v`，键是 `Path`，`MUSIC_U` 压根没进 `req.cookies`。

  也就是说：这串凭据从存进配置文件那一刻起就是坏的，界面上的「已登录」是个谎言。

  现在存入前会先规范化（`util::normalize_cookie_header`：丢属性段、丢空段、
  按 `"; "` 重连、同名取最后一次），**组装请求头时再过一遍**——后者让存量坏配置
  无需重新扫码即可自愈。

- **网易云的请求会带上酷狗的 `dfid`**。`dfid` 是酷狗专有的设备指纹（它把 dfid 拼进
  cookie 交给上游做风控校验），网易云没有这个机制。原先无条件拼进去，等于把一个
  别家的设备标识发给了它，既没用、又让人分不清这串凭据到底属于谁。现在按
  `SourceKind::uses_device_fingerprint()` 判断，只发给酷狗。

- **网易云的四个写接口走了会重试的通道**（加歌 / 删歌 / 建歌单 / 删歌单），违反
  「写操作不重试」的约定：删歌第一次其实成功了、只是响应丢了的话，重发会得到
  「歌不存在」——用户看到一句失败提示，而歌其实已经删掉了。改用 `*_mutating` 变体。

- **网易云登录后界面弹「获取用户资料失败」**。拉资料走的是酷狗端点（不带 uid），
  网易云返回 `{"code":400,"message":"参数错误"}`。现在按音源分派：网易云走
  `/user/detail?uid=`（昵称、头像、等级）。会员信息在网易云没有对应端点
  （`/user/vip/detail` 是 404），通过新增的 `Capability::vip` 跳过，不再白打接口。

### 文档

- **README 拆分为门面版**（295 → 129 行）：只留一句话 slogan、三个核心卖点、
  最短安装路径、精简功能表、与 cmus / mpd + ncmpcpp 的对比、以及指向 docs/ 的索引。
  边下边播原理、环境要求、启动脚本与环境变量、第三方许可分析、完整免责条款分别
  移入 `docs/DESIGN.md`、`docs/INSTALL.md`、`docs/LICENSES.md`、`docs/DISCLAIMER.md`。
- 新增 `docs/INSTALL.md`：环境要求、安装路径、API 服务部署（一键 + 手动）、
  启动器脚本与两张环境变量表、不用常驻服务的 fish 启动函数、装完先做什么。
- 新增 `docs/LICENSES.md`、`docs/DISCLAIMER.md`。
- `docs/USER_GUIDE.md` 补「功能一览」完整表，并在网易云一节写明 cookie 的格式要求
  （手动填写时要写 `k=v; k=v`，不要整段粘贴 `Set-Cookie`）。
- **改正 `docs/DESIGN.md` 里一处关于内存的错误断言**。原文写「边下边播不额外吃内存，
  写进环形缓冲、不在内存里拼装整首歌」，实际 `StreamingBuffer` 是**只追加不回收**的
  字节缓冲（要支持 `Seek` 就得留着已读字节）。实测往缓冲里塞 64 MiB、RSS 就涨 64 MiB
  （1:1），所以放几十 MB 的 Hi-Res 时内存会按曲目体积线性上涨。已在文档中如实说明，
  README 的内存对比脚注也加了边界提示。

### 其它

- `scripts/license-stats.py` 改用 `cargo metadata` 取 `license` 字段。原先直接读
  `$CARGO_HOME/registry/src/<name>-<version>/Cargo.toml`，于是「读不到」的数量随本机
  缓存漂移——同一份 `Cargo.lock`，一次跑出 26 个读不到、一次跑出 102 个，数字没法复核。
  现在结果只取决于 `Cargo.lock`：468 个依赖中 455 个宽松许可、13 个 `MPL-2.0`、0 个读不到。

## [0.3.4] - 2026-09-25

### 新增

- **输出设备可选**：设置页最后一项「输出设备」显示**实际打开的那张卡**，可在
  「系统默认」与枚举到的设备之间切换（写进配置文件的 `audio_device`）。
  这条是为一种很常见的故障准备的：进度条在走、状态是「播放中」，但一点声音都没有
  ——声音被送到了另一张卡。现在不用去翻系统配置也能看出来。切换设备会停一下当前
  这首，新设备就绪后自动按原位置续播；**新设备打不开就继续用原来那张**，只提示一句，
  不会把播放弄哑。

  候选列表筛过两道：ALSA 会把自己定义的所有 PCM 都报成设备（本机实测 52 项，大半是
  `lavrate` / `samplerate` / `jack` / `oss` 这类插件），这里只留「能给出默认输出配置」
  的，再排除 `null`（Discard all samples——它能打开、能「正常播放」，只是把所有采样
  丢掉，是「播放中却没声音」的另一种成因），最后按名字去重。

  「系统默认」显示 `default` 那张卡的自述名（如
  `Default ALSA Output (currently PipeWire Media Server)`），而不是 cpal 硬编码的
  "Default Audio Device"——后者看不出声音实际交给了谁。

### 修复

- **「播放中却没声音」能自证清白**：原先遇到这种情况完全无从下手——设备打开正常、
  解码正常、状态是播放中，而错误信息一个都没有。现在界面上直接摆出实际设备名，
  排查只需看一眼。根因实测是系统侧：`/etc/asound.conf` 把 ALSA 的 `default` 写死成
  `card 2`，而那张卡已经不是用户听的那张 USB 声卡了（卡号会变，写死数字迟早出事），
  于是声音进了没人接的板载口。又因为用的是 `type hw`（独占），它连 PipeWire 都绕
  过去了——`pactl list sink-inputs` 里根本看不到这个程序。

## [0.3.3] - 2026-09-24

### 新增

- **系统托盘**：启动时注册为 `org.kde.StatusNotifierItem`，Quickshell / waybar /
  KDE 等状态栏会显示图标，悬停显示当前曲目，**右键弹出菜单**（播放 / 暂停、
  上一首、下一首，走 `com.canonical.dbusmenu`）。**自适配**，三层降级都静默跳过、
  不影响播放：没有图形会话（既无 `WAYLAND_DISPLAY` 也无 `DISPLAY`）→ 完全不连
  D-Bus；没有 session bus → 跳过；状态栏没提供 `org.kde.StatusNotifierWatcher` →
  注册调用失败后放弃。不需要时用 `--no-tray` 或配置里的 `tray = false` 关闭
  （**重启生效**）。图标是内嵌的（源 `assets/tray.svg`，运行时缩放到 22 / 64 两个
  尺寸），不依赖系统图标主题。
- **托盘右键菜单**必须给 `Menu` 属性一个**真实对象路径**。SNI 规范里 `/` 表示
  「无菜单」，但实测 Quickshell 会据此把右键整个跳过（`hasMenu` 为假），
  于是「点了没反应」——所以这里注册了 `/StatusNotifierItem/menu` 并实现
  `com.canonical.dbusmenu`。左键的 `activate()` 仍按用户要求留空。
- **托盘菜单里的「最小化 / 显示窗口」**：把 TUI 从平铺布局里收起来（音乐照常播），
  再点一次放回去。终端程序没法自己最小化窗口（xdg-shell 没有这个请求），只能借
  compositor 的 IPC——走 niri 的 `toggle-window-minimized`。**怎么找到自己的窗口**
  是这里唯一的难点：niri 给的 `pid` 是**终端模拟器**的（kitty 等），匹配不上，所以
  启动时用 OSC 0 把终端标题设成 `kugou-tui`，之后按标题**精确**匹配找回窗口
  （不能用 `contains`：实测有浏览器标签页的标题里也带着 `kugou-tui`，会把浏览器
  最小化掉）。只有检测到 `NIRI_SOCKET` 时这一项才出现在菜单里——不是 niri 就整项
  不出现，而不是置灰。

### 修复

- **TUI 画面被音频后端的报错污染（乱码）**：libjack / libasound 会**直接往 fd 2
  写报错**，例如 `jack server is not running or cannot be started`、
  `JackShmReadWritePtr::~JackShmReadWritePtr - Init not done for -1, skipping unlock`、
  `ALSA lib pcm_oss.c:404 ... Cannot open device /dev/dsp`。TUI 在 alternate screen 上
  时，这些字符直接打在 ratatui 画好的界面里，而**增量重绘只写「内容变了的单元格」**
  ——屏幕上的第三方字符不在任何 buffer 里，永远不会被覆盖，于是残留成一片乱码。
  **窗口越窄越明显**：报错行会被终端折行成多行，而最大化时一行就够、几乎看不出来。
  现在进入 TUI 之前把 stderr 接到日志文件（`logger::redirect_stderr_to_log`，
  用 `dup2`）：画面干净了，报错仍留在日志里——排查「没声音」时它正是关键线索。
- **概念版音源下，从云端歌单播放只能听到试听**：`Song::source` 解析时填的是默认值
  `Kugou`，而 `stamp_songs()` 只覆盖了 5 个 `*_tracks_all` 方法——歌单**首屏**为了
  「先出界面」直接调 `ApiClient::user_playlist_tracks`，绕过了盖章，于是这批歌被当成
  标准版的歌，取链请求打到标准版端口；两平台 token 不通用，标准版只能给 60 秒试听。
  现在 `SourceKind` 多一个公开的 `stamp()`，首屏结果补盖一次。
  （来自 [@ccchenyulin](https://github.com/ccchenyulin) 的修复）
- **自定义字母键会抢走输入框的按键**：`[keymap]` 的自定义键位表在模式判断**之前**
  拦截，于是把 `seek_forward` 绑到 `l` 之后，搜索框里就打不出 `l` 了。现在输入态下
  无修饰的字符键优先交给输入框（`ctrl+` / `alt+` 组合仍走自定义表）。顺带两处：
  新建歌单弹窗改用 `TextInput`（自带光标 / Delete / Home / End）并显示真实光标；
  帮助面板与界面里的「按 R 重试」这类提示会按 `[keymap]` 改写后再显示。
  （来自 [@ccchenyulin](https://github.com/ccchenyulin) 的修复）

### 文档

- README 首屏的界面示意图从 ASCII 手绘换成**真实终端截图**（`assets/screenshot-0.3.3.jpg`，
  950×1021，JPEG 压到 175 KB）。

## [0.3.2] - 2026-09-24

### 修复

- **`f` 没法通过 `[keymap]` 重绑**：`parse_key` 里 F1–F12 那条判的是「长度 ≤ 3 且
  以 `f` 开头」，`f` 也满足，于是 `f[1..]` 是空串、解析失败、整条返回 `None`。
  结果是**只有 `f`（歌手地区筛选）**在配置里绑不上，其余单字母都行。写「帮助面板
  里的键都真绑过」那条测试时才发现的——测试第一次跑就挂在它上面。
- `scripts/kugou-api-install` 用 `git clone --depth 1` 拉默认分支的 tip，而 README
  要求固定到验证过的提交；两处说法不一致，且浅克隆里根本 `checkout` 不到那个 SHA。
  现在改用 `git fetch --depth 1 origin <sha>` 只拉那一个提交（实测 1 秒内完成，
  `.git` 仅 416K），并且**已装好的那份也会检查**——HEAD 与钉的提交不一致时给出
  提示与切换命令，但不擅自切。
- `scripts/kugou-api-install` 的 `start_kugou` 没把安装目录传给 `kugou-api`。
  `KUGOU_API_ROOT` 与 `KUGOU_API_DIR` 是两套变量，ROOT 非默认值时会「装在一处、
  从另一处起」。
- `kugou-api-install` 的用法注释里写着 `qqmusic`，但实现只支持 kugou 与 netease
  ——那条示例按下去只会得到「未知音源」。

### 新增

- **`scripts/kugou-api` 补齐 restart、PID 管理与启动预检**：预检会补 `node_modules`
  （自动 `npm install --omit=dev`）与客户端二进制（自动 `cargo build --release`）；
  参数按「环境变量 > 配置文件 > 默认值」取值；后台启动后打印 PID、日志路径与访问
  地址；新增 `restart`；端口被占用与「已在运行」分开判。详见脚本头部注释。

### 文档

- **`[keymap]`（自定义键位）此前在文档里一个字都没有**。CONFIGURATION.md 新增
  「自定义键位」一节：段写法、按键名规则、59 个可用动作名（按用途分组，已与
  `action_from_name` 逐字核对）、三种非法条目各自的日志文案。
- 补上 5 个「能按但任何地方都没写」的键位：`E` / `J` / `K`（音源页的设为默认与
  调优先级——调整音源优先级的唯一入口）进帮助面板，`Home` / `End`（`g` / `G` 的
  别名）进 KEYBINDINGS.md。
- README 安装步骤 1 现在把 `scripts/kugou-api-install` 列为推荐路径，并点明
  KuGouMusicApi 是**独立仓库**（无 submodule、无 vendor），本地没有就跑不起来；
  CONTRIBUTING 的手动 clone 也补上了提交固定。

## [0.3.1] - 2026-09-24

### 修复

- **可视化空态把「已停止」说成「已暂停」**：那里只判「有没有当前曲目」，于是
  `Stopped` 也显示「已暂停 —— 按 Space 继续」。可两者按 Space 的后果不同：停止是
  **从头播**，暂停是**接着播**。按下去的下一秒界面就在骗人。现在分开说。
- 侧边栏「清理缓存」的键位和目录名粘成一个词（`C清理 kugou-tui`）。改成 `[C] 的
  方括号写法，和状态栏的「[?] 帮助 [q] 退出」一致。

## [0.3.0] - 2026-09-24

> **行为变化**：启动时不再打印「已连接」——那一刻一个请求都还没发过，接口全挂也
> 照样这么说。现在只陈述配置与登录态，连通性由真实请求的结果驱动（侧边栏
> 「连接」区块标题显示 已连通 / 未连通 / 未验证）。
>
> 另外，列表载入失败时面板会显示「载入失败：… · 按 R 重试」，而不是永远停在
> 「载入中…」。

### 新增

- **瞬时网络故障自动重试**。本机走 fake-IP 代理，网络快慢波动大：一次连接被拒、
  一次响应体读到一半断掉，都会让请求直接失败，而这类失败**换个时刻重发就成功**。
  代价最实在的是取播放地址那条路——`song_stream_url` 会逐个试多个候选
  (hash, 音质)，某个候选因为一次抖动失败就被跳过；全抖过去之后，用户看到的是
  「没有可用的播放地址（可能需要 VIP 或已下架）」，一个和真实原因无关的结论。
  现在读接口会自动重试：**总 3 次尝试（首次 + 2 次重试），间隔 300ms → 900ms**。
  - **重试**：传输层断在连接或读体上（连接被拒 / 连接被重置 / 响应体截断），
    以及 HTTP 408 / 429 / 5xx。
  - **不重试**：**超时**（已经等满 15 秒，说明对端卡住而不是抖了一下，再等两轮
    是拿用户的时间换一个大概率相同的结果）；业务错误码（服务回了话，重发还是
    这个答复）；其它 4xx；200 却返回非 JSON（内容问题，不是传输问题）。
- **写接口显式不重试**（`get_json_mutating` / `get_json_uncached_mutating`）。
  重试的前提是「重发不改变结果」，而写操作不满足：`/playlist/del` 第一次其实删
  成功了、只是响应在路上丢了的话，重发会得到「歌单不存在」——用户看到失败提示，
  而歌单其实已经没了。收藏 / 移出 / 删歌单 / 新建歌单 / 领 VIP 六处改用不重试的
  变体；其余读接口维持自动重试。
- **帮助面板可滚动**（`?` 打开）：`CHEATSHEET` 有 38 条，而 34 行的终端只放得下
  26 条；面板又是模态的、按键全被吞掉，于是最后 12 条——Space / `n`·`p` /
  `←`·`→` / `+`·`-` / `m` / `r` / `l` / `[`·`]` / `W`，也就是**整块播放控制**
  ——在常见尺寸下永远看不到。现在支持 `j`/`k`、`↑`/`↓`、`PgUp`/`PgDn`、`g`/`G`，
  底部固定显示「第 N-M 条 / 共 38 条」，内容放得下时这一行不出现。

### 修复

- **接口挂掉时列表永远停在「载入中…」**：`loading` 原本只在成功路径
  （`replace()`）里清零，11 处置位没有任何失败出口。于是请求一失败，面板就一直
  转圈——状态栏报着错，面板里还在转，用户既不知道失败了、也不知道该按什么。
  现在失败会收掉「载入中」并把原因落到面板上（「载入失败：… · 按 R 重试」）。
  受影响：歌单广场、歌手、排行榜、云端歌单、搜索、歌单/歌手/榜单的歌曲列表。
- **启动时无条件打印「已连接」**：这句话在启动路径上发出，那一刻一个请求都还
  没发过，接口全挂也照样这么说——用户看到「已连接」就把网络问题排除掉了，
  然后往别处找原因。现在启动只陈述配置与登录态
  （`音源 酷狗概念版 · 127.0.0.1:3001（已登录）`），连通性改由真实请求的结果
  驱动：侧边栏「连接」区块标题显示 `已连通` / `未连通` / `未验证`，第一次请求
  成功时才改口说「已连接」。注意业务错误码（需要登录、页码越界…）算**连通**——
  它恰恰证明服务是通的，只有连接被拒 / 超时才算连不上。
- **「我的资料」取不到资料时只写日志**：首页会永久停在「加载中…」，用户分不清
  是失败还是慢。现在显示失败原因，并给一个可点的重试入口（账号区任意位置）。
- 歌单 / 歌手 / 榜单的歌曲列表标题不再挂「（载入中…）」：面板正文本来就会显示
  「载入中…」，标题再挂一次是重复；而载入失败时那个后缀会留在标题上撒谎。
- **帮助面板的列宽改成从数据算**：原来写死 `14 / Min(20) / 6`，于是「功能」列
  吃掉所有剩余宽度、把「分类」推到弹窗最右边，中间空出二十几列；「按键」列写死
  14 而最长的键名（`Tab / S-Tab`）只有 11 列。现在两列都按最长的那条算，空档
  没了；窄到放不下三列时「分类」整列让位（它是分组标签，不影响「这个键干嘛」），
  而不是把「按键」挤成 0 宽——30 列的终端上原先只剩「功能」一列可见。
- `right_click_at` 的文档注释写着「**不做上下文菜单**」，函数体却调
  `open_context_menu()`。注释改成与实现一致（右键 = 键盘 `;`）。
- **歌词取失败不再说成「暂无歌词」**。原来失败时会塞一份空歌词进去，面板于是渲染
  成「暂无歌词」——那是在替这首歌断言「**它本来就没有歌词**」，用户据此就会去
  别处找原因。现在走独立的 `Loaded::LyricFailed`，面板显示「载入失败：<原因>」。
  空歌词（请求成功但内容为空）仍然照常说「暂无歌词」，两者不再混为一谈。
  顺带：歌词请求也吃到了自动重试（实测注入 503 时会重试两次才放弃）。
  不走 `Loaded::Failed` 是刻意的——那条路会往状态栏写错误，而歌词失败不影响播放，
  每切一首歌闪一条太吵。

## [0.2.0] - 2026-09-22

> **破坏性变更**：删掉了「歌词」「封面」两个标签页，数字键 `8` / `9` 改给音源与设置；
> `1`–`7` 与 `0` 的落点不变。详见下面「删除」一节。

### 新增

- **设置页**（`,` 打开，也可点侧边栏最后一项）：主题、音质、播放模式、刷新
  间隔、歌词偏移、每页条数、缓存上限、16 色模式、歌词面板、侧边导航、
  下载目录、封面铺满方式。↑↓ 选择，←→ / Enter 改值，鼠标点击选中、再点一次改值。
  改完立即写入 `config.toml`，不等退出时保存。
- **6 套主题**：冷蓝 / 石墨 / 日落 / 森林 / 霓虹 / 暗紫（`config.theme`）。
  每套都有真彩与 16 色两个版本；16 色版只换主色与选中底色，语义色
  （成功 / 警告 / 错误）不随主题变。
- **单曲下载**（`W`）：把当前播放歌曲保存到 `config.download_dir`
  （默认 `~/Music`，设置页里改）。文件名 `<歌手> - <歌名>.<ext>`，
  扩展名从 URL 末段推断；已存在的文件不覆盖。
- **逐字歌词（卡拉 OK 效果）**：解析 KRC 的每字时间戳，当前行按 已唱 /
  **正在唱** / 未唱 三态着色，唱到哪亮到哪。拿不到逐字信息（纯 LRC 或这行没有
  逐字标记）时退回整行高亮。
  注意 KRC 的写法是 `<偏移,时长,0>字` ——**标记在字前面**（不是 `字<...>`）。
- **分块并发下载**：`/song/url` 拿到的直链若支持 Range，就分最多 4 块并发
  下载；不支持或文件太小则退回单连接。实测 128kbps 单曲 1.41s → 1.10s。
- **播放时预取下一首**：当前这首开播后，后台把队列下一首下到缓存，切歌秒开。
- **`cover_fill` 配置项**：首页那块大封面怎么铺满区域，可选 `crop`（默认，
  居中裁剪）、`stretch`（拉伸铺满）、`fit`（完整显示、左右留白）。封面区是
  「多少列 × 多少行」，换算成像素后几乎永远不是正方形，而专辑封面多是正方形——
  铺满 / 不变形 / 不裁剪三者只能取两个，这个开关让你自己挑。
  见 [docs/CONFIGURATION.md](docs/CONFIGURATION.md)。
- **每日领取概念版 VIP**：酷狗概念版自带「每天领一天 VIP」的机制，现在做成了
  自动的——登录成功后、切到概念版音源时、启动时各试一次，**一天只领一次**
  （日期记在会话里）。流程照 MoeKoeMusic：先领一天（`/youth/day/vip`），
  隔 500ms 再升级成「畅听 VIP」（`/youth/day/vip/upgrade`）。
  「我的资料」里多一行状态（`领取今日 VIP · 按 V` / `领取中…` / `今日 VIP 已领取`），
  快捷键 `V` 或**鼠标点那一行**都能触发。**只在概念版音源下显示**——这是概念版
  专属接口，标准版账号调不通，摆一个按了没用的入口比不摆更糟。
  接口带风控，失败提示会把「反复失败请到手机端领取」放在最前面（状态栏按宽度
  截断，写在后面就看不到了）。

### 变更

- **删掉「歌词」与「封面」两个标签页**：首页（左右分栏）已经有封面和歌词，
  列表页右下角也有歌词面板，这两页是重复的第三、第四处。主区空间还给首页与列表。
- **首页移到侧边栏第一项**：原先「正在播放」组排在「发现 / 我的」之后，
  首页作为默认落点却要往下数七行才看得到。现在该组整体置顶。
- **数字键 8 / 9 改给「音源」与「设置」**：删页后如果让后面的标签顺延，
  `0`（可视化）会被顶到 `8`、音源与设置也跟着挪，肌肉记忆全乱。现在把
  `可视化` 留在下标 9（仍是 `0` 键），空出来的 8、9 两格给原先够不到的
  音源与设置：**1–7 与 0 一个都没动**。
- **逐字歌词改成仿 Apple Music**：原先每个字只有「未唱 / 正在唱 / 已唱」三档，
  推进是硬跳的。现在按每个字**自己的进度**（`LyricWord::progress_at`）在底色与
  强调色之间插值，边界字取两色之间的过渡，看上去是渐变扫过。配套三点：
  - 非当前行按离当前行的距离**线性变暗**（新增主题色 `lyric_far`），当前行才突出；
  - 去掉「正在唱」的下划线——逐字推进时它会在字之间跳，比不做还闹；
  - 歌词真的显示在屏幕上**且**正在播放时，刷新提到 ~30fps（原先只有可视化页提速）。
    5fps 下再精细的插值也是五格一跳。不看歌词时一点都不额外费电，简易模式
    （`lite_mode`）仍然不做任何提速。
  - 16 色模式没有中间色阶可插，`mix` 自动退回两端取一，行为等价于原来的三档离散。
    想更接近 Apple Music 的观感需要真彩终端（`basic_color = false`）。

- **可视化改成真频谱**：原先画的是 28 格时域音量历史，被拉伸铺满整宽、
  柱子之间没有空隙，动起来是一整片此起彼伏的墙。现在对音频线程采集的
  采样做 FFT 后按**对数**分 64 个频段（40Hz–16kHz），柱子 1 列 + 1 列空隙。
- **滚轮一次滚一行**（原来是 3 行）。
- **封面渲染交给 `ratatui-image`**：不再自己往 stdout 写 kitty 转义序列，
  改由 widget 写进 ratatui 的 Buffer——内容不变时一个字节都不重发。
  依赖 `ratatui-image` 11.x，`rust-version` 提到 1.86。
- **README 的界面图换成真实截图**：原先那张是手绘的，停留在「5 个标签、无分组、
  无数字键」的版本，与实际界面早已不符，却标注着「实际画面」。现在由模拟后端
  （歌单广场 + 歌单歌曲 + 逐字歌词 + 一段 ffmpeg 生成的**静音**音频）驱动程序渲染
  出 108×30 的真实画面——数据是合成的，不含任何真实音乐内容，也不会再和代码脱节。

### 关于播放等待

**边下边播已经做了**：取到直链后先攒够 128 KB（约 8 秒音频）就开播，剩下的在后台
继续下并同时落盘缓存，所以等待是「攒开头」而不是「下完整首」。

剩下的等待由**音质档位**决定：`high`（Hi-Res）单曲实测 65 MB，按 1.9 MB/s 单连接
要 34 秒——这是文件大小的物理限制。分块并发下载（最多 4 块）+ 预取队列下一首能
把切歌压到秒开，同一首第二次播放直接命中缓存。想更快只能降音质
（`flac` ~30 MB / `320` ~10 MB）。

> 流式缓冲的读指针跑到还没下到的位置时会阻塞等数据，表现是声音停一下再继续。

### 修复

- **`s` / `a` / `d` 报「当前没有选中的歌曲」**：这些动作原先只认「焦点在歌曲列表
  那一栏」，焦点停在歌单 / 歌手 / 排行榜的**上半部分**、或在**队列页**时就拿不到歌。
  新增 `selected_song()` 按「队列选中 → 当前页歌曲列表 → 队列当前 → 正在播放」依次
  回退，所以收藏（`s`）、加入队列（`a`）、从云端歌单移出（`d`）在更多位置都能用。
  也修了它在队列没选中时直接放弃的问题。
- **侧边栏点「音源」没反应**：点击走的是 `Tab::from_number`，那是数字键映射、
  只覆盖前 10 页，而音源排第 11 位 → 得到 `None`，什么都不做。鼠标现在不受
  键盘那十个键的限制。
- **VIP / 下架歌曲拿不到播放地址**（多处，逐条修）：
  - `/privilege/lite` 必须用 **GET + `hash` 参数**。服务端读的是顶层 `hash`
    （逗号分隔可传多个），不是 `resource` 数组；用 POST + `resource` 会得到
    `error_code 20010 "hash and AlbumAudioID is empty"`，而 20010 被我们的客户端
    当成「需要登录」，整条路径静默失效 —— 日志里那句「需要登录」跟登录态无关。
  - **概念版必须传 `ppage_id=356753938`**（单个）。不传时服务端用的默认三段值
    `356753938,823673182,967485191` 会让下架歌返回 `status=3`。标准版忽略客户端
    传的值（用自己硬编码），所以统一传它对两种平台都安全。
  - **绝不传 `album_audio_id`**：实测带它时服务端**只按它取歌、忽略 hash**——
    固定一个 aid 配三个不同 hash 返回的是同一个 URL，会**静默播成另一首歌**。
    MoeKoeMusic 也只传 hash 定位。
  - **候选逐个试、单个失败不中断**：某组合返回 502 时不再让整条流程失败。
  - `status=3` 的文案修正：它不是「无版权/下架」，而是「文件标识与账号权限
    不匹配」，原文案会误导用户以为歌下架了。
- **终端能力探测会吞掉整个键盘**：`Picker::from_query_stdio()` 发查询后阻塞读
  stdin，而那个读没有自身超时；终端不回应时（tmux、部分终端、某些 SSH）线程
  永远卡在 `stdin().read()`，之后所有按键都被它吃掉。改为只看环境变量。
  顺带把探测结果（协议类型 + 单元格像素尺寸）写进日志：封面显示不对时，
  先看这一条就知道是不是根本没命中图形协议。
- **首页封面「没有完全填满」**：根因不是没放大，而是**框和图的形状对不上**。
  `ratatui-image` 的三种 `Resize`（`Fit` / `Scale` / `Crop`）**全都保持宽高比**——
  `Scale` 并不是「拉伸到区域一样大」，它只是「允许放大」，仍然等比。而封面区
  的像素比例（约 1.5:1）与方形封面永远对不上，于是不管选哪个都留黑边。
  现在按目标区域的像素尺寸先裁剪/缩放图片本身（等价 CSS `object-fit: cover`），
  再交给协议渲染，做到真正铺满。
- **侧边栏里按 `↑` `↓` / `j` `k` 跳到了错误的页**：移动走的是 `Tab::ALL`
  （数字键落点），而侧边栏**显示**的是 `Tab::SIDEBAR_ORDER`，两者顺序不同——
  从「队列」往下按会跳到「歌词」，可屏幕上下一个明明写着「首页」。首/末项跳转
  （`g` / `G`）同理。现在统一按显示顺序走，鼠标点击的命中区也改成按显示顺序解释。
- **`0` 键按下去没有任何反应**：键位表的分支写的是 `'1'..='9'`，把 `0` 漏掉了。
  侧边栏印着「0 可视化」、文档写着「1–9、0」、`Tab::number_key()` 也照常返回
  `'0'`——界面和文档一起骗人，而且不会报错。现在 `0`-`9` 全部可用。
- **文档里两处快捷键写错**：设置页把歌词面板标成 `y`（实际是 `l`）、侧边导航标成
  `b`（实际是 `\`）；帮助面板与 KEYBINDINGS 还宣称数字键能「在列表内跳到第 N 项」，
  但那个功能并不存在（焦点通常在歌曲列表上，真做出来反而会让最常用的「按数字切页」
  失效）。已按实际行为改正。
- **播放条右侧那段被提前截断**：那段宽度是按**字符数**算的，而截断是按**显示宽度**
  做的——「下一首 <中文歌名>」里歌名一个字占两列，算出来的宽度明显偏小，于是
  明明还有空间也被截成「下一首 WE GO · 播放中 · …」。改用 `display_width` 计算。
  （是为 README 抓真实截图时才看出来的：截图里那个省略号很显眼。）

### 文档

以下都是**过期或被功能改动落下**的说法，已按当前代码改正：

- **「边下边播没做」**：README、FAQ、CHANGELOG 三处都还写着「先下载再解码，
  不是边下边播」。实际上早已实现（攒够 128 KB 就开播）。顺带补上它的真正限制：
  读指针跑到未下到的位置会阻塞，以及**续播上次位置时不走流式**（会先下完）。
- **Rust 版本**：README / CONTRIBUTING / 一处源码注释写 1.85，实际 `rust-version`
  是 **1.86**（由 `ratatui-image` 11.x 决定）。
- **Node 版本**：写「16+」，上游 `engines` 实际是 `>=12`。
- **平台**：写「linux-macos-windows」，但 `zbus` 在 Windows 上要求 `async-io`
  而这里只开了 `tokio`，所以 Windows 编译不过。改成 `linux`，并在文档里说明。
- **内存**：README 写 14 MiB、DESIGN/CONFIGURATION 写 13.8/16.8 MiB，四处互相
  对不上。重新实测（VmRSS）：空数据 14.2、已登录首页 15.3、歌单页 15.8、
  播放中 16.9 MiB。
- **体积与依赖数**：6.7 MiB → **6.8 MiB**；`Cargo.lock` 468 → **469** 个包。
- **快速上手的「按 `2` 进歌单广场」**：数字键调整后歌单是 `3`，`2` 是搜索。
- **第三方许可表**：数字过期（427/21/13/7），且声称「由脚本自动提取」而脚本根本
  没进仓库。现在按 `scripts/license-stats.py` 重算为 429 / 13 / 26，并**把脚本
  真的提交进来**，以后可重新生成。
- **终端要求**：写「256 色」，但主题与逐字渐变需要真彩（24 位）才完整。

## [0.1.0] - 2026-09-20

首个可用版本，已发布至 https://github.com/sijin-xb/kugou-tui

### 新增

- **MPRIS 桌面集成**：注册为 `org.mpris.MediaPlayer2.kugou-tui`。
  状态栏 / 媒体控件 / `playerctl` 可直接控制并显示封面；支持拖进度条（SetPosition）。
  没有 D-Bus 时自动跳过，不影响播放。

- **音频可视化**：导航第 6 个标签页（按 `6` 进入）。
  数据取自音频线程在透传采样时记录的真实峰值——**不是随机动画**：
  静音会掉到底，鼓点会顶到头。带「快起慢落」缓动与峰值保持。
- **歌词译文 / 音译**：解析 KRC 里 `[language:base64]` 标签（官方未记载），
  按行挂到歌词上。请求格式由 `fmt=lrc` 改为 `fmt=krc`——译文只存在于 KRC。

### 修复

- **音源不再回退**：顶层 `api_base` 与 `sources.active` 是两个各自独立持久化的
  字段，启动时未对齐，导致「退出前是概念版、重启变回标准版」，需手动按 `v`。
  已在 `Config::load()` 与 `merge_cli()` 之间统一（命令行 `--api-base` 仍可覆盖单次会话）。
- **搜索结果歌名为空**：`/search` 返回的歌名字段是 `FileName`（大写 F/N），
  候选键名里只有小写 `filename`，大小写不匹配导致全部显示 `-`。
- **可视化页方向键完全无响应**：`move_selection` 里可视化分支写在最前面，
  抢在 `(_, Focus::Sidebar)` 之前，导致该页连侧边栏切换标签都不响应。已移到最后。
- **音源档案被污染**：`sync_active_source()` 曾把运行时 `api_base` 回写进音源档案，
  用 `--api-base` 临时指向别处后会永久改坏该音源地址。已改为只同步 cookie / dfid。

### 变更

- 依赖的 KuGouMusicApi 锁定到 `a5a9801`（上游活跃，接口字段会变）。
- 修正过时的「酷狗 ↔ 网易云」注释——网易云音源早已移除，实际是「酷狗 ↔ 酷狗概念版」。

### 已知限制

- 歌词译文只在服务端 KRC 带 `[language:]` 标签时才有，部分歌曲没有。
- `/artist/lists` 偶发返回上游 502，此时需手动 `R` 刷新（尚未加重试）。
