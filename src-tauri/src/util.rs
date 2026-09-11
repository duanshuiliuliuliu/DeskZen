//! 通用小工具

use std::sync::{Mutex, MutexGuard};

/// 取互斥锁；**锁中毒时恢复而不是 panic**。
///
/// 引擎里有 60 多处取锁（状态、气泡、播放、偏好……），几乎所有线程都会用到其中几个。
/// 如果某处 panic 让锁中毒，`.unwrap()` 会把后续每一个线程一起带崩——对桌面常驻程序来说
/// 表现为"角色悄悄不动了"。中毒意味着上一个持锁者的临界区被中断，但本项目所有临界区
/// 都只做内存读写（不做 IO/回调），恢复后继续用是安全的。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
}
