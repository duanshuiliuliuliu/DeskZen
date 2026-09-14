//! 运行日志：自己写文件，单份大小与级别都能在设置页即时调整。
//!
//! - 落点：`<app_log_dir>/deskzen.log`（Windows：`%LOCALAPPDATA%\com.deskzen.desktop\logs`）
//! - 滚动：单份超过设置的大小（5/10/30 MB，默认 10MB）就重命名成 `deskzen_<时间>.log`，
//!   只保留最近 [`KEEP_FILES`] 份历史文件；启动时接着现有文件追加
//! - 级别：默认 `debug`（记抽链、每拍推进、气泡调度这些细节），设置页可切 trace/debug/info/warn/error
//! - 记录重点：状态为什么切、链为什么被选中或跳过、气泡为什么没说、AI 生成为什么失败；
//!   **不记录聊天内容**，只记请求规模与结果
//!
//! 之所以不用现成的日志插件：滚动大小与级别都要能在运行中改，而插件的这两项在启动时就固定了。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Manager};

/// 历史日志保留份数（不含当前正在写的那份）
const KEEP_FILES: usize = 3;
/// 日志文件名（不含滚动后缀）
const FILE_STEM: &str = "deskzen";
/// 设置页可选的单文件大小档位（MB）
pub(crate) const SIZE_OPTIONS_MB: &[u64] = &[5, 10, 30];
/// 单文件大小默认值（MB）
pub(crate) const DEFAULT_SIZE_MB: u64 = 10;
/// 可选的日志级别（由轻到重）
pub(crate) const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];
/// 默认级别：debug —— 排查问题时默认就有细节可看
pub(crate) const DEFAULT_LEVEL: &str = "debug";

/// 当前级别（存 `log::LevelFilter` 的判别值；注意 log 的次序是 error=1 … trace=5）
static LEVEL_FILTER: AtomicUsize = AtomicUsize::new(log::LevelFilter::Debug as usize);
/// 当前单文件上限（字节）
static MAX_BYTES: AtomicU64 = AtomicU64::new(DEFAULT_SIZE_MB * 1024 * 1024);

/// 级别名 → 过滤级别；未知返回 None
fn filter_of(level: &str) -> Option<log::LevelFilter> {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(log::LevelFilter::Trace),
        "debug" => Some(log::LevelFilter::Debug),
        "info" => Some(log::LevelFilter::Info),
        "warn" | "warning" => Some(log::LevelFilter::Warn),
        "error" => Some(log::LevelFilter::Error),
        _ => None,
    }
}

/// 当前过滤级别
fn current_filter() -> log::LevelFilter {
    match LEVEL_FILTER.load(Ordering::Relaxed) {
        rank if rank == log::LevelFilter::Trace as usize => log::LevelFilter::Trace,
        rank if rank == log::LevelFilter::Debug as usize => log::LevelFilter::Debug,
        rank if rank == log::LevelFilter::Info as usize => log::LevelFilter::Info,
        rank if rank == log::LevelFilter::Warn as usize => log::LevelFilter::Warn,
        _ => log::LevelFilter::Error,
    }
}

/// 设置页下拉用的大小档位（MB）
pub(crate) fn size_options_mb() -> Vec<u64> {
    SIZE_OPTIONS_MB.to_vec()
}

/// 当前日志级别名
pub(crate) fn level_name() -> &'static str {
    match current_filter() {
        log::LevelFilter::Trace => "trace",
        log::LevelFilter::Debug => "debug",
        log::LevelFilter::Info => "info",
        log::LevelFilter::Warn => "warn",
        _ => "error",
    }
}

/// 当前单文件上限（MB）
pub(crate) fn size_mb() -> u64 {
    MAX_BYTES.load(Ordering::Relaxed) / (1024 * 1024)
}

/// 归一化级别：未知值退回默认（配置被手改坏时不至于丢日志）
pub(crate) fn normalize_level(level: &str) -> &'static str {
    match filter_of(level) {
        Some(log::LevelFilter::Trace) => "trace",
        Some(log::LevelFilter::Debug) => "debug",
        Some(log::LevelFilter::Info) => "info",
        Some(log::LevelFilter::Warn) => "warn",
        Some(log::LevelFilter::Error) => "error",
        _ => DEFAULT_LEVEL,
    }
}

/// 归一化大小：不在档位里就退回默认
pub(crate) fn normalize_size_mb(size_mb: u64) -> u64 {
    if SIZE_OPTIONS_MB.contains(&size_mb) {
        size_mb
    } else {
        DEFAULT_SIZE_MB
    }
}

/// 运行中切换级别（对后续记录立即生效）
pub(crate) fn set_level(level: &str) -> &'static str {
    let name = normalize_level(level);
    let filter = filter_of(name).unwrap_or(log::LevelFilter::Debug);
    LEVEL_FILTER.store(filter as usize, Ordering::Relaxed);
    name
}

/// 运行中切换单文件上限（下一次滚动判断就用新值）
pub(crate) fn set_max_size_mb(size_mb: u64) -> u64 {
    let mb = normalize_size_mb(size_mb);
    MAX_BYTES.store(mb * 1024 * 1024, Ordering::Relaxed);
    mb
}

/// 模块名前缀去掉，日志里只留 `engine` / `playback` 这种短标签
fn short_target(target: &str) -> &str {
    match target.strip_prefix("deskzen_lib::") {
        Some(rest) => rest,
        None if target == "deskzen_lib" => "app",
        None => target,
    }
}

/// 日志写入器：当前文件句柄 + 已写字节数
struct Logger {
    dir: PathBuf,
    state: Mutex<State>,
}

struct State {
    /// 当前文件句柄；滚动期间短暂为 None（先关句柄再改名，Windows 上必须这样）
    file: Option<File>,
    bytes: u64,
}

impl Logger {
    /// 打开（或追加）日志文件；目录不可用时返回 None（日志失败不影响功能）
    fn new(dir: PathBuf) -> Option<Self> {
        fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{FILE_STEM}.log"));
        let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Self {
            dir,
            state: Mutex::new(State {
                file: Some(file),
                bytes,
            }),
        })
    }

    /// 写一行；超过上限先把当前文件滚成带时间戳的历史文件
    fn write_line(&self, line: &str) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let limit = MAX_BYTES.load(Ordering::Relaxed);
        if state.bytes > 0 && state.bytes + line.len() as u64 > limit {
            self.rotate(&mut state);
        }
        let Some(file) = state.file.as_mut() else {
            return;
        };
        if writeln!(file, "{line}").is_ok() {
            state.bytes += line.len() as u64 + 1;
        }
    }

    /// 滚动：当前文件改名成 `deskzen_<本地时间>.log`，再开一个新的当前文件
    fn rotate(&self, state: &mut State) {
        let current = self.dir.join(format!("{FILE_STEM}.log"));
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        // 同一秒内连续滚动时避免撞名：加一个递增序号
        let mut rotated = self.dir.join(format!("{FILE_STEM}_{stamp}.log"));
        let mut seq = 0;
        while rotated.exists() {
            seq += 1;
            rotated = self.dir.join(format!("{FILE_STEM}_{stamp}_{seq}.log"));
        }
        // 先关句柄再改名（Windows 上打开中的文件不能改名）
        drop(state.file.take());
        if fs::rename(&current, &rotated).is_err() {
            // 改名失败就接着往原文件写，别把日志弄丢
            state.file = OpenOptions::new().create(true).append(true).open(&current).ok();
            return;
        }
        state.file = OpenOptions::new().create(true).append(true).open(&current).ok();
        state.bytes = 0;
        self.prune();
    }

    /// 只保留最近 [`KEEP_FILES`] 份历史文件（按文件名里的时间戳排序）
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut history: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(&format!("{FILE_STEM}_")) && name.ends_with(".log")
                    })
            })
            .collect();
        history.sort();
        if history.len() > KEEP_FILES {
            for path in &history[..history.len() - KEEP_FILES] {
                let _ = fs::remove_file(path);
            }
        }
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // 更严重的级别数值更小，所以"记录级别 ≤ 当前级别"就是放行
        current_filter()
            .to_level()
            .is_some_and(|max| metadata.level() <= max)
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}: {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            record.level().as_str(),
            short_target(record.target()),
            record.args()
        );
        // 开发时从终端启动也能直接看到
        println!("{line}");
        self.write_line(&line);
    }

    fn flush(&self) {}
}

/// 安装日志：在 setup 阶段调用一次（重复调用只更新级别与大小）
pub(crate) fn install(app: &AppHandle, level: &str, size_mb: u64) {
    set_level(level);
    set_max_size_mb(size_mb);
    let Ok(dir) = app.path().app_log_dir() else {
        return;
    };
    if let Some(logger) = Logger::new(dir) {
        log::set_max_level(log::LevelFilter::Trace);
        // 只可能失败一次（已经装过 logger）：参数在上面已经更新过了
        let _ = log::set_boxed_logger(Box::new(logger));
    }
}

/// 日志目录（打开用）
pub(crate) fn log_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_log_dir().ok()
}

/// 在系统文件管理器里打开日志目录
pub(crate) fn open_dir(app: &AppHandle) -> Result<(), String> {
    let dir = log_dir(app).ok_or("取不到日志目录")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&dir).spawn();
    result.map(|_| ()).map_err(|e| e.to_string())
}

/// 前端日志转发：把 webview 的 console / 未捕获错误写进同一个文件
pub(crate) fn log_from_web(level: &str, message: &str) {
    let message = message.trim();
    if message.is_empty() {
        return;
    }
    // 前端消息可能很长（堆栈），截断避免刷屏
    let message = if message.chars().count() > 2000 {
        let cut: String = message.chars().take(2000).collect();
        format!("{cut}…（已截断）")
    } else {
        message.to_string()
    };
    match normalize_level(level) {
        "trace" => log::trace!(target: "web", "{message}"),
        "debug" => log::debug!(target: "web", "{message}"),
        "warn" => log::warn!(target: "web", "{message}"),
        "error" => log::error!(target: "web", "{message}"),
        _ => log::info!(target: "web", "{message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这两个用例会改全局的级别/大小，串行执行避免互相干扰（cargo 默认并行跑用例）
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn level_normalization_falls_back_to_debug() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert_eq!(normalize_level("DEBUG"), "debug");
        assert_eq!(normalize_level(" warn "), "warn");
        assert_eq!(normalize_level("loud"), DEFAULT_LEVEL);
        assert_eq!(set_level("error"), "error");
        assert_eq!(level_name(), "error");
        assert_eq!(set_level(DEFAULT_LEVEL), DEFAULT_LEVEL);
    }

    /// 级别过滤要"更严重的都放行、更轻的挡掉"（log 的判别值是 error=1…trace=5，容易写反）
    #[test]
    fn level_filter_keeps_more_severe_records() {
        use log::Log as _;
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("deskzen-log-level-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let logger = Logger::new(dir.clone()).expect("测试目录应可写");
        let enabled = |level: log::Level| {
            logger.enabled(
                &log::Metadata::builder()
                    .level(level)
                    .target("test")
                    .build(),
            )
        };

        set_level("debug");
        assert!(enabled(log::Level::Debug), "debug 级别下要记 debug");
        assert!(enabled(log::Level::Info));
        assert!(enabled(log::Level::Error));

        set_level("info");
        assert!(!enabled(log::Level::Debug), "info 级别下不记 debug");
        assert!(enabled(log::Level::Info));

        set_level("error");
        assert!(!enabled(log::Level::Warn));
        assert!(enabled(log::Level::Error));

        set_level(DEFAULT_LEVEL);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_options_are_the_documented_ones() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert_eq!(size_options_mb(), vec![5, 10, 30]);
        assert_eq!(normalize_size_mb(30), 30);
        assert_eq!(normalize_size_mb(1), DEFAULT_SIZE_MB, "1M 已下线，回退默认");
        assert_eq!(normalize_size_mb(7), DEFAULT_SIZE_MB);
        assert_eq!(DEFAULT_SIZE_MB, 10);
        assert_eq!(set_max_size_mb(5), 5);
        assert_eq!(size_mb(), 5);
        assert_eq!(set_max_size_mb(DEFAULT_SIZE_MB), DEFAULT_SIZE_MB);
    }

    #[test]
    fn short_target_strips_crate_prefix() {
        assert_eq!(short_target("deskzen_lib::playback"), "playback");
        assert_eq!(short_target("deskzen_lib"), "app");
        assert_eq!(short_target("web"), "web");
    }

    #[test]
    fn rotates_when_exceeding_limit_and_keeps_only_recent_files() {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("deskzen-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // 上限临时压到 1KB，方便用少量写入触发多次滚动
        let before = MAX_BYTES.load(Ordering::Relaxed);
        MAX_BYTES.store(1024, Ordering::Relaxed);

        let logger = Logger::new(dir.clone()).expect("测试目录应可写");
        for _ in 0..200 {
            logger.write_line(&"x".repeat(64));
        }

        assert!(dir.join("deskzen.log").exists(), "当前日志文件应存在");
        let history: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with("deskzen_"))
            .collect();
        assert!(!history.is_empty(), "超过上限后应产生历史文件");
        assert!(
            history.len() <= KEEP_FILES,
            "历史文件应只保留最近 {KEEP_FILES} 份，实际 {}",
            history.len()
        );

        let _ = fs::remove_dir_all(&dir);
        MAX_BYTES.store(before, Ordering::Relaxed);
    }
}
