//! 音频流下载。
//!
//! # 两条路径：边下边播，以及「续播时先下完」
//!
//! **默认走流式**（[`Downloader::start_streaming`]）：取到直链后先攒够
//! [`PREROLL_BYTES`]（128 KB，约 8 秒音频）就交给解码器开播，剩下的在后台
//! 继续下、同时落盘到缓存。所以首播的等待是「攒开头」，不是「下完整首」。
//!
//! rodio 的解码器要求 `Read + Seek`，所以流式缓冲得自己实现一个会增长、
//! 且能 `Seek` 的 `Read`（见 [`crate::audio::streaming`]）。它的代价是
//! **读指针跑到还没下到的位置会阻塞等数据**——表现是声音停一下再继续。
//!
//! 但有一个例外：**要跳到中间去（续播上次的位置）时不能走流式**。
//! 缓冲里只有开头那点数据，seek 到几百秒的位置会一直等下载、超时失败，
//! 结果从头播——用户实测反馈「续播会直接从最开始听」就是这个原因。
//! 那种情况改走 [`Downloader::fetch_to`]，把整首下完再播。
//!
//! 落盘下载换来的好处是实打实的：
//!
//! * 常驻内存只有解码缓冲（几百 KB），与歌曲码率无关；
//! * 拖进度条是真正的随机访问，没有「跳不过去」的区域；
//! * 重复播放零网络开销；
//! * 断点续传、失败重试都变成简单的文件操作。
//!
//! # 流式下载写的是 `.part`
//!
//! 两条路径都先写同目录下的 `.part`，**全部写完才改名**成正式缓存文件。
//! 流式那条尤其重要：它一边下一边播，中途失败/被取消是常态，若直接写正式
//! 文件名，磁盘上就会留下一个「看起来完整、其实只有半首」的文件，下次播放
//! 命中它就会在中间莫名其妙地结束（而且 `cache.find` 是按文件存在与否判断的，
//! 它看不出长短）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};

use super::streaming::StreamingBuffer;
use crate::error::{AppError, Result};
use crate::logger::tlog;
use std::io::Write;

/// 走分块并发的门槛。小文件并发反而慢（每次分块都要建一次连接），不值得。
const PARALLEL_MIN_BYTES: u64 = 512 * 1024;
/// 最多分几块。再多 CDN 也未必给更快，还容易触发限流。
const PARALLEL_MAX_CHUNKS: u64 = 4;
/// 单块大小的下限，避免把一首歌切成几十个碎片。
const MIN_CHUNK_BYTES: u64 = 256 * 1024;

/// 一段 Range 请求要下载的范围（闭区间，`end` 是最后一个字节的下标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkRange {
    start: u64,
    end: u64,
}

impl ChunkRange {
    /// 这一段有多少字节。闭区间，所以是 `end - start + 1`。
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

/// `url` 是否指向一个本地文件（汽水解密后的产物）。
///
/// 调用方用它决定「要不要走流式下载」：本地文件已经完整躺在盘上，
/// 流式缓冲那套「边下边播」对它没有意义。
pub fn is_local_url(url: &str) -> bool {
    url.starts_with("file://")
}

/// 把 `file://` URL 还原成本地路径；不是本地 URL 则返回 `None`。
///
/// 只认 `file://` 开头，且**必须**是绝对路径（`file:///...`）。
/// 相对路径的 `file://` 在不同平台上含义不同，宁可当成普通 URL 交给
/// 上层报错，也不要猜。
fn local_file_path(url: &str) -> Option<std::path::PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // Windows 上是 file:///C:/path，Unix 上是 file:///path —— 都要能吃下。
    // 去掉可能的 leading slash（Unix 保留，Windows 的 /C:/ 需要去掉）。
    let path = if cfg!(windows) {
        rest.strip_prefix('/').unwrap_or(rest)
    } else {
        rest
    };
    // URL 里的百分号编码要还原（路径里有空格/中文时 reqwest 类库会这么写）
    Some(std::path::PathBuf::from(percent_decode(path)))
}

/// 还原 `%XX` 编码。非法转义原样保留（宁可路径报错，也不要静默改写）。
fn percent_decode(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 把本地文件复制到 `target`，带进度回调。
///
/// 写 `.part` 再改名，与网络下载路径一致：中途失败不会留下
/// 「看起来完整」的坏缓存文件。
///
/// 整首复制在 `spawn_blocking` 里做：几十 MB 的拷贝是纯 IO，
/// 放异步线程上会把那个工作线程占住。
async fn copy_local(source: &Path, target: &Path, progress: ProgressFn<'_>) -> Result<u64> {
    let metadata = tokio::fs::metadata(source)
        .await
        .map_err(|error| AppError::io_at(source.display().to_string(), error))?;
    let total = metadata.len();

    let source = source.to_path_buf();
    let target = target.to_path_buf();
    let temp = target.with_extension("part");
    let source_for_task = source.clone();
    let temp_for_task = temp.clone();

    let copied = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
        std::fs::copy(&source_for_task, &temp_for_task)
    })
    .await
    .map_err(|error| AppError::Other(format!("本地文件复制任务失败：{error}")))?
    .map_err(|error| AppError::io_at(source.display().to_string(), error))?;

    // 失败时清掉半截的 .part，否则下次会拿它当缓存（`cache.find` 只看文件存在）
    if let Err(error) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(AppError::io_at(target.display().to_string(), error));
    }

    progress(copied, Some(total));
    Ok(copied)
}

/// 解析 `Content-Range: bytes <start>-<end>/<total|*>`。
///
/// 只认这一种写法。服务端回 `Content-Range` 的目的就是声明「这确实是你要的那一段」，
/// 认不出来的形式（多段、非 bytes 单位、范围倒挂）一律当作不可信——调用方会退回
/// 单连接重下，而不是拿一个来路不明的响应去拼文件。
///
/// 返回 `(start, end, total)`；总长度写成 `*` 时为 `None`。
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        other => Some(other.parse::<u64>().ok()?),
    };
    Some((start, end, total))
}

/// 校验一个 Range 响应确实是「请求的那一段」。
///
/// **这是分块下载里最要紧的一道闸。** 只判 `is_success()` 是不够的：CDN 或中间代理
/// 忽略 `Range` 时会回 `200 OK` 加**整首**内容。并发分块下，四个任务各自
/// `seek(自己的起点)` 再写整首，互相覆盖，最后得到一个「字节数正确、内容全错」的
/// 文件——而且它会照常改名成正式缓存文件，之后每次播放都命中这个坏文件。
///
/// 所以这里要求三件事同时成立：状态码必须是 `206 Partial Content`、`Content-Range`
/// 必须能解析、解析出来的范围必须**逐字节等于**请求的范围（顺带核对总长度与 HEAD
/// 声明的一致）。任何一条不满足都返回错误，由 [`Downloader::fetch_to`] 退回单连接
/// 重下——宁可慢一次，不能坏一个缓存文件。
fn validate_range_response(
    range: ChunkRange,
    status: reqwest::StatusCode,
    content_range: Option<&str>,
    total: u64,
) -> Result<()> {
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(AppError::Audio(format!(
            "服务端忽略了 Range 请求（HTTP {}，期望 206）",
            status.as_u16()
        )));
    }
    let Some(value) = content_range else {
        return Err(AppError::Audio(
            "Range 响应没有 Content-Range 头".to_string(),
        ));
    };
    let Some((start, end, declared_total)) = parse_content_range(value) else {
        return Err(AppError::Audio(format!("Content-Range 无法解析：{value}")));
    };
    if start != range.start || end != range.end {
        return Err(AppError::Audio(format!(
            "Content-Range 与请求不符：请求 {}-{}，服务端回 {start}-{end}",
            range.start, range.end
        )));
    }
    if declared_total.is_some_and(|declared| declared != total) {
        return Err(AppError::Audio(format!(
            "Content-Range 报的总长度与 HEAD 不一致（HEAD 说 {total}）"
        )));
    }
    Ok(())
}

/// 下载过程中的进度回调：`(已下载字节, 总字节)`。总字节在服务端不给
/// `Content-Length` 时为 `None`。
pub type ProgressFn<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

/// 音频下载器。
///
/// 与 [`crate::api::ApiClient`] 分开，因为直链指向 CDN 而不是 KuGouMusicApi，
/// 既不需要带 cookie，超时策略也不一样（CDN 大文件要宽松得多）。
#[derive(Debug, Clone)]
pub struct Downloader {
    http: reqwest::Client,
    /// 正在流式下载的目标（临时文件路径）→ 已经交给调用方的缓冲。
    ///
    /// **同一个目标上起两条下载会互相破坏。** 后开的那条 `truncate(true)` 会把
    /// 前一条已经写进 `.part` 的字节从盘上抹掉，而缓冲只保留尾部窗口，窗口之外的
    /// 字节全靠 `pread` 这个文件读回来——抹掉之后读回的是空洞（听感是噪音）；
    /// 失败 / 取消时的 `remove_file` 也会把另一条的文件删掉，让它改名失败、白下
    /// 一整首。
    ///
    /// 触发它**不需要任何异常操作**：连按两次 Enter 就是两条 `start_streaming`
    /// 落在同一个目标上（`App::active_stream` 要等到攒够开头才被握住，中间那段
    /// 窗口里谁也拦不住第二条）。所以第二条直接复用第一条的缓冲，不再起任务。
    ///
    /// 登记在**起任务之前**、摘除在**回调之前**：晚了会漏掉重复，早了会留下
    /// 一条永远摘不掉的记录（那首歌从此再也起不了流）。
    streams: Arc<Mutex<HashMap<PathBuf, StreamingBuffer>>>,
}

impl Downloader {
    /// 当前登记在案的流式下载数（诊断用）。
    ///
    /// 正常应当恒为 0 或 1——登记在起任务之前、摘除在回调之前，一首歌只该有一条。
    /// 长时间听歌之后如果它跟着 RSS 一起涨，就说明有任务没走到摘除那一步，
    /// 「内存只涨不落」的答案就在这条路上；反之可以把这条路径整个排除。
    pub fn active_streams(&self) -> usize {
        self.streams
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    pub fn new(proxy: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            // 整首歌可能几十 MB，超时给足
            .timeout(std::time::Duration::from_secs(180))
            .connect_timeout(std::time::Duration::from_secs(8))
            .user_agent(concat!("kugou-tui/", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(2);

        if let Some(proxy_url) = proxy.map(str::trim).filter(|url| !url.is_empty()) {
            let parsed = reqwest::Proxy::all(proxy_url)
                .map_err(|error| AppError::Config(format!("代理地址 {proxy_url} 无效：{error}")))?;
            builder = builder.proxy(parsed);
        }

        let http = builder
            .build()
            .map_err(|error| AppError::Config(format!("构造下载客户端失败：{error}")))?;

        Ok(Self {
            http,
            streams: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 把 `url` 下载到 `target`。
    ///
    /// 先写同目录下的 `.part` 临时文件，全部写完再原子重命名。这样即使中途被杀
    /// 或断网，也不会留下一个「看起来完整、实际损坏」的缓存文件被后续播放命中。
    ///
    /// 返回写入的字节数。
    pub async fn fetch_to(
        &self,
        url: &str,
        target: &Path,
        progress: ProgressFn<'_>,
    ) -> Result<u64> {
        // 本地文件（汽水解密后的产物）：直接复制，不走网络。
        //
        // 为什么需要这条分支：汽水的音频是加密的，取链那层已经把
        // 「下载 + 解密 + 落盘」做完了，交回来的是一个 `file://` 路径
        // 而不是 HTTP 直链。这里把它复制进音频缓存，于是缓存查找
        // （`cache.find`）、回收、预取全都照常生效——不必给汽水
        // 单开一套缓存逻辑。
        if let Some(path) = local_file_path(url) {
            return copy_local(&path, target, progress).await;
        }

        // 先问一次 HEAD：拿到文件长度和「是否支持 Range」。
        // 支持并发就并发——一首 8 MB 的歌单连接爬要十几秒，分 4 块通常能砍到
        // 三分之一；不支持（或文件太小）就老老实实单连接，别为省几秒把
        // 兼容性搞坏。
        if let Some(total) = self.probe_parallel(url).await {
            let chunks = Self::split_into_chunks(total);
            if chunks.len() > 1 {
                match self
                    .fetch_parallel(url, target, total, chunks, progress)
                    .await
                {
                    Ok(bytes) => return Ok(bytes),
                    Err(error) => {
                        // 并发失败（比如 CDN 中途变卦）就退回单连接，
                        // 不要让用户因为一次优化尝试而播不了歌。
                        tlog!(
                            crate::logger::LEVEL_WARN,
                            "分块下载失败，退回单连接：{}",
                            error.user_hint()
                        );
                    }
                }
            }
        }

        self.fetch_sequential(url, target, progress).await
    }

    /// 问服务端「这个文件多大、支不支持分段下载」。两者都满足才返回长度。
    async fn probe_parallel(&self, url: &str) -> Option<u64> {
        let response = self.http.head(url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let accepts_ranges = response
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("bytes"));
        if !accepts_ranges {
            return None;
        }
        let total = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())?;
        (total >= PARALLEL_MIN_BYTES).then_some(total)
    }

    /// 把 `total` 字节切成若干块。块数受 [`PARALLEL_MAX_CHUNKS`] 和
    /// [`MIN_CHUNK_BYTES`] 双重约束，避免「4 块每块才 8 KB」这种极端。
    fn split_into_chunks(total: u64) -> Vec<ChunkRange> {
        let mut count = (total / MIN_CHUNK_BYTES).clamp(1, PARALLEL_MAX_CHUNKS);
        if count <= 1 {
            return vec![ChunkRange {
                start: 0,
                end: total.saturating_sub(1),
            }];
        }
        // 让每块不小于 MIN_CHUNK_BYTES，最后一块吃掉余数
        count = count.min(PARALLEL_MAX_CHUNKS);
        let size = total / count;
        (0..count)
            .map(|index| {
                let start = index * size;
                let end = if index == count - 1 {
                    total - 1
                } else {
                    start + size - 1
                };
                ChunkRange { start, end }
            })
            .collect()
    }

    /// 分块并发下载：每块一个 Range 请求，各自 seek 到自己的偏移写入同一个临时文件。
    async fn fetch_parallel(
        &self,
        url: &str,
        target: &Path,
        total: u64,
        chunks: Vec<ChunkRange>,
        progress: ProgressFn<'_>,
    ) -> Result<u64> {
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| AppError::io_at(parent.display().to_string(), error))?;
        }
        let temp_path = temp_path_for(target);
        // 预分配：让各块能并发 seek 到任意偏移而不互相踩踏
        let file = tokio::fs::File::create(&temp_path)
            .await
            .map_err(|error| AppError::io_at(temp_path.display().to_string(), error))?;
        file.set_len(total)
            .await
            .map_err(|error| AppError::io_at(temp_path.display().to_string(), error))?;
        drop(file);

        let received = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::with_capacity(chunks.len());
        for range in chunks {
            let url = url.to_string();
            let http = self.http.clone();
            let path = temp_path.clone();
            let received = received.clone();
            handles.push(tokio::spawn(async move {
                let mut file = tokio::fs::File::options()
                    .write(true)
                    .open(&path)
                    .await
                    .map_err(|error| AppError::io_at(path.display().to_string(), error))?;
                file.seek(SeekFrom::Start(range.start))
                    .await
                    .map_err(|error| AppError::io_at(path.display().to_string(), error))?;

                let mut response = http
                    .get(&url)
                    .header(
                        reqwest::header::RANGE,
                        format!("bytes={}-{}", range.start, range.end),
                    )
                    .send()
                    .await?;

                // 先验响应，再写文件。**顺序不能反**：写出去再检查的话，坏数据已经
                // 落进 `.part` 了，而别的块还在往同一个文件里写。
                let status = response.status();
                let content_range = response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                validate_range_response(range, status, content_range.as_deref(), total)?;

                let expected = range.len();
                let mut written = 0u64;
                while let Some(chunk) = response.chunk().await? {
                    file.write_all(&chunk)
                        .await
                        .map_err(|error| AppError::io_at(path.display().to_string(), error))?;
                    written += chunk.len() as u64;
                    received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                file.flush()
                    .await
                    .map_err(|error| AppError::io_at(path.display().to_string(), error))?;

                // 字节数对不上就是没下完（连接被掐、CDN 少给一段）。这时候
                // `.part` 里这一段是半截的，绝不能当成功。
                if written != expected {
                    return Err(AppError::Audio(format!(
                        "分块 {}-{} 只收到 {written}/{expected} 字节",
                        range.start, range.end
                    )));
                }
                Ok::<u64, AppError>(written)
            }));
        }

        // 一边等一边汇报进度：各块的字节数汇总到 `received`
        let mut done = false;
        while !done {
            done = true;
            for handle in handles.iter_mut() {
                if !handle.is_finished() {
                    done = false;
                    break;
                }
            }
            progress(received.load(Ordering::Relaxed), Some(total));
            if !done {
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            }
        }

        let mut total_written = 0u64;
        for handle in handles {
            match handle.await {
                Ok(Ok(bytes)) => total_written += bytes,
                Ok(Err(error)) => {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return Err(error);
                }
                Err(error) => {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return Err(AppError::Audio(format!("下载任务异常：{error}")));
                }
            }
        }

        // 每个块都已经自证「写满自己那一段」了，总和还必须等于总长度——否则说明
        // 分块切分或服务端的长度声明有问题。**不能只看「写进去的字节数大于 0」**：
        // `.part` 上面已经 `set_len(total)` 预分配过，文件大小本身就证明不了完整性，
        // 而半截文件被改名成正式缓存后，下次播放会在中间莫名结束。
        if total_written != total {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(AppError::Audio(format!(
                "分块下载不完整：期望 {total} 字节，实收 {total_written} 字节"
            )));
        }

        // 落盘之后再改名。少了这一步，改名后的文件在掉电/崩溃时可能还是空洞。
        let file = tokio::fs::File::options()
            .write(true)
            .open(&temp_path)
            .await
            .map_err(|error| AppError::io_at(temp_path.display().to_string(), error))?;
        file.sync_all()
            .await
            .map_err(|error| AppError::io_at(temp_path.display().to_string(), error))?;
        drop(file);

        tokio::fs::rename(&temp_path, target)
            .await
            .map_err(|error| AppError::io_at(target.display().to_string(), error))?;
        progress(total_written, Some(total));
        Ok(total_written)
    }

    /// 单连接顺序下载（原来的实现）。分块不可用时退回这条路径。
    async fn fetch_sequential(
        &self,
        url: &str,
        target: &Path,
        progress: ProgressFn<'_>,
    ) -> Result<u64> {
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| AppError::io_at(parent.display().to_string(), error))?;
        }

        let mut response = self.http.get(url).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::HttpStatus {
                path: url.to_string(),
                status: status.as_u16(),
            });
        }

        let total_bytes = response.content_length();
        let temp_path = temp_path_for(target);
        let mut file = tokio::fs::File::create(&temp_path)
            .await
            .map_err(|error| AppError::io_at(temp_path.display().to_string(), error))?;

        let mut written: u64 = 0;
        let result = async {
            while let Some(chunk) = response.chunk().await? {
                file.write_all(&chunk)
                    .await
                    .map_err(|error| AppError::io_at(temp_path.display().to_string(), error))?;
                written += chunk.len() as u64;
                progress(written, total_bytes);
            }
            file.flush()
                .await
                .map_err(|error| AppError::io_at(temp_path.display().to_string(), error))?;
            file.sync_all()
                .await
                .map_err(|error| AppError::io_at(temp_path.display().to_string(), error))?;
            Ok::<u64, AppError>(written)
        }
        .await;

        drop(file);

        match result {
            // 服务端给了 Content-Length 就必须一个字节不差地收满。少了就是断了，
            // 不能因为「收到的不是 0」就当成成功——半截文件进缓存比报错更坏。
            Ok(bytes) if bytes > 0 && total_bytes.is_none_or(|expected| expected == bytes) => {
                tokio::fs::rename(&temp_path, target)
                    .await
                    .map_err(|error| AppError::io_at(target.display().to_string(), error))?;
                Ok(bytes)
            }
            Ok(bytes) if bytes > 0 => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                Err(AppError::Audio(format!(
                    "下载不完整：服务端声明 {} 字节，实收 {bytes} 字节",
                    total_bytes.unwrap_or(0)
                )))
            }
            Ok(_) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                Err(AppError::Audio(format!(
                    "从 {url} 下载到的内容为空，可能该歌曲需要 VIP 或已下架"
                )))
            }
            Err(error) => {
                // 清理半成品，避免污染缓存
                let _ = tokio::fs::remove_file(&temp_path).await;
                Err(error)
            }
        }
    }

    /// 把一个小文件整个读进内存（封面图用，不落盘）。
    ///
    /// 封面每张几十 KB，没必要为它建一套磁盘缓存；而且它和音频缓存的回收策略
    /// 也不一样（按修改时间删旧的，会把正在看的封面删掉）。
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self.http.get(url).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::HttpStatus {
                path: url.to_string(),
                status: status.as_u16(),
            });
        }
        let bytes = response.bytes().await.map_err(AppError::Http)?;
        Ok(bytes.to_vec())
    }

    /// 根据直链推断音频容器扩展名。
    ///
    /// 酷狗的直链形如 `http://xxx/yyy.mp3?token=...`，扩展名在路径段里，
    /// 所以要先把 query 和 fragment 去掉再取后缀。
    pub fn extension_from_url(url: &str) -> &'static str {
        // 汽水的解密产物是 `file:///…/名字.m4a`：路径就是文件名的一部分，
        // 不能按 URL 那样先砍 query 再猜。直接交给路径解析。
        if let Some(path) = local_file_path(url) {
            return Self::extension_from_path(&path);
        }

        let without_fragment = url.split('#').next().unwrap_or(url);
        let without_query = without_fragment
            .split('?')
            .next()
            .unwrap_or(without_fragment);
        let extension = without_query
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();

        match extension.as_str() {
            "mp3" => "mp3",
            "flac" => "flac",
            "m4a" | "mp4" => "m4a",
            "aac" => "aac",
            "ogg" | "oga" => "ogg",
            "wav" => "wav",
            "ape" => "ape",
            // 推断不出来时按 mp3 存：rodio 走的是内容探测，扩展名只影响缓存查找
            _ => "mp3",
        }
    }

    /// 从本地路径推断缓存扩展名。
    fn extension_from_path(path: &Path) -> &'static str {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        match extension.as_str() {
            "mp3" => "mp3",
            "flac" => "flac",
            "m4a" | "mp4" => "m4a",
            "aac" => "aac",
            "ogg" | "oga" => "ogg",
            "wav" => "wav",
            "ape" => "ape",
            _ => "mp3",
        }
    }
}

/// `song.mp3` → `song.mp3.part`：整首下载用的临时文件。
fn temp_path_for(target: &Path) -> PathBuf {
    with_part_suffix(target, "")
}

/// 流式下载（边下边播）用的临时文件：`song.mp3.stream.part`。
///
/// **刻意与 [`temp_path_for`] 分开。** 同一个目标上完全可能同时有一条整首下载和
/// 一条流式下载：前者是「预取下一首」在后台跑，后者是用户恰好切到了那一首。
/// 两条共用一个 `.part` 的话，整首下载那次 `File::create` 会把流式那条已经
/// `push` 过的字节从盘上抹掉——而缓冲窗口之外的数据全靠 `pread` 这个文件读回来，
/// 读回的就是空洞（听感是噪音）。分开之后两条各写各的，最后各自改名，谁赢都对。
///
/// 后缀仍然以 `.part` 结尾：`cache::is_partial` 按扩展名判断半成品，
/// 缓存回收与「清空缓存」都靠它跳过正在写的文件。
fn stream_temp_path_for(target: &Path) -> PathBuf {
    with_part_suffix(target, "stream")
}

fn with_part_suffix(target: &Path, tag: &str) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    if !tag.is_empty() {
        name.push(".");
        name.push(tag);
    }
    name.push(".part");
    target.with_file_name(name)
}

// ============================================================================
// 流式下载（边下边播）
// ============================================================================

/// 开播前至少要攒够的字节数。
///
/// 太小（几 KB）：解码器刚探完格式就没数据，第一秒就卡住。
/// 太大：等待时间又退回"下完才播"。按常见码率（128kbps ≈ 16 KB/s）取
/// 128 KB ≈ 8 秒音频，够解码器稳定跑起来，等待又不明显。
pub const PREROLL_BYTES: u64 = 128 * 1024;

/// 一次流式下载的结局。
///
/// 三种情况在界面上的待遇完全不同，所以不能只用一个 `Result`：取消不是错误
/// （用户自己切了歌，不该弹红字），失败要如实说，成功只做收尾（**不能**顺手
/// 把音源换成本地文件——那会把正在放的位置冲回 0）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOutcome {
    /// 整首下完，并已改名为正式缓存文件。
    Completed,
    /// 下载失败，原因见内层字符串。
    Failed(String),
    /// 用户切歌 / 停止，主动取消；中途文件已清理，不算错误。
    Cancelled,
}

impl Downloader {
    /// 开始流式下载，**立即返回**缓冲。
    ///
    /// 后台任务一边灌 buffer 一边写 `cache_path` 对应的 `.part` 文件：这次播完
    /// 缓存就在了，下次直接走本地文件，不用再下。
    ///
    /// 返回的 buffer 可直接交给 `AudioEngine::load`——读指针跑到还没下载到的
    /// 位置时会在 `read()` 里阻塞等数据，表现是声音停一下，而不是提前结束。
    ///
    /// 缓冲只在内存里留一个窗口，其余落在 `.part` 上（见
    /// [`crate::audio::streaming`]），所以这里必须**先**把文件建好并把句柄交给它。
    pub fn start_streaming(
        &self,
        url: &str,
        cache_path: PathBuf,
        on_done: impl FnOnce(StreamOutcome) + Send + 'static,
    ) -> Result<StreamingBuffer> {
        let part_path = stream_temp_path_for(&cache_path);

        // 整个「查重 → 建文件 → 登记」在一把锁里做完：`start_streaming` 是在
        // `runtime.spawn` 出来的任务里被调的，两条任务可以真的并发到这里。
        let mut streams = self
            .streams
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        // 同一个目标已经有流在跑：把它的缓冲交回去，不再起第二条。调用方拿到的是
        // 同一份数据，预攒够开头的判断、播放、收尾全都照旧。
        if let Some(existing) = streams.get(&part_path) {
            return Ok(existing.clone());
        }

        if let Some(parent) = part_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AppError::io_at(parent.display().to_string(), error))?;
        }
        // 必须是**读写**打开的：缓冲要靠这个句柄 `pread` 把回收掉的字节读回来，
        // 而 `File::create` 只给 `O_WRONLY`（对这种句柄 `pread` 直接 EBADF）。
        // `truncate` 顺手把上一次留下的同名 `.part` 清掉重用。
        let file = Arc::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&part_path)
                .map_err(|error| AppError::io_at(part_path.display().to_string(), error))?,
        );

        let buffer = StreamingBuffer::with_spill(None, Arc::clone(&file));
        // 登记必须早于起任务：任务可能瞬间就跑完并去摘登记，那时它还没被登记，
        // 摘除是个空操作，这条记录就永远留着了。
        streams.insert(part_path.clone(), buffer.clone());
        drop(streams);

        let writer = buffer.clone();
        let http = self.http.clone();
        let url = url.to_string();
        let streams = Arc::clone(&self.streams);

        tokio::spawn(async move {
            let outcome = match stream_into(&http, &url, &writer, &file).await {
                Ok(()) if writer.is_cancelled() => {
                    // 用户在下载中途切了歌：删掉半成品，不留痕、不报错
                    let _ = tokio::fs::remove_file(&part_path).await;
                    StreamOutcome::Cancelled
                }
                Ok(()) => {
                    writer.finish(None);
                    match tokio::fs::rename(&part_path, &cache_path).await {
                        Ok(()) => StreamOutcome::Completed,
                        Err(error) => {
                            // 数据是完整的，能照常播完；只是这次没进缓存，
                            // 下次播放要重下一次。不值得惊动用户。
                            tlog!(
                                crate::logger::LEVEL_WARN,
                                "音频改名到 {} 失败：{error}",
                                cache_path.display()
                            );
                            StreamOutcome::Completed
                        }
                    }
                }
                Err(error) => {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    if writer.is_cancelled() {
                        // 取消的收尾在 `cancel()` 里已经做完了（标志位 + 唤醒
                        // 阻塞的读线程），别再记一条假错误
                        StreamOutcome::Cancelled
                    } else {
                        let message = error.to_string();
                        tlog!(crate::logger::LEVEL_WARN, "流式下载 {url} 失败：{message}");
                        // 先把失败写进缓冲再回调：正阻塞在 read() 的解码线程要能
                        // 立刻醒过来（并知道是为什么），不能干等到超时。
                        writer.finish(Some(message.clone()));
                        StreamOutcome::Failed(message)
                    }
                }
            };
            // 摘登记要早于回调：回调那一侧（`App::start_download`）可能立刻再起
            // 一条同目标的流——比如用户切走又切回来。摘晚了它会拿到一条已经收工的
            // 缓冲，画面卡在「缓冲中」。
            streams
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&part_path);
            on_done(outcome);
        });

        Ok(buffer)
    }
}

async fn stream_into(
    http: &reqwest::Client,
    url: &str,
    buffer: &StreamingBuffer,
    file: &std::fs::File,
) -> Result<()> {
    let mut response = http
        .get(url)
        .send()
        .await?
        .error_for_status()
        .map_err(|error| AppError::HttpStatus {
            path: url.to_string(),
            status: error.status().map(|status| status.as_u16()).unwrap_or(0),
        })?;

    // 用 reqwest 自带的 chunk()，不引 futures_util——为一个循环加依赖不值当
    // `&File` 也实现了 Write，写的是同一个 fd（不共享偏移，pread 不受影响）
    let mut sink = file;
    while let Some(chunk) = response.chunk().await? {
        if buffer.is_cancelled() {
            return Ok(());
        }
        // **顺序不能反**：先落盘再进内存。缓冲只保留尾部窗口，窗口之外的字节
        // 靠 `pread(this file)` 读回来——先 push 后写文件的话，读指针跑到
        // 窗口外面时会从文件里读到还没写入的空洞（听感是噪音）。
        sink.write_all(&chunk)
            .map_err(|error| AppError::io_at(url.to_string(), error))?;
        buffer.push(&chunk);
    }

    sink.flush()
        .map_err(|error| AppError::io_at(url.to_string(), error))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_extension_ignoring_query_string() {
        assert_eq!(
            Downloader::extension_from_url("http://cdn.kugou.com/a/b/abc.mp3?token=xyz&x=1"),
            "mp3"
        );
        assert_eq!(Downloader::extension_from_url("https://x/y.flac"), "flac");
        assert_eq!(
            Downloader::extension_from_url("https://x/y.m4a#frag"),
            "m4a"
        );
    }

    #[test]
    fn falls_back_to_mp3_for_unknown_extension() {
        assert_eq!(Downloader::extension_from_url("http://x/stream"), "mp3");
        assert_eq!(Downloader::extension_from_url("http://x/y.weird"), "mp3");
    }

    /// `.part` 必须落在**目标文件的同一个目录**里。
    ///
    /// 期望值用 `join` 拼、不写成字面量 `/tmp/cache/abc-128.mp3.part`：
    /// `with_file_name` 内部是 `parent().join(...)`，而 Windows 上 `join` 用 `\`
    /// 拼接，拿字面量比对会在那边假失败。
    #[test]
    fn builds_part_file_beside_target() {
        let target = Path::new("/tmp/cache/abc-128.mp3");
        let path = temp_path_for(target);
        assert_eq!(path, Path::new("/tmp/cache").join("abc-128.mp3.part"));
        assert_eq!(
            path.parent(),
            target.parent(),
            "`.part` 必须和正式缓存文件同目录，否则改名会跨文件系统"
        );
    }

    /// 同一个目标上不能有两条流。
    ///
    /// **回归测试**：修复前连按两次 Enter（或一次切歌再切回来）就会起两条
    /// `start_streaming`，两条都 `truncate` 同一个 `.part`，把对方已经 `push`
    /// 过的字节从盘上抹掉——缓冲窗口之外的数据正是从这个文件 `pread` 回来的，
    /// 读回空洞就是噪音。现在第二条直接复用第一条的缓冲，不再起任务。
    ///
    /// 用「没人监听的端口」做 URL 是为了**确定性**：`#[tokio::test]` 是单线程
    /// 运行时，spawn 出去的任务要等到下一次 `await` 才会被调度，所以这两次调用
    /// 之间不存在竞态，第一条的登记一定还在。
    #[tokio::test]
    async fn a_second_stream_for_the_same_target_reuses_the_first_buffer() {
        let dir = temp_dir("stream-dedupe");
        let target = dir.join("song.mp3");
        let downloader = Downloader::new(None).expect("构造下载器");

        let url = "http://127.0.0.1:1/song.mp3";
        let first = downloader
            .start_streaming(url, target.clone(), |_| {})
            .expect("第一条应当能建起来");
        let second = downloader
            .start_streaming(url, target.clone(), |_| {})
            .expect("第二条也应当成功——它复用第一条的缓冲");

        // 两个句柄指向**同一份**缓冲：取消一个，另一个立刻看得到。
        first.cancel();
        assert!(
            second.is_cancelled(),
            "第二条拿到的应当是第一份缓冲，而不是新起的一条流"
        );
        assert!(second.is_finished(), "取消也算收工");
    }

    /// 整首下载与流式下载不能共用一个临时文件。
    ///
    /// 「预取下一首」在后台整首下载时，用户完全可能正好切到那一首——两条下载
    /// 落在同一个目标上。共用一个 `.part` 的话，整首下载那次 `File::create`
    /// 会把流式那条已经落盘的字节截掉，而缓冲窗口之外的数据全靠从这个文件
    /// `pread` 读回来。分开之后各写各的，最后各自改名，谁赢都对。
    #[test]
    fn stream_and_whole_file_downloads_never_share_a_part_file() {
        let target = Path::new("/tmp/cache/abc-128.mp3");
        let whole = temp_path_for(target);
        let stream = stream_temp_path_for(target);

        assert_ne!(whole, stream, "两条下载不能共用一个临时文件");
        for path in [&whole, &stream] {
            assert_eq!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("part"),
                "临时文件必须以 .part 结尾，否则缓存回收会把它当正式缓存删掉：{path:?}"
            );
            assert_eq!(path.parent(), target.parent(), "改名不能跨文件系统");
        }
    }

    /// 小文件不分块——一次分块要建一次连接，小文件并发反而是负优化。
    #[test]
    fn small_file_is_never_split() {
        let chunks = Downloader::split_into_chunks(100 * 1024);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start, 0);
        assert_eq!(chunks[0].end, 100 * 1024 - 1);
    }

    /// 大文件分块：各块首尾相接、整首歌一个字节都不能漏或多。
    #[test]
    fn chunks_cover_the_file_exactly_once() {
        let total = 8 * 1024 * 1024;
        let chunks = Downloader::split_into_chunks(total);
        assert!(chunks.len() > 1, "8 MB 应当分块");
        assert!(chunks.len() <= PARALLEL_MAX_CHUNKS as usize);

        // 首尾相接
        assert_eq!(chunks[0].start, 0);
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].end + 1, pair[1].start, "块之间不能有缝");
        }
        // 最后一块正好到文件末尾
        assert_eq!(chunks.last().unwrap().end, total - 1);
        // 每块都不小于下限
        for range in &chunks {
            assert!(
                range.end - range.start + 1 >= MIN_CHUNK_BYTES,
                "块太小：{range:?}"
            );
        }
    }

    // ========================================================================
    // 分块下载的回归测试
    //
    // 这一组测的是「服务端不守规矩时会不会写出坏缓存」——纯函数测试覆盖不到，
    // 必须有一个真的 HTTP 服务端。刻意**不引 wiremock / httpmock**：为一个测试
    // 拖一整棵依赖树不划算，而这里要模拟的恰恰是「服务端不按协议来」，手写反而
    // 更直接。用 std 的 TcpListener + 一个线程，不碰 tokio 的 net feature。
    // ========================================================================

    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;

    /// 服务端收到的请求，只解析测试用得到的字段。
    #[derive(Debug, Clone)]
    struct TestRequest {
        method: String,
        /// `Range: bytes=start-end` 解析出来的闭区间。
        range: Option<(u64, u64)>,
    }

    /// 服务端要回的响应。
    #[derive(Debug, Clone)]
    struct TestResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        /// 覆盖 `Content-Length`。HEAD 的响应体是空的，但必须声明真实长度，
        /// 否则探活那一步拿不到文件大小、根本不会走分块。
        content_length: Option<usize>,
        /// 只写前 N 字节就断开，模拟传输中断。
        truncate_at: Option<usize>,
    }

    impl TestResponse {
        fn ok(body: Vec<u8>) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body,
                content_length: None,
                truncate_at: None,
            }
        }

        fn partial(body: Vec<u8>, content_range: String) -> Self {
            Self {
                status: 206,
                headers: vec![("Content-Range".to_string(), content_range)],
                body,
                content_length: None,
                truncate_at: None,
            }
        }

        fn head(total: usize) -> Self {
            Self {
                status: 200,
                headers: vec![("Accept-Ranges".to_string(), "bytes".to_string())],
                body: Vec::new(),
                content_length: Some(total),
                truncate_at: None,
            }
        }

        fn truncated(mut self, at: usize) -> Self {
            self.truncate_at = Some(at);
            self
        }
    }

    /// 起一个只服务本用例的 HTTP 服务端，返回直链与「收到过几个带 Range 的请求」。
    ///
    /// 计数器用来确认「确实走了分块」——只看最终文件对不对是不够的，退回单连接
    /// 时结果一样正确，那样就测不到分块这条路径了。
    fn spawn_server(
        handler: Box<dyn Fn(&TestRequest) -> TestResponse + Send + Sync + 'static>,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");
        let range_hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&range_hits);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let Some(request) = read_request(&stream) else {
                    continue;
                };
                if request.range.is_some() {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                let response = handler(&request);
                let _ = write_response(stream, &response);
            }
        });

        (format!("http://{addr}/song.mp3"), range_hits)
    }

    fn read_request(stream: &TcpStream) -> Option<TestRequest> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let method = line.split_whitespace().next()?.to_string();

        let mut range = None;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).ok()? == 0 || header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("range")
            {
                range = parse_request_range(value);
            }
        }
        Some(TestRequest { method, range })
    }

    fn parse_request_range(value: &str) -> Option<(u64, u64)> {
        let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
        Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
    }

    fn write_response(mut stream: TcpStream, response: &TestResponse) -> std::io::Result<()> {
        let length = response.content_length.unwrap_or(response.body.len());
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {length}\r\nConnection: close\r\n",
            response.status,
            match response.status {
                200 => "OK",
                206 => "Partial Content",
                _ => "Error",
            }
        );
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;

        let body = match response.truncate_at {
            Some(at) => &response.body[..at.min(response.body.len())],
            None => &response.body[..],
        };
        stream.write_all(body)?;
        stream.flush()
    }

    /// 造一份可校验的假音频：每个字节由下标决定，任何错位都能被断言抓到。
    fn payload(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kugou-tui-dl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 1 MiB：够触发分块（阈值 512 KiB），切成 4 块。
    const TEST_TOTAL: usize = 1024 * 1024;

    /// 正常情况：服务端老老实实按 Range 回 206，文件必须逐字节正确。
    #[tokio::test]
    async fn parallel_download_assembles_exact_bytes() {
        let data = Arc::new(payload(TEST_TOTAL, 7));
        let body = Arc::clone(&data);
        let (url, range_hits) = spawn_server(Box::new(move |request| {
            if request.method == "HEAD" {
                return TestResponse::head(TEST_TOTAL);
            }
            match request.range {
                Some((start, end)) => TestResponse::partial(
                    body[start as usize..=end as usize].to_vec(),
                    format!("bytes {start}-{end}/{TEST_TOTAL}"),
                ),
                None => TestResponse::ok((*body).clone()),
            }
        }));

        let dir = temp_dir("parallel-ok");
        let target = dir.join("song.mp3");
        let downloader = Downloader::new(None).expect("构造下载器");
        let bytes = downloader
            .fetch_to(&url, &target, &|_, _| {})
            .await
            .expect("应当下载成功");

        assert_eq!(bytes as usize, TEST_TOTAL);
        assert_eq!(std::fs::read(&target).expect("读回文件"), *data);
        assert!(
            range_hits.load(Ordering::Relaxed) > 1,
            "应当走分块并发，实际只收到 {} 个 Range 请求",
            range_hits.load(Ordering::Relaxed)
        );
    }

    /// **回归测试**：服务端忽略 `Range`，对每个分块请求都回 `200 OK` + 整首内容。
    ///
    /// 修复前这里会「成功」：四个任务各自 `seek(自己的起点)` 再写整首，互相覆盖，
    /// 得到一个内容全错的文件，还照常改名进缓存。修复后必须识别出响应不合规、
    /// 退回单连接，最终文件逐字节正确。
    #[tokio::test]
    async fn server_ignoring_range_falls_back_instead_of_corrupting() {
        let data = Arc::new(payload(TEST_TOTAL, 11));
        let body = Arc::clone(&data);
        let (url, _hits) = spawn_server(Box::new(move |request| {
            if request.method == "HEAD" {
                return TestResponse::head(TEST_TOTAL);
            }
            TestResponse::ok((*body).clone())
        }));

        let dir = temp_dir("ignore-range");
        let target = dir.join("song.mp3");
        let downloader = Downloader::new(None).expect("构造下载器");
        let bytes = downloader
            .fetch_to(&url, &target, &|_, _| {})
            .await
            .expect("应当退回单连接后成功");

        assert_eq!(bytes as usize, TEST_TOTAL);
        assert_eq!(
            std::fs::read(&target).expect("读回文件"),
            *data,
            "内容必须逐字节正确"
        );
        assert_eq!(
            std::fs::metadata(&target).expect("stat").len() as usize,
            TEST_TOTAL,
            "文件长度不能超过声明长度——覆盖写会把它撑大"
        );
    }

    /// 服务端回 206，但 `Content-Range` 与请求的范围错位一格。
    ///
    /// 这种响应「看起来是对的」：状态码对、长度也对，只有范围错了。修复前会直接
    /// 写进 `.part`；修复后判为不可信，退回单连接重下，结果仍然正确。
    #[tokio::test]
    async fn mismatched_content_range_is_rejected() {
        let data = Arc::new(payload(TEST_TOTAL, 3));
        let body = Arc::clone(&data);
        let (url, range_hits) = spawn_server(Box::new(move |request| {
            if request.method == "HEAD" {
                return TestResponse::head(TEST_TOTAL);
            }
            match request.range {
                Some((start, end)) => TestResponse::partial(
                    body[start as usize..=end as usize].to_vec(),
                    // 故意错位一格
                    format!("bytes {}-{}/{TEST_TOTAL}", start + 1, end + 1),
                ),
                None => TestResponse::ok((*body).clone()),
            }
        }));

        let dir = temp_dir("bad-range");
        let target = dir.join("song.mp3");
        let downloader = Downloader::new(None).expect("构造下载器");
        downloader
            .fetch_to(&url, &target, &|_, _| {})
            .await
            .expect("应当退回单连接后成功");

        assert!(
            range_hits.load(Ordering::Relaxed) > 0,
            "这个用例的前提是先试过分块"
        );
        assert_eq!(std::fs::read(&target).expect("读回文件"), *data);
    }

    /// 传一半就断：分块与单连接两条路径都不能把半截数据当成成功，
    /// 也不能在磁盘上留下 `.part` 或目标文件。
    #[tokio::test]
    async fn truncated_transfer_leaves_nothing_behind() {
        let data = Arc::new(payload(TEST_TOTAL, 5));
        let body = Arc::clone(&data);
        let (url, _hits) = spawn_server(Box::new(move |request| {
            if request.method == "HEAD" {
                return TestResponse::head(TEST_TOTAL);
            }
            match request.range {
                Some((start, end)) => {
                    let slice = body[start as usize..=end as usize].to_vec();
                    let half = slice.len() / 2;
                    TestResponse::partial(slice, format!("bytes {start}-{end}/{TEST_TOTAL}"))
                        .truncated(half)
                }
                None => TestResponse::ok((*body).clone()).truncated(TEST_TOTAL / 2),
            }
        }));

        let dir = temp_dir("truncated");
        let target = dir.join("song.mp3");
        let downloader = Downloader::new(None).expect("构造下载器");
        let result = downloader.fetch_to(&url, &target, &|_, _| {}).await;

        assert!(result.is_err(), "半截数据不能算成功：{result:?}");
        assert!(!target.exists(), "失败时不能留下目标文件");
        assert!(
            !temp_path_for(&target).exists(),
            "失败时不能留下 .part 半成品"
        );
    }

    #[test]
    fn parses_content_range_in_the_only_form_we_trust() {
        assert_eq!(
            parse_content_range("bytes 0-99/1000"),
            Some((0, 99, Some(1000)))
        );
        assert_eq!(
            parse_content_range("bytes 500-999/*"),
            Some((500, 999, None))
        );
        assert_eq!(
            parse_content_range("bytes  10-20 / 30 "),
            Some((10, 20, Some(30)))
        );
        // 以下都判为不可信
        assert_eq!(
            parse_content_range("items 0-99/1000"),
            None,
            "单位不是 bytes"
        );
        assert_eq!(parse_content_range("bytes 99-0/1000"), None, "范围倒挂");
        assert_eq!(parse_content_range("bytes 0-99"), None, "没有总长度段");
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn rejects_range_responses_that_are_not_the_requested_slice() {
        use reqwest::StatusCode;
        let range = ChunkRange {
            start: 100,
            end: 199,
        };
        let ok = |status, header| validate_range_response(range, status, header, 1000);

        assert!(ok(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/1000")).is_ok());
        // 总长度写 `*`：允许
        assert!(ok(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/*")).is_ok());

        assert!(
            ok(StatusCode::OK, Some("bytes 100-199/1000")).is_err(),
            "200 说明忽略了 Range"
        );
        assert!(
            ok(StatusCode::PARTIAL_CONTENT, None).is_err(),
            "缺 Content-Range"
        );
        assert!(
            ok(StatusCode::PARTIAL_CONTENT, Some("bytes 101-200/1000")).is_err(),
            "范围错位"
        );
        assert!(
            ok(StatusCode::PARTIAL_CONTENT, Some("bytes 100-199/2000")).is_err(),
            "总长度与 HEAD 不一致"
        );
    }

    #[test]
    fn chunk_len_counts_the_closed_interval() {
        assert_eq!(ChunkRange { start: 0, end: 0 }.len(), 1);
        assert_eq!(ChunkRange { start: 10, end: 19 }.len(), 10);
    }
}
