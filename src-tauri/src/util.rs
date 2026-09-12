//! 通用小工具

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// 取互斥锁；**锁中毒时恢复而不是 panic**。
///
/// 引擎里有 60 多处取锁（状态、气泡、播放、偏好……），几乎所有线程都会用到其中几个。
/// 如果某处 panic 让锁中毒，`.unwrap()` 会把后续每一个线程一起带崩——对桌面常驻程序来说
/// 表现为"角色悄悄不动了"。中毒意味着上一个持锁者的临界区被中断，但本项目所有临界区
/// 都只做内存读写（不做 IO/回调），恢复后继续用是安全的。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// 递归复制目录内容到 `dst`（`dst` 可不存在）。用于旧版数据目录迁移。
/// 只做普通文件与子目录复制；遇到符号链接按文件复制（Windows 上角色目录不含链接）。
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("创建目录失败 {}: {e}", dst.display()))?;
    let entries =
        std::fs::read_dir(src).map_err(|e| format!("读取目录失败 {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("读取目录项失败: {e}"))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| format!("复制文件失败 {}: {e}", from.display()))?;
        }
    }
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

    #[test]
    fn copy_dir_recursive_copies_nested_tree() {
        let root = std::env::temp_dir().join(format!(
            "deskzen-util-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = root.join("old");
        let dst = root.join("new");
        std::fs::create_dir_all(src.join("characters/local-demo/clips")).unwrap();
        std::fs::write(src.join("prefs.json"), "{\"zoom\":1.5}").unwrap();
        std::fs::write(src.join("characters/local-demo/clips/wave.webp"), b"RIFF").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("prefs.json")).unwrap(),
            "{\"zoom\":1.5}"
        );
        assert_eq!(
            std::fs::read(dst.join("characters/local-demo/clips/wave.webp")).unwrap(),
            b"RIFF"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
