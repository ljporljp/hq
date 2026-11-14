//! 本地磁盘缓存。
//!
//! 接口返回的原始文本按 `key` 各存一个文件，用途有两个：
//!
//! 1. 启动时先读缓存，界面不用等首次请求就能画出上一份数据；
//! 2. 请求失败（断网、对端异常、休市返回空）时回退到上一次成功的响应。
//!
//! 不做过期淘汰：每类数据只有一份文件，成功取数时覆盖。时效性由调用方
//! （[`crate::model`]）的 TTL 决定，缓存本身只负责"存得住"。

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// 缓存目录：`%LOCALAPPDATA%\hq\cache`，环境变量缺失时退到临时目录。
fn cache_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|dir| dir.join("hq").join("cache"))
        .unwrap_or_else(|| std::env::temp_dir().join("hq-cache"))
}

#[derive(Clone, Debug)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new() -> Self {
        Self { dir: cache_dir() }
    }

    /// 指定目录，主要给测试用。
    #[cfg(test)]
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    #[cfg(test)]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.txt"))
    }

    /// 读取某份缓存及其写入时间；文件不存在、为空、读不出来都返回 `None`。
    pub fn read(&self, key: &str) -> Option<(String, SystemTime)> {
        let path = self.path(key);
        let at = fs::metadata(&path).and_then(|meta| meta.modified()).ok()?;
        let text = fs::read_to_string(&path).ok()?;
        (!text.trim().is_empty()).then_some((text, at))
    }

    /// 只接受 `max_age` 以内的缓存；过期、损坏、不存在都返回 `None`。
    ///
    /// 行情不同于配置：昨天的分时曲线、一周前的报价原样拿出来当"最新"用会误导，
    /// 所以每类数据各自给一个可回退的时限。
    pub fn read_fresh(&self, key: &str, max_age: Duration) -> Option<String> {
        let (text, at) = self.read(key)?;
        let age = SystemTime::now().duration_since(at).unwrap_or_default();
        (age <= max_age).then_some(text)
    }

    /// 写入缓存。失败（无权限、磁盘满）直接忽略——缓存不该影响主流程。
    pub fn write(&self, key: &str, text: &str) {
        let _ = self.write_inner(key, text);
    }

    fn write_inner(&self, key: &str, text: &str) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(key);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text)?;
        // Windows 上 rename 不能覆盖已存在的目标文件，先删旧的。
        let _ = fs::remove_file(&path);
        fs::rename(&tmp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache(name: &str) -> Cache {
        let dir = std::env::temp_dir().join(format!("hq-cache-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Cache::at(dir)
    }

    #[test]
    fn round_trips_text_and_timestamp() {
        let cache = temp_cache("roundtrip");
        assert!(cache.read("quotes").is_none());
        cache.write("quotes", "hello");
        let (text, _at) = cache.read("quotes").expect("cached");
        assert_eq!(text, "hello");
        // 覆盖写：留下的是最新内容。
        cache.write("quotes", "world");
        assert_eq!(cache.read("quotes").expect("cached").0, "world");
        let _ = fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn ignores_empty_payloads() {
        let cache = temp_cache("empty");
        cache.write("empty", "  \n");
        assert!(cache.read("empty").is_none());
        let _ = fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn rejects_stale_entries() {
        let cache = temp_cache("stale");
        cache.write("old", "payload");
        assert!(cache.read_fresh("old", Duration::from_secs(60)).is_some());
        // 把文件的修改时间拨回一小时前，模拟过期缓存。
        let old = SystemTime::now() - Duration::from_secs(3600);
        let path = cache.dir().join("old.txt");
        fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(old))
            .expect("set mtime");
        assert!(cache.read_fresh("old", Duration::from_secs(60)).is_none());
        let _ = fs::remove_dir_all(cache.dir());
    }
}
