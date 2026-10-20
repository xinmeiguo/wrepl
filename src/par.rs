//! 零依赖的并行映射，供流水线、批量验证、侦察三处共用。
//!
//! ## 为什么单独成模块
//!
//! 这套东西最早长在 [`crate::pipeline`] 里，只有"逐文件改写"一段在用。
//! 2026-10-20 批量验证（`verify::pair_dirs`）与侦察（`probe::probe_dir`）
//! 也要按文件并行 —— 三处都是"一堆互不依赖的文件，处理结果按原顺序归位"，
//! 同一个需求。复制第二份的那天起，两条路的并发上限、失败语义就会各走各的，
//! 所以搬出来一份。
//!
//! ## 三条必须守住的（从 pipeline 迁来时立的规矩，仍然有效）
//!
//! 1. **结果顺序 = 输入顺序**。报告行序若随线程调度抖动，同一批文件两次跑出
//!    两份不同的报告，归档时无法解释。所以结果落在与下标一一对应的格子里，
//!    收尾时按顺序取出。
//! 2. **单个 item 失败不中断整批**。失败由 `f` 自己包成结果值，并行层不感知错误。
//! 3. `threads == 1` 时**不启线程**，走纯串行路径。这既是排查
//!    "是不是并行的锅"的第一手段，也是并行版与串行版做等价对照的开关。
//!
//! ## 分配策略
//!
//! 用**原子取号**（动态负载均衡）而不是预先把文件平均切给各线程：
//! docx 大小差别很大（同一交付包里 100 KB 与 5 MB 混在一起），
//! 均分会让线程之间进度差出好几倍，取号则谁空谁拿下一个。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// 算出真正要用几个线程：`requested = 0` 表示"用满本机核数"。
///
/// 上限是 `jobs`（活没那么多时开满线程只是白起线程）。
///
/// 为什么默认给满核数而不是压到 8 之类：逐文件改写是整批里最重的一段，
/// 把默认并发压下来会让大文档批明显变慢；而真实 docx 尺寸（几 MB/件）下，
/// 高并发带来的内存峰值并不构成问题。真撞上「超大文档 × 高核数」，
/// 用 `--threads N` 手动压下来即可。
pub fn resolve_threads(requested: usize, jobs: usize) -> usize {
    let n = if requested > 0 {
        requested
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    };
    n.max(1).min(jobs.max(1))
}

/// 把 `items` 按索引分发给若干工作线程，**结果按原顺序返回**。
///
/// ## 为什么手写而不是 rayon
///
/// 离线构建环境里没有 rayon / crossbeam 的缓存，也不值得为一个 `par_iter`
/// 引入新的依赖树。这里要的恰好就是"按索引并行、按索引归位"，
/// 标准库的 `thread::scope` + 原子计数器就够。
pub fn parallel_map<T, R, F>(items: &[T], requested: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    let n = items.len();
    let workers = resolve_threads(requested, n);
    if workers <= 1 || n <= 1 {
        return (0..n).map(|i| f(i, &items[i])).collect();
    }

    let next = AtomicUsize::new(0);
    // 每格一把锁：一个文件算完才落一次，锁竞争可以忽略。
    let slots: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let r = f(i, &items[i]);
                // 锁中毒（某个线程 panic）也要把值放进去，否则收尾取不到
                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            });
        }
    });

    // 到这里所有工作线程都已 join，每格必然被填过一次。
    slots
        .into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .map(|o| o.expect("每个索引恰好被处理一次"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par_result_order_matches_input() {
        // 故意让先领到的活算得慢：顺序若靠"谁先算完"就会错
        let items: Vec<usize> = (0..64).collect();
        let out = parallel_map(&items, 4, |i, &x| {
            if i % 7 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            x * 2
        });
        assert_eq!(out, items.iter().map(|x| x * 2).collect::<Vec<_>>());
    }

    #[test]
    fn par_threads_one_is_serial() {
        // threads=1 不启线程：用"当前线程 id"直接验证
        let items: Vec<u8> = (0..8).collect();
        let main = std::thread::current().id();
        let out = parallel_map(&items, 1, |_, &x| {
            assert_eq!(std::thread::current().id(), main);
            x
        });
        assert_eq!(out, items);
    }

    #[test]
    fn par_empty_and_single() {
        let empty: Vec<u8> = Vec::new();
        assert!(parallel_map(&empty, 8, |_, &x| x).is_empty());
        assert_eq!(parallel_map(&[7u8], 8, |_, &x| x + 1), vec![8]);
    }

    #[test]
    fn resolve_threads_bounds() {
        assert_eq!(resolve_threads(0, 0), 1); // 无活也要给 1，避免 0 线程
        assert_eq!(resolve_threads(0, 1), 1);
        assert_eq!(resolve_threads(16, 3), 3); // 活少时不超过活数
        assert_eq!(resolve_threads(4, 100), 4); // 显式指定优先
    }
}
