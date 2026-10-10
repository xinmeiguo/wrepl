//! 运行日志 + 崩溃捕获。
//!
//! ## 为什么需要它
//!
//! 图形界面版跑在 **Windows 图形子系统**（PE Subsystem = 2，见 `main.rs`），
//! 没有控制台 —— `stdout` / `stderr` 全部丢掉。程序一旦 panic，用户
//! **只能看到窗口凭空消失**，什么线索都没有，连"是崩了还是被关了"都分不清。
//!
//! 这个模块把两类信息落到同一个文件里：
//!
//! 1. **关键节点的日志** —— 启动 / 打开面板 / 列目录 / 行点击 / 结果 / 退出
//! 2. **panic 的完整信息** —— 消息、位置（文件:行:列）、调用栈
//!
//! 好处是「日志停在哪一条」本身就有诊断意义，三种结局可以区分开：
//!
//! | 日志末尾 | 结论 |
//! |---|---|
//! | 有 `!!! PANIC !!!` 段 | 程序自己崩了，位置就在那一段里 |
//! | 有「主循环结束」 | 窗口被正常关闭（`WM_CLOSE` / Alt+F4） |
//! | 埋点中断，两者都没有 | 进程被外部干掉（杀软 / 管控套件） |
//!
//! 日志位置：`%LOCALAPPDATA%\wrepl\wrepl-gui.log`（超过 1 MB 轮转成 `.log.old`）。
//! 程序崩溃时还会弹一个对话框把路径告诉用户 —— 否则没人知道去哪找。

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// 超过这个大小就轮转一次，免得日志无限长。
const MAX_BYTES: u64 = 1_048_576;

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
/// 串行化写入。多线程（UI + 后台列目录）都可能来写。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn log_path() -> Option<PathBuf> {
    LOG_PATH
        .get_or_init(|| {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let dir = base.join("wrepl");
            std::fs::create_dir_all(&dir).ok()?;
            Some(dir.join("wrepl-gui.log"))
        })
        .clone()
}

/// 日志文件路径（给对话框显示用）。
pub fn path_text() -> String {
    log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "（取不到日志路径）".to_string())
}

/// 记一行。任何线程都能调。
pub fn log(msg: impl AsRef<str>) {
    write_line(msg.as_ref());
}

fn write_line(msg: &str) {
    let Some(p) = log_path() else { return };
    // 锁中毒也要继续写 —— 崩溃现场比锁的洁癖重要
    let _g = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(md) = std::fs::metadata(&p) {
        if md.len() > MAX_BYTES {
            let _ = std::fs::rename(&p, p.with_extension("log.old"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "[{}] {}", stamp(), msg);
    }
}

/// 安装 panic 钩子，并写一条启动头。**只生效一次**。
pub fn init(what: &str) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut m = String::from("!!! PANIC !!!");
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "（payload 不是字符串）".to_string());
            m.push_str(&format!("\n  消息: {msg}"));
            match info.location() {
                Some(l) => m.push_str(&format!("\n  位置: {}:{}:{}", l.file(), l.line(), l.column())),
                None => m.push_str("\n  位置: （未知）"),
            }
            m.push_str("\n  调用栈:");
            for line in std::backtrace::Backtrace::force_capture()
                .to_string()
                .lines()
            {
                m.push_str(&format!("\n    {line}"));
            }
            m.push_str("\n  ↑ 这一段就是程序退出的原因");
            write_line(&m);
            crash_box(&msg);
            prev(info);
        }));
        write_line(&format!("======== {what} v{} 启动 ========", env!("CARGO_PKG_VERSION")));
        write_line(&format!("日志: {}", path_text()));
        if let Ok(exe) = std::env::current_exe() {
            write_line(&format!("程序: {}", exe.display()));
        }
        write_line(&format!(
            "参数: {:?}",
            std::env::args().skip(1).collect::<Vec<_>>()
        ));
    });
}

/// 弹出「出错了」对话框。GUI 版没有控制台，不弹窗用户就只见窗口消失。
#[cfg(windows)]
fn crash_box(msg: &str) {
    box_impl("wrepl —— 出错了", &format!(
        "wrepl 遇到内部错误，必须关闭。\n\n{msg}\n\n完整日志：\n{}",
        path_text()
    ));
}

/// 弹「窗口没能打开」的对话框。
///
/// ## 为什么这条非得有不可
///
/// 图形界面版是**图形子系统**（PE Subsystem = 2），没有控制台 ——
/// `stderr` 无处可去。而 "窗口建不出来" 恰恰是它最常见的失败方式：
/// 显卡驱动只提供 OpenGL 1.1（远程桌面、虚拟机、只装了「Microsoft 基本显示适配器」
/// 的机器都是这样）时，`eframe::run_native` 会**返回 Err**，
/// `main` 一返回错误，进程就安安静静地退出了 ——
/// **用户看到的就是"双击了，什么也没发生"**，连一句提示都没有。
///
/// 所以这里把话说全：出了什么事、为什么、日志在哪、
/// 以及**接下来能干什么**（命令行版干的是同一件事，而且不需要显卡）。
#[cfg(windows)]
pub fn startup_failure_box(detail: &str) {
    let text = format!(
        "wrepl 图形界面没能打开窗口，程序即将退出。\n\
         \n\
         最常见的原因（按出现频率）：\n\
         1. 这台机器的显卡驱动没有提供 OpenGL 2.1 以上 ——\n\
         \x20  远程桌面、虚拟机、或只装了「Microsoft 基本显示适配器」时都是这样；\n\
         2. 安全软件 / 公司管控套件拦住了本程序创建窗口；\n\
         3. 系统过旧（本程序要求 64 位 Windows 10 及以上，且需 x64 CPU）。\n\
         \n\
         ── 不影响你干活 ──\n\
         同一个压缩包里的 wrepl.exe（命令行版）做的是**完全一样的事**，\n\
         而且不需要显卡、不需要窗口。用法：\n\
         \n\
         \x20   wrepl.exe apply \"要处理的文件夹\" --rules-file \"规则.txt\" --verify-after\n\
         \n\
         规则文件每行一条，制表符分隔：查找内容<TAB>替换为。\n\
         先用 --dry-run 空跑一遍看命中，再正式跑。\n\
         \n\
         ── 完整日志 ──\n\
         %LOCALAPPDATA%\\wrepl\\wrepl-gui.log\n\
         {}\n\
         \n\
         ── 技术细节 ──\n\
         {}",
        path_text(),
        detail
    );
    box_impl("wrepl —— 窗口没能打开", &text);
}

#[cfg(not(windows))]
pub fn startup_failure_box(detail: &str) {
    eprintln!(
        "wrepl 图形界面没能打开窗口：{detail}\n\
         （命令行版 wrepl 做的是同一件事，且不需要显卡）\n日志：{}",
        path_text()
    );
}

/// 弹一个只读的模态消息框。失败就算了 —— 报错本身不该再引发错误。
#[cfg(windows)]
fn box_impl(caption: &str, text: &str) {
    unsafe extern "system" {
        fn MessageBoxW(hwnd: *mut core::ffi::c_void, text: *const u16, caption: *const u16, ty: u32)
        -> i32;
    }
    let t: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let c: Vec<u16> = caption.encode_utf16().chain(std::iter::once(0)).collect();
    const MB_ICONERROR: u32 = 0x0000_0010;
    const MB_SETFOREGROUND: u32 = 0x0001_0000;
    const MB_TOPMOST: u32 = 0x0004_0000;
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            t.as_ptr(),
            c.as_ptr(),
            MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

#[cfg(not(windows))]
fn crash_box(msg: &str) {
    eprintln!("wrepl 遇到内部错误: {msg}（日志：{}）", path_text());
}

// ─────────────────────────── 诊断开关 ───────────────────────────
//
// 这台机器上**合成鼠标输入到不了窗口**（`SendInput` 与 `PostMessage` 都在
// 管控套件的拦截层里，实测前台抢到了、光标到位了，egui 一个事件都收不到）。
// 于是"自己点一下看看"这条常规复现路子是断的，只能让程序自己把动作做出来。
//
// 下面两个环境变量只服务于这个目的，正常使用不会碰到（不设 = 完全无影响）。

/// `WREPL_DEBUG_PICKER=dir|file|save` —— 启动即把选择器打开。
pub fn debug_picker_mode() -> Option<String> {
    let v = std::env::var("WREPL_DEBUG_PICKER").ok()?;
    let v = v.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    Some(v)
}

/// `WREPL_DEBUG_CLICK=<行号>[,d]` —— 面板首次列出内容后，模拟点一下第 N 行
/// （`d` 后缀 = 双击）。**只触发一次**。
pub fn debug_click() -> Option<(usize, bool)> {
    static USED: AtomicBool = AtomicBool::new(false);
    if USED.swap(true, Ordering::SeqCst) {
        return None;
    }
    let v = std::env::var("WREPL_DEBUG_CLICK").ok()?;
    let (n, d) = match v.split_once(',') {
        Some((a, b)) => (a.trim().parse::<usize>().ok()?, b.trim().eq_ignore_ascii_case("d")),
        None => (v.trim().parse::<usize>().ok()?, false),
    };
    Some((n, d))
}

// ─────────────────────────── 时间戳 ───────────────────────────

/// 本机时区的 `YYYY-MM-DD HH:MM:SS.mmm`。走 Win32 `GetLocalTime`，
/// 比 SystemTime 少一层"我算时区"的风险（见 `picker` 里同类做法）。
#[cfg(windows)]
fn stamp() -> String {
    #[repr(C)]
    #[derive(Default)]
    struct SYSTEMTIME {
        year: u16,
        month: u16,
        dow: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        ms: u16,
    }
    unsafe extern "system" {
        fn GetLocalTime(t: *mut SYSTEMTIME);
    }
    let mut t = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.year, t.month, t.day, t.hour, t.minute, t.second, t.ms
    )
}

#[cfg(not(windows))]
fn stamp() -> String {
    "---------- --:--:--.---".to_string()
}
