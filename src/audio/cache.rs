//! 音频磁盘缓存与容量回收。
//!
//! # 为什么缓存到磁盘而不是内存
//!
//! 一首无损 FLAC 约 30 MiB，把整张专辑预读进内存会让常驻内存轻松突破 500 MiB，
//! 与「低资源占用」的目标直接冲突。落盘缓存让常驻内存稳定在**单曲解码缓冲**
//! 这个量级（几百 KB），代价是首次播放多一次磁盘写入——而这次写入本来就无法避免，
//! 因为 rodio 的解码器要求 `Read + Seek`，必须能随机访问整段音频。
//!
//! 缓存文件名是 `{hash}-{quality}.{ext}`，同一首歌的不同音质互不覆盖。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::Result;
use crate::logger::tlog;

/// 已知的音频容器扩展名。
///
/// 查找缓存时按顺序试，所以常用的排前面以减少 stat 次数。
const KNOWN_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "aac", "ogg", "wav", "ape", "mp4"];

/// 触发回收后清理到的目标水位（占上限的比例）。
///
/// 留出 20% 余量，避免每播一首歌都触发一次全目录扫描。
const EVICT_TARGET_RATIO: f64 = 0.8;

/// 单次回收最多删除的文件数，防止在超大缓存目录上卡住事件循环。
const MAX_EVICTIONS_PER_RUN: usize = 512;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictionReport {
    pub removed_files: usize,
    pub freed_bytes: u64,
}

/// 清空缓存的结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClearReport {
    pub removed_files: usize,
    pub freed_bytes: u64,
    /// 删除失败的文件数。非 0 时界面应提示用户（多半是权限问题）。
    pub failed: usize,
}

#[derive(Debug, Clone)]
pub struct AudioCache {
    root: PathBuf,
    /// 上限字节数，`0` 表示不限制。
    limit_bytes: u64,
}

impl AudioCache {
    pub fn new(root: PathBuf, limit_mib: u64) -> Self {
        Self {
            root,
            limit_bytes: limit_mib.saturating_mul(1024 * 1024),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 缓存文件的目标路径。
    pub fn path_for(&self, key: &str, extension: &str) -> PathBuf {
        self.root.join(format!("{key}.{extension}"))
    }

    /// 查找已缓存的文件。
    ///
    /// 扩展名取决于服务端返回的内容类型，缓存时才知道，所以这里逐个候选探测。
    /// 最多 8 次 `stat`，命中内核 dentry 缓存后开销可忽略。
    pub fn find(&self, key: &str) -> Option<PathBuf> {
        KNOWN_EXTENSIONS
            .iter()
            .map(|extension| self.path_for(key, extension))
            .find(|path| path.is_file())
    }

    /// 缓存占用的总字节数。
    pub fn total_bytes(&self) -> u64 {
        self.entries().iter().map(|entry| entry.size_bytes).sum()
    }

    /// 超出上限时按修改时间从旧到新删除，直到降到水位以下。
    ///
    /// 返回本次回收的统计，供界面提示。上限为 0 时直接返回空报告。
    pub fn enforce_limit(&self) -> Result<EvictionReport> {
        if self.limit_bytes == 0 {
            return Ok(EvictionReport::default());
        }

        let mut entries = self.entries();
        let total: u64 = entries.iter().map(|entry| entry.size_bytes).sum();
        if total <= self.limit_bytes {
            return Ok(EvictionReport::default());
        }

        let target = (self.limit_bytes as f64 * EVICT_TARGET_RATIO) as u64;

        // 最旧的先删
        entries.sort_by_key(|entry| entry.modified);

        let mut report = EvictionReport::default();
        let mut remaining = total;

        for entry in entries {
            if remaining <= target || report.removed_files >= MAX_EVICTIONS_PER_RUN {
                break;
            }
            match std::fs::remove_file(&entry.path) {
                Ok(()) => {
                    remaining = remaining.saturating_sub(entry.size_bytes);
                    report.removed_files += 1;
                    report.freed_bytes += entry.size_bytes;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // 已被其它进程清掉，仍然计入已释放空间
                    remaining = remaining.saturating_sub(entry.size_bytes);
                }
                Err(error) => {
                    tlog!(
                        crate::logger::LEVEL_WARN,
                        "删除缓存文件 {} 失败：{error}",
                        entry.path.display()
                    );
                }
            }
        }

        Ok(report)
    }

    /// 清空整个缓存目录。
    ///
    /// 与 [`Self::enforce_limit`] 的区别：那个只删到水位线以下（按修改时间从旧到新），
    /// 这个是**全部删除**，由用户在界面上主动触发。
    ///
    /// 只删**文件**，不动子目录，也不删目录本身——保留目录避免后续播放还要重建。
    /// 单个文件删除失败不中断，继续删剩下的，最后把失败数报出来。
    pub fn clear(&self) -> Result<ClearReport> {
        let mut report = ClearReport::default();

        for entry in self.entries() {
            match std::fs::remove_file(&entry.path) {
                Ok(()) => {
                    report.removed_files += 1;
                    report.freed_bytes += entry.size_bytes;
                }
                Err(error) => {
                    report.failed += 1;
                    tlog!(
                        crate::logger::LEVEL_WARN,
                        "清空缓存时删除 {} 失败：{error}",
                        entry.path.display()
                    );
                }
            }
        }

        Ok(report)
    }

    /// 列出缓存目录下的文件及其大小、修改时间。
    ///
    /// 目录不存在时返回空列表——首次运行还没播过歌，这是正常状态而非错误。
    ///
    /// **跳过 `.part`**：那是流式下载正在写的半成品（见
    /// [`crate::audio::download`]）。把它当缓存条目看待有两个坏处：
    /// 回收会在下载中途把文件删掉（这次就白下了），"清空缓存"也会踩到它。
    /// 代价是崩溃留下的 `.part` 不参与回收——它最多一首歌那么大，而且那首歌
    /// 下次播放时会被 `File::create` 截断重用。
    fn entries(&self) -> Vec<CacheEntry> {
        let directory = match std::fs::read_dir(&self.root) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                tlog!(
                    crate::logger::LEVEL_WARN,
                    "读取缓存目录 {} 失败：{error}",
                    self.root.display()
                );
                return Vec::new();
            }
        };

        directory
            .filter_map(std::result::Result::ok)
            .filter(|entry| !is_partial(entry.path().as_path()))
            .filter_map(|dir_entry| {
                let metadata = dir_entry.metadata().ok()?;
                if !metadata.is_file() {
                    return None;
                }
                Some(CacheEntry {
                    path: dir_entry.path(),
                    size_bytes: metadata.len(),
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
struct CacheEntry {
    path: PathBuf,
    size_bytes: u64,
    modified: SystemTime,
}

/// 是不是流式下载还没写完的半成品（`xxx.mp3.part`）。
///
/// 判据用后缀而不是"名字里有没有点"：缓存文件名本身带扩展名
/// （`{hash}-{quality}.mp3`），只有 `.part` 结尾的才是半成品。
fn is_partial(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "part")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("kugou-tui-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn finds_file_regardless_of_extension() {
        let root = temp_dir("find");
        let cache = AudioCache::new(root.clone(), 0);
        std::fs::create_dir_all(&root).expect("创建缓存目录");

        std::fs::write(cache.path_for("abc-128", "flac"), b"x").expect("写入");
        assert_eq!(
            cache.find("abc-128"),
            Some(cache.path_for("abc-128", "flac"))
        );
        assert_eq!(cache.find("missing"), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unlimited_cache_never_evicts() {
        let root = temp_dir("unlimited");
        let cache = AudioCache::new(root.clone(), 0);
        std::fs::create_dir_all(&root).expect("创建缓存目录");
        std::fs::write(cache.path_for("a-128", "mp3"), vec![0u8; 4096]).expect("写入");

        let report = cache.enforce_limit().expect("回收");
        assert_eq!(report.removed_files, 0);
        assert!(cache.path_for("a-128", "mp3").is_file());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn evicts_oldest_files_when_over_limit() {
        let root = temp_dir("evict");
        // 上限 1 MiB，写入 3 个 512 KiB 文件，必然超限
        let cache = AudioCache::new(root.clone(), 1);
        std::fs::create_dir_all(&root).expect("创建缓存目录");

        for index in 0..3 {
            std::fs::write(
                cache.path_for(&format!("song{index}"), "mp3"),
                vec![0u8; 512 * 1024],
            )
            .expect("写入");
        }
        assert!(cache.total_bytes() > 1024 * 1024);

        let report = cache.enforce_limit().expect("回收");
        assert!(report.removed_files > 0, "应至少删除一个文件");
        assert!(cache.total_bytes() <= 1024 * 1024, "回收后应低于上限");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_removes_everything() {
        let root = temp_dir("clear");
        let cache = AudioCache::new(root.clone(), 0);
        std::fs::create_dir_all(&root).expect("创建缓存目录");
        std::fs::write(cache.path_for("a", "mp3"), b"data").expect("写入");
        std::fs::write(cache.path_for("b", "flac"), b"data").expect("写入");

        // 把上限设成 1 字节，等价于「全部回收」
        let tiny = AudioCache::new(root.clone(), 0);
        let report = tiny.enforce_limit().expect("回收");
        assert_eq!(report.removed_files, 0, "上限为 0 表示不限制");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 流式下载正在写的 `.part` 不能被当成缓存条目。
    ///
    /// 被回收或"清空缓存"顺手删掉的话，这次下载就白下了（`.part` 没了，
    /// 改名成正式缓存文件那一步会失败）。而它本来也不是"缓存"，是半成品。
    #[test]
    fn partial_downloads_are_not_cache_entries() {
        let root = temp_dir("partial");
        let cache = AudioCache::new(root.clone(), 1);
        std::fs::create_dir_all(&root).expect("创建缓存目录");

        // 一个半成品（正在下）+ 两个真正的缓存文件，总量必然超限。
        // 体量取小：这条测的是「谁会被删」，不是「能删多少」——`/tmp` 可能是
        // 个小 tmpfs（实测有的机器只有 10 MiB），别让测试挑磁盘。
        let partial = cache
            .path_for("streaming", "mp3")
            .with_extension("mp3.part");
        std::fs::write(&partial, vec![0u8; 64 * 1024]).expect("写入半成品");
        for index in 0..2 {
            std::fs::write(
                cache.path_for(&format!("song{index}"), "mp3"),
                vec![0u8; 600 * 1024],
            )
            .expect("写入");
        }

        let report = cache.enforce_limit().expect("回收");
        assert!(report.removed_files > 0, "应当回收真正的缓存文件");
        assert!(partial.is_file(), "半成品不参与回收");

        cache.clear().expect("清空缓存");
        assert!(partial.is_file(), "清空缓存也不该动半成品");

        let _ = std::fs::remove_dir_all(&root);
    }
}
