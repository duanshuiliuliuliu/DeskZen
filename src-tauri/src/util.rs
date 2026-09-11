//! 通用小工具

use std::sync::{Mutex, MutexGuard};
use std::path::Path;

/// 取互斥锁；**锁中毒时恢复而不是 panic**。
///
/// 引擎里有 60 多处取锁（状态、气泡、播放、偏好……），几乎所有线程都会用到其中几个。
/// 如果某处 panic 让锁中毒，`.unwrap()` 会把后续每一个线程一起带崩——对桌面常驻程序来说
/// 表现为"角色悄悄不动了"。中毒意味着上一个持锁者的临界区被中断，但本项目所有临界区
/// 都只做内存读写（不做 IO/回调），恢复后继续用是安全的。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 原子写文本文件：先写同目录的 `<文件名>.tmp`，再 rename 覆盖目标。
/// 这样即使写到一半崩溃，也不会留下被截断的半截配置（偏好/历史/气泡缓存共用）。
pub(crate) fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("路径缺少父目录: {}", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp_name);
    std::fs::write(&tmp, contents).map_err(|e| format!("写入临时文件失败: {e}"))?;
    // Windows 下 rename 使用 MoveFileEx + REPLACE_EXISTING，可原子覆盖已存在的目标文件
    std::fs::rename(&tmp, path).map_err(|e| format!("替换文件失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn lock_recovers_from_poisoned_mutex() {
        let value = Arc::new(Mutex::new(1));
        let cloned = Arc::clone(&value);
        // 另一个线程持锁时 panic，锁被标记为中毒
        let _ = thread::spawn(move || {
            let _guard = cloned.lock().unwrap();
            panic!("模拟临界区 panic");
        })
        .join();
        assert!(value.lock().is_err(), "锁应处于中毒状态");
        // 恢复后仍能读写，且数据完好
        *lock(&value) += 1;
        assert_eq!(*lock(&value), 2);
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!(
            "deskzen-util-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = dir.join("prefs.json");
        atomic_write(&target, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":1}");
        // 覆盖写：内容更新，且不残留 .tmp
        atomic_write(&target, "{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":2}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
