//! 取消信号：一个只增不减的原子布尔。
//!
//! ## 为什么不用 channel
//!
//! 取消是**广播 + 幂等 + 永不回滚**的：任何一处都可以发、任何一处都可以查，
//! 且不关心谁收到、收几次。channel 需要有人收、且只能被收一次 —— 而"停止"
//! 这件事天然是"所有人同时知道"，不是一个排队的事件。
//!
//! ## 检查点只放在文件边界
//!
//! 一个文件改到一半时停下，产物就是半成品（zip 写了一半、或者内存里改完了没落盘），
//! 这比"这个文件压根没处理"难解释得多。所以取消只在**两个文件之间**生效：
//! 已开始的那个一定做完，后面的不再开始。代价是取消最多等一个文件（本机实测
//! 单件 40 ms 左右，8.8 MB 的媒体型文档约 1 秒）。
//!
//! ## `Relaxed` 够不够
//!
//! 这里只用它做"要不要继续"的判断，没有别的数据依赖它，也不需要和其他内存
//! 操作建立 happens-before。`Relaxed` 在保证"最终可见"这一层已经足够，
//! 用 `SeqCst` 只是白付一次全核同步的代价。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 可克隆的取消令牌。克隆出来的都是**同一份**状态。
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// 新建一个"尚未取消"的令牌。
    pub fn new() -> Self {
        Self::default()
    }

    /// 发取消。重复调用无副作用。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// 是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 新建的令牌没被取消() {
        assert!(!Cancel::new().is_cancelled());
    }

    #[test]
    fn 取消对所有克隆可见() {
        let a = Cancel::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled(), "克隆共享同一份状态，不是各自一份");
    }

    #[test]
    fn 重复取消不报错也不回滚() {
        let c = Cancel::new();
        c.cancel();
        c.cancel();
        assert!(c.is_cancelled(), "取消是单向的，没有 un-cancel");
    }

    /// 跨线程：一个线程发、另一个线程查（真实用法就是界面线程发、工作线程查）。
    #[test]
    fn 跨线程可见() {
        let c = Cancel::new();
        let worker = c.clone();
        let h = std::thread::spawn(move || {
            let mut n = 0;
            while !worker.is_cancelled() {
                n += 1;
                if n > 10_000_000 {
                    break;
                }
                std::thread::yield_now();
            }
            n
        });
        c.cancel();
        let n = h.join().expect("工作线程不应 panic");
        assert!(n < 10_000_000, "取消没能让工作线程退出");
    }
}
