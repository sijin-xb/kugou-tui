//! 流式音频缓冲：边下边播的底座。
//!
//! # 为什么需要它
//!
//! `rodio::Decoder<R>` 要求 `R: Read + Seek`（见 docs.rs，rodio 0.22 没有
//! `new_stream`）。网络流只能 `Read` 不能 `Seek`，所以不能直接喂给 rodio。
//!
//! 这里在内存里攒一段**会增长**的字节缓冲区，并且让它实现 `Seek`：
//! 读指针还没下载到的地方就**阻塞等待**后台下载线程补数据。Decoder 因此
//! 以为自己在读一个"很大的本地文件"，实际数据还在路上。
//!
//! # 阻塞的代价
//!
//! `read()` 阻塞时 rodio 的播放线程就停在那儿——表现是**声音卡住**（不是结束），
//! 而进度不动。所以每个等待都带超时：数据停了 [`WAIT_TIMEOUT`] 之后要能
//! 把控制权交回上层（见下节「断了怎么办」），不能永远卡死——音频线程在
//! `Player::clear()` 里等这条源退出，真的永久阻塞会让切歌一起卡住。
//!
//! # 内存：只留一个窗口，其余落盘
//!
//! 缓冲**不保留整首**。下载的每个字节都会先落到磁盘（[`StreamingBuffer::push`]
//! 的调用方保证「先写文件、再 push」），内存里只留读指针附近的一个尾部窗口
//! （[`MAX_KEPT_BYTES`]）。窗口之内的读取零系统调用；窗口之外（下载早跑到
//! 前面去了，或者解码器往回 seek）用 `pread` 从落盘文件读回来——那本来就是
//! 刚落盘的数据，在页缓存里，开销接近一次 memcpy。
//!
//! 于是常驻内存与曲目体积**无关**：放一首 65 MiB 的 Hi-Res，内存代价还是
//! 那几 MiB 窗口。这修掉的正是「连续播放 / 频繁切歌之后 RSS 涨到一百多 MB
//! 且不回落」——旧实现把整首（而且是 `Vec` 倍增过的整首）都压在内存里，
//! 切歌时下载任务还攥着它不放。
//!
//! # 断了怎么办
//!
//! 读超时、下载失败、下载被取消，三者都让读指针等不到数据。这里只如实
//! 记下状态（[`StreamingBuffer::is_finished`] / [`StreamingBuffer::error`] /
//! [`StreamingBuffer::is_cancelled`]），**不自己决定是"歌放完了"还是"出错了"**
//! ——那是播放引擎的事（它知道曲目总时长和上下文，见 `engine::Runtime::sync`）。
//! 旧实现让这些情况统统表现为解码器读到 EOF，而 rodio 把 EOF 当作"这首放完了"，
//! 于是单曲循环下会**从头再放一遍**（用户看到的就是"进度回到开头"）。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// 等数据的最长时间。超过就当作这次流断了，让上层去决定「续播」还是「报错」。
const WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// 内存里最多保留多少已下载的字节。
const MAX_KEPT_BYTES: usize = 4 * 1024 * 1024;

/// 触发回收后回落到多少。
///
/// 留一段比上限小得多的水位，回收（一次 `split_off` 加一次分配）就不会每收到
/// 一个块都跑一遍。
const TARGET_KEPT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
struct Inner {
    /// 内存里保留的一段字节，覆盖 `[base, base + data.len())`。
    ///
    /// 只保留尾部窗口（见 [`MAX_KEPT_BYTES`]），**不是整首**。
    data: Vec<u8>,
    /// `data[0]` 在整首音频里的偏移。
    base: u64,
    /// 已经落盘的字节数。`[0, flushed)` 都能从落盘文件里读回来。
    flushed: u64,
    /// 服务端给的总长度（`Content-Length`），拿不到就是 None。
    total: Option<u64>,
    /// 下载任务是否已经收工（成功、失败、取消都算）。
    finished: bool,
    /// 下载线程填进来的错误信息。
    error: Option<String>,
    /// 用户切歌 / 停止，这次下载被主动取消。
    cancelled: bool,
}

/// 内存窗口的两个水位。
///
/// 做成值（而不是到处直接读常量）是为了测试能注入一组小水位：验证回收逻辑不需要
/// 真去写一个 40 MiB 的临时文件——`/tmp` 可能是个小 tmpfs，测试不该依赖它有多大。
#[derive(Debug, Clone, Copy)]
struct Window {
    /// 超过它就回收。
    max: usize,
    /// 回收后回落到多少。
    target: usize,
}

/// 生产参数。
const PROD_WINDOW: Window = Window {
    max: MAX_KEPT_BYTES,
    target: TARGET_KEPT_BYTES,
};

/// 一边下载一边增长的音频缓冲。
///
/// 克隆出来的实例共享同一份数据与同一个读位置以外的状态：下载任务持有一份写，
/// 播放线程持有一份读。
#[derive(Clone, Debug)]
pub struct StreamingBuffer {
    inner: Arc<(Mutex<Inner>, Condvar)>,
    /// 落盘文件的只读句柄。窗口之外的数据靠它 `pread` 读回来。
    ///
    /// `None` 表示没有落盘文件（只有测试会这样构造），此时退化成「全部留在
    /// 内存里」的旧行为。留着它也让 `push` 的回收逻辑不必关心盘上的事。
    spill: Option<Arc<File>>,
    /// 内存窗口的水位。
    window: Window,
    /// 读指针。每个克隆有自己的读位置（播放线程只需要一个读者）。
    pos: u64,
}

impl StreamingBuffer {
    /// 建一个**不落盘**的缓冲：所有下载到的字节都留在内存里。
    ///
    /// 只给测试用。生产路径一律 [`Self::with_spill`]——不落盘就没有回收的
    /// 依据（窗口之外的字节将无处可读），`push` 也会因此退化成"全部留在
    /// 内存里"的旧行为，而那正是要修掉的东西。
    #[cfg(test)]
    pub fn new(total: Option<u64>) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(Inner {
                    total,
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            spill: None,
            window: PROD_WINDOW,
            pos: 0,
        }
    }

    /// 建一个落盘到 `file` 的缓冲。
    ///
    /// 调用方必须保证：**每一段 [`Self::push`] 进来的字节，都已经先写进了
    /// `file`**。这是「内存窗口之外的字节可以从磁盘读回来」这条前提的全部依据，
    /// 顺序反了就会读到空洞（读到全 0 的音频，听起来是噪音或静音）。
    pub fn with_spill(total: Option<u64>, file: Arc<File>) -> Self {
        Self::build(total, file, PROD_WINDOW)
    }

    /// 测试用：指定一组小水位，好在几 KiB 的负载上验证回收与回读。
    ///
    /// 生产参数本身（4 MiB / 1 MiB）的实测数据记在 docs/MAINTENANCE.md 的
    /// 内存对照实验里——那种量级的对照要在进程外测才算数，不适合塞进单元测试。
    #[cfg(test)]
    pub fn with_window_for_test(
        total: Option<u64>,
        file: Arc<File>,
        max: usize,
        target: usize,
    ) -> Self {
        Self::build(total, file, Window { max, target })
    }

    fn build(total: Option<u64>, file: Arc<File>, window: Window) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(Inner {
                    total,
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            spill: Some(file),
            window,
            pos: 0,
        }
    }

    /// 已经下载到的字节数。
    ///
    /// 不随内存窗口回收而减少——它说的是「下到哪了」，不是「内存里还有多少」。
    pub fn buffered_bytes(&self) -> u64 {
        let (lock, _cvar) = &*self.inner;
        let inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        inner.flushed
    }

    /// 下载任务是否已经收工（成功、失败、取消都算）。
    ///
    /// 判断「还有没有可能等到更多数据」用它；判断「这首是不是完整下好了」
    /// 用 [`Self::is_complete`]。
    pub fn is_finished(&self) -> bool {
        let (lock, _cvar) = &*self.inner;
        let inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        inner.finished
    }

    /// 整首是否已经完整下完（没出错、没被取消）。
    ///
    /// 播放引擎用它区分「真的放完了」与「数据断了」：流没下完播放器就空了，
    /// 那不是正常结束。
    pub fn is_complete(&self) -> bool {
        let (lock, _cvar) = &*self.inner;
        let inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        inner.finished && inner.error.is_none() && !inner.cancelled
    }

    /// 这次下载是不是被主动取消的（用户切歌 / 停止）。
    ///
    /// 取消不是错误：上层据此**静默**收场，不弹错误、不切歌。
    pub fn is_cancelled(&self) -> bool {
        let (lock, _cvar) = &*self.inner;
        let inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        inner.cancelled
    }

    /// 下载失败的原因。正常结束（含尚未结束）时为 `None`。
    pub fn error(&self) -> Option<String> {
        let (lock, _cvar) = &*self.inner;
        let inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        inner.error.clone()
    }

    /// 追加一段下载到的数据，并唤醒正在等它的读线程。
    ///
    /// **调用前必须已经把这段字节写进 [`Self::with_spill`] 的那个文件**，
    /// 否则窗口之外的读取会读到空洞。
    pub fn push(&self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        // 已经被取消就别再往里堆了：没人会读，堆进去只是白占内存
        if inner.cancelled {
            return;
        }
        inner.data.extend_from_slice(chunk);
        inner.flushed += chunk.len() as u64;

        // 落了盘的字节不必都留在内存里：超出窗口就把前面那段丢掉，读的时候
        // 再从磁盘取。`split_off` 出来的新块容量正好等于长度，旧的大块在这里
        // 释放——RSS 因此不会随曲目体积线性上涨。
        if self.spill.is_some() && inner.data.len() > self.window.max {
            let dropped = inner.data.len() - self.window.target;
            let tail = inner.data.split_off(dropped);
            inner.base += dropped as u64;
            inner.data = tail;
        }

        drop(inner);
        cvar.notify_all();
    }

    /// 标记下载结束。`error` 非空表示失败——读线程据此报错，不再傻等。
    pub fn finish(&self, error: Option<String>) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        inner.finished = true;
        inner.error = error;
        drop(inner);
        cvar.notify_all();
    }

    /// 取消这次下载（用户切歌 / 停止播放）。
    ///
    /// 除了置标志位，还会**把内存窗口立刻交还**并唤醒正阻塞在 `read()` 里的
    /// 解码线程——否则切歌要等到下一次读超时（最多 15 秒）才真的放开，
    /// 而音频线程正在 `Player::clear()` 里等它。
    pub fn cancel(&self) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        if inner.finished {
            return;
        }
        inner.cancelled = true;
        inner.finished = true;
        // 有落盘文件才敢丢窗口：没有它，被丢掉的字节就没处读了
        if self.spill.is_some() {
            inner.base = inner.flushed;
            inner.data = Vec::new();
        }
        drop(inner);
        cvar.notify_all();
    }

    /// 等到 `target` 字节之前的数据都已落盘可用。
    ///
    /// * `Ok(Some(flushed))` —— 够用了，`flushed` 是当前可读到的总字节数；
    /// * `Ok(None)` —— 下载已经收工，而数据确实不够（正常 EOF）；
    /// * `Err` —— 等待超时，或下载失败（错误信息原样带出来）。
    ///
    /// 超时是**每次等不到新数据**重新计时的，不是总预算：下载慢但一直在动，
    /// 读就一直等下去（这才是边下边播该有的表现——声音停一下再继续）。
    fn wait_available(&self, target: u64) -> std::io::Result<Option<u64>> {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());

        while inner.flushed < target && !inner.finished {
            let (guard, timeout) = cvar
                .wait_timeout(inner, WAIT_TIMEOUT)
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
            if timeout.timed_out() && inner.flushed < target {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "等待音频数据超时：想要 {} 字节，只有 {} 字节",
                        target, inner.flushed
                    ),
                ));
            }
        }

        if inner.flushed >= target {
            return Ok(Some(inner.flushed));
        }
        // 下载结束了还是不够 —— 把原因说清楚（失败 / 取消 / 确实到末尾了）。
        // 注意错误只在**真读不到**的时候才抛：已经下到的部分照常能播，
        // 不该因为后半段失败就把用户已经听到的东西也掐掉。
        match inner.error.clone() {
            Some(error) => Err(std::io::Error::other(error)),
            None => Ok(None),
        }
    }

    /// 等下载彻底收工（成功或失败都算）。拿不到总长度时估算"末尾"用。
    fn wait_finished(&self) {
        let (lock, cvar) = &*self.inner;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        while !inner.finished {
            let (guard, _timeout) = cvar
                .wait_timeout(inner, WAIT_TIMEOUT)
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
        }
    }
}

/// 从内存窗口里拷一段。窗口没覆盖到 `pos` 时返回 `None`（调用方改走磁盘）。
fn copy_from_window(inner: &Inner, pos: u64, out: &mut [u8]) -> Option<usize> {
    if inner.data.is_empty() {
        return None;
    }
    let start = pos.checked_sub(inner.base)? as usize;
    if start >= inner.data.len() {
        return None;
    }
    let count = out.len().min(inner.data.len() - start);
    out[..count].copy_from_slice(&inner.data[start..start + count]);
    Some(count)
}

impl Read for StreamingBuffer {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // 先把读指针需要的数据等来
        let Some(available) = self.wait_available(self.pos + 1)? else {
            // 已经到文件末尾了，正常结束
            return Ok(0);
        };

        // 这一次最多能读到哪里：不能越过已经下载到的位置
        let remaining = available.saturating_sub(self.pos);
        let want = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));

        // 路径一：命中内存窗口——常规情况，零系统调用
        let from_window = {
            let (lock, _cvar) = &*self.inner;
            let inner = lock.lock().unwrap_or_else(|e| e.into_inner());
            copy_from_window(&inner, self.pos, &mut buf[..want])
        };
        if let Some(count) = from_window {
            self.pos += count as u64;
            return Ok(count);
        }

        // 路径二：这段被回收掉了，从落盘文件读回来。
        //
        // 用的是同一个 inode：下载完成后文件会被改名成正式缓存文件，缓存回收
        // 也可能把它删掉——只要这个句柄还在，数据就读得到。
        let Some(file) = self.spill.as_ref() else {
            return Ok(0);
        };

        let mut offset = self.pos;
        let mut filled = 0;
        while filled < want {
            match file.read_at(&mut buf[filled..want], offset) {
                Ok(0) => break,
                Ok(count) => {
                    filled += count;
                    offset += count as u64;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.pos += filled as u64;
        Ok(filled)
    }
}

impl Seek for StreamingBuffer {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let (lock, _cvar) = &*self.inner;
        let total_hint = {
            let inner = lock.lock().unwrap_or_else(|e| e.into_inner());
            inner.total
        };

        let target = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(offset) => {
                // 流式播放时文件还没下完，"末尾"只能用声明的总长度估算
                let total = total_hint.unwrap_or_else(|| {
                    self.wait_finished();
                    self.buffered_bytes()
                });
                total.saturating_add_signed(offset)
            }
            SeekFrom::Current(offset) => self.pos.saturating_add_signed(offset),
        };

        // 往回 seek 到已经被回收的区域是**允许的**——那部分数据在落盘文件里，
        // 读的时候从磁盘取。只有"还没下载到"的位置才等（或报错）。
        match self.wait_available(target)? {
            Some(_) => {
                self.pos = target;
                Ok(target)
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "音频数据不足：需要 {} 字节，实际 {} 字节",
                    target,
                    self.buffered_bytes()
                ),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};
    use std::thread;

    /// 造一个落盘缓冲：临时文件 + 文件句柄，返回 (缓冲, 文件, 路径)。
    ///
    /// 和 `download::start_streaming` 一样按**读写**打开——缓冲要 `pread`
    /// 读回被回收的字节，只写打开的句柄做不了这件事。
    ///
    /// `window` 给 `Some((max, target))` 就用这组小水位（在生产水位下验证回收
    /// 得写好几 MiB 进 `/tmp`，而 `/tmp` 完全可能是个小 tmpfs）。
    fn spilled(
        total: Option<u64>,
        window: Option<(usize, usize)>,
    ) -> (StreamingBuffer, Arc<File>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "kugou-tui-stream-test-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let file = Arc::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .expect("建临时文件"),
        );
        let buffer = match window {
            Some((max, target)) => {
                StreamingBuffer::with_window_for_test(total, Arc::clone(&file), max, target)
            }
            None => StreamingBuffer::with_spill(total, Arc::clone(&file)),
        };
        (buffer, file, path)
    }

    /// 「先写文件、再 push」是缓冲的不变量，测试里也要照办。
    fn write_and_push(buffer: &StreamingBuffer, file: &File, chunk: &[u8]) {
        let mut sink = file;
        std::io::Write::write_all(&mut sink, chunk).expect("写临时文件");
        buffer.push(chunk);
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_buffered_data() {
        let buffer = StreamingBuffer::new(Some(4));
        buffer.push(b"abcd");
        buffer.finish(None);

        let mut reader = buffer.clone();
        let mut out = [0u8; 4];
        assert_eq!(reader.read(&mut out).unwrap(), 4);
        assert_eq!(&out, b"abcd");
        // 读完就是 EOF
        assert_eq!(reader.read(&mut out).unwrap(), 0);
    }

    #[test]
    fn read_blocks_until_data_arrives() {
        let buffer = StreamingBuffer::new(None);
        let writer = buffer.clone();

        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            writer.push(b"late");
            writer.finish(None);
        });

        // 下载线程还没动，这里会阻塞等——正是边下边播要的行为
        let mut reader = buffer.clone();
        let mut out = [0u8; 4];
        assert_eq!(reader.read(&mut out).unwrap(), 4);
        assert_eq!(&out, b"late");
        handle.join().expect("下载线程正常结束");
    }

    #[test]
    fn seek_beyond_buffer_reports_error() {
        let buffer = StreamingBuffer::new(Some(100));
        buffer.push(b"ab");
        buffer.finish(None);

        let mut reader = buffer.clone();
        // 声明 100 字节但只下了 2 字节，拖到第 50 字节应当失败而不是假装成功
        assert!(reader.seek(SeekFrom::Start(50)).is_err());
    }

    #[test]
    fn seek_within_buffer_works() {
        let buffer = StreamingBuffer::new(Some(6));
        buffer.push(b"abcdef");
        buffer.finish(None);

        let mut reader = buffer.clone();
        reader.seek(SeekFrom::Start(3)).expect("前 3 字节已下载");
        let mut out = [0u8; 3];
        assert_eq!(reader.read(&mut out).unwrap(), 3);
        assert_eq!(&out, b"def");
    }

    /// 下载失败的**原因**要能传到读者手上，但已经下到的部分照常可读。
    ///
    /// 顺序很重要：先读完手里那两字节，再读第三字节才报错。旧实现一进
    /// `read()` 就把错误抛出来，等于「后半段没下完」顺手把前半段也掐了。
    #[test]
    fn download_error_surfaces_after_buffered_bytes_are_consumed() {
        let buffer = StreamingBuffer::new(None);
        buffer.push(b"ab");
        buffer.finish(Some("连接被重置".to_string()));

        let mut reader = buffer.clone();
        let mut out = [0u8; 8];
        assert_eq!(reader.read(&mut out).unwrap(), 2, "已下到的部分应当能读");
        assert_eq!(&out[..2], b"ab");

        // 只有 2 字节，读第 3 字节时应当把下载错误抛出来
        reader.read(&mut out).expect_err("下载失败应当报错");
    }

    /// 内存窗口是**有上限**的：下再多字节也不会在内存里留那么多。
    ///
    /// 这条钉的是「连续播放 / 频繁切歌后 RSS 破百 MB」那个问题：旧实现把整首
    /// 都留在 `Vec` 里（而且还倍增过），现在只留一个窗口，其余靠落盘文件。
    ///
    /// 这里用注入的小水位（4 KiB）跑等价逻辑——生产水位是 4 MiB，真去写几十 MiB
    /// 的临时文件只会让测试挑磁盘，验证不了更多东西。
    #[test]
    fn keeps_only_a_bounded_window_in_memory() {
        let (buffer, file, path) = spilled(None, Some((4 * 1024, 1024)));
        let chunk = vec![7u8; 512];

        // 512 字节 × 64 = 32 KiB，8 倍于窗口上限
        for _ in 0..64 {
            write_and_push(&buffer, &file, &chunk);
        }
        buffer.finish(None);

        let (lock, _cvar) = &*buffer.inner;
        let held = lock.lock().unwrap().data.len();
        assert!(held <= 4 * 1024, "内存窗口不该超过上限：{held} > 4096");
        assert_eq!(buffer.buffered_bytes(), 64 * 512, "下到哪了不受回收影响");

        cleanup(&path);
    }

    /// 被回收掉的字节仍然读得到——从落盘文件里取。
    #[test]
    fn reads_reclaimed_bytes_back_from_disk() {
        let (buffer, file, path) = spilled(None, Some((4 * 1024, 1024)));
        let chunk: Vec<u8> = (0..=255u8).collect();
        let chunk = chunk.repeat(4); // 1 KiB，内容可预测
        // 32 KiB，足以让头部被回收好几轮
        for _ in 0..32 {
            write_and_push(&buffer, &file, &chunk);
        }
        buffer.finish(None);

        // 从开头顺序读一段，再往回 seek 到已经被回收的位置（模拟解码器回头）
        let mut reader = buffer.clone();
        let mut out = vec![0u8; 1024];
        assert_eq!(reader.read(&mut out).unwrap(), out.len());
        assert_eq!(&out[..], &chunk[..]);

        reader.seek(SeekFrom::Start(1000)).expect("落盘数据可回读");
        let mut small = [0u8; 16];
        assert_eq!(reader.read(&mut small).unwrap(), 16);
        assert_eq!(&small, &chunk[1000 % chunk.len()..1000 % chunk.len() + 16]);

        cleanup(&path);
    }

    /// 取消之后：读得到已下到的部分，然后是干净的 EOF（不是错误）。
    #[test]
    fn cancel_keeps_buffered_bytes_and_ends_cleanly() {
        let (buffer, file, path) = spilled(None, None);
        write_and_push(&buffer, &file, b"abcde");

        let mut reader = buffer.clone();
        buffer.cancel();

        assert!(buffer.is_cancelled());
        assert!(!buffer.is_complete(), "被取消的不算完整下载");
        assert!(buffer.error().is_none(), "取消不是失败");

        let mut out = [0u8; 8];
        assert_eq!(reader.read(&mut out).unwrap(), 5);
        assert_eq!(&out[..5], b"abcde");
        assert_eq!(reader.read(&mut out).unwrap(), 0, "取消后是 EOF，不是报错");

        cleanup(&path);
    }

    /// 已取消的缓冲不再往里堆数据——没人会读，堆进去只是白占内存。
    #[test]
    fn push_after_cancel_is_ignored() {
        let (buffer, file, path) = spilled(None, None);
        write_and_push(&buffer, &file, b"ab");
        buffer.cancel();

        let before = buffer.buffered_bytes();
        buffer.push(b"cdef");
        assert_eq!(buffer.buffered_bytes(), before);

        cleanup(&path);
    }

    /// `is_finished` 与 `is_complete` 的分工：前者说"任务收工了"，后者说
    /// "整首完好"。播放引擎靠这个区分「真的放完了」和「下载失败」。
    #[test]
    fn finished_and_complete_are_different_things() {
        let failed = StreamingBuffer::new(None);
        failed.finish(Some("超时".to_string()));
        assert!(failed.is_finished());
        assert!(!failed.is_complete());

        let ok = StreamingBuffer::new(None);
        ok.finish(None);
        assert!(ok.is_finished());
        assert!(ok.is_complete());
    }
}
