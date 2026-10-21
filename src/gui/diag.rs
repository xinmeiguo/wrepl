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
/// 串行化写入 + **常驻文件句柄**。多线程（UI + 后台列目录）都可能来写。
///
/// 为什么存句柄而不是每条 `open`：Windows 上一次 `open` + `close` 是两次
/// syscall，各约 0.3–1 ms。一批 100 个文件、每文件几行日志就是几百次，
/// 实测能占掉 100–300 ms —— 纯属白花。复用句柄省掉的正是这两次调用。
///
/// 为什么**不用 `BufWriter`**：这个文件存在的唯一理由是抓**崩溃现场**
/// （见模块头那张表 —— "日志停在哪一条"本身就有诊断意义）。用户态缓冲会把
/// panic 前最后几行留在内存里没落盘，等于把最要紧的那几行丢掉。直接写
/// `File` 没有用户态缓冲，`writeln!` 落的每一行都直达内核。
static WRITE_LOCK: Mutex<Option<std::fs::File>> = Mutex::new(None);

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

// ─────────────────────────── 界面状态持久化 ───────────────────────────
//
// 只存「上次停在哪儿」这一件事：一个目录 + 最近 5 条。刻意**不**存窗口位置、
// 主题、排序偏好 —— 那些要么由窗口管理器管，要么改了会让"上次是什么样"
// 变成猜谜。范围越小越不容易写坏，也越不需要迁移逻辑。

/// 状态文件所在目录（与 `wrepl-gui.log` 同目录，见 `log_path`）。
///
/// `pub` 是因为 `state.rs` 的 `state.json` 也要落在这里 —— 两个文件放在一起，
/// 用户排查时只需去一个地方（见 [`crate::state`]）。
pub fn state_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join("wrepl");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    })
    .clone()
}

fn state_file() -> Option<PathBuf> {
    Some(state_dir()?.join("last-dir.txt"))
}

/// 读回「上次停留的目录」。
///
/// 读不出来（文件不存在 / 被手改坏 / 那个盘现在没插着）一律当"没有" ——
/// **绝不因此让程序起不来**。这个值只影响选择器一打开停在哪，
/// 拿不到就退回 home，跟 0.4.1 的行为一样。
pub fn load_last_dir() -> Option<PathBuf> {
    let raw = std::fs::read_to_string(state_file()?).ok()?;
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let p = PathBuf::from(t);
    // 盘拔了 / 目录被删了 → 当没有，让调用方退回 home，而不是给一条点不回来的路径。
    p.is_dir().then_some(p)
}

/// 记下「这次的目录」。
///
/// 写失败（只读介质、权限不足、磁盘满）**静默忽略** —— 这是一份便利状态，
/// 存不上不该影响任何正事。同 [`log`] 的取向。
pub fn save_last_dir(dir: &std::path::Path) {
    let Some(f) = state_file() else { return };
    if let Err(e) = std::fs::write(&f, dir.to_string_lossy().as_bytes()) {
        log(format!("上次目录写不进去（忽略）：{e}"));
    }
}

/// 记一行。任何线程都能调。
pub fn log(msg: impl AsRef<str>) {
    write_line(msg.as_ref());
}

fn write_line(msg: &str) {
    let Some(p) = log_path() else { return };
    // 锁中毒也要继续写 —— 崩溃现场比锁的洁癖重要
    let mut slot = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 轮转：**先把手上的句柄放掉，再改名**。
    // Windows 上被打开的文件（Rust 默认共享模式不含 `FILE_SHARE_DELETE`）
    // `rename` 会失败 —— 句柄不 drop，这一步就是空转，日志会一路长到撑爆磁盘。
    match std::fs::metadata(&p) {
        Ok(md) if md.len() > MAX_BYTES => {
            *slot = None;
            let _ = std::fs::rename(&p, p.with_extension("log.old"));
        }
        // 文件不在了（用户手删 / 清理工具扫掉）→ 丢掉旧句柄，下面重开一个。
        // 不重开的话写入会进那个已被删除的句柄，表现为"日志突然不更新了"。
        Err(_) => *slot = None,
        _ => {}
    }

    if slot.is_none() {
        *slot = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .ok();
    }
    if let Some(f) = slot.as_mut() {
        let _ = writeln!(f, "[{}] {}", stamp(), msg);
    }
}

// ───────────────────── 接管第三方库的日志（`log` 门面） ─────────────────────
//
// 图形子系统下没有 stderr，**库里打的日志默认全丢**。而「窗口建不出来」最要紧的
// 现场恰恰在库里：`egui_glow` 的 `painter.rs:150-158` 会打出
//
//     opengl version: 1.1.0
//     opengl renderer: GDI Generic
//     opengl vendor: Microsoft Corporation
//
// （源码注释写着"这三样在 gl 1.1 上也拿得到"），紧接着 `painter.rs:161` 才因为
// `gl.version().major < 2` 返回 `PainterError`。把这三行接进我们的日志文件，
// 「建窗失败」就从一句干巴巴的报错，变成能直接看出**驱动到底给了什么**。
//
// 噪音控制：`Info` 及以上全收；`Debug` 只放行与图形上下文直接相关的模块，
// 免得 egui/winit 的逐帧 debug 把这个小日志刷爆。想全看就设 `WREPL_LOG=debug`。

/// 这条 `log` 记录要不要收。**纯函数**，便于测试。
fn 放行(target: &str, level: log::Level, 全开: bool) -> bool {
    // log 的级别序是 Error < Warn < Info < Debug < Trace，
    // 所以 `<= Info` 就是「前三档全收」。
    if level <= log::Level::Info {
        return true;
    }
    if 全开 {
        return true;
    }
    // 其余（Debug / Trace）只关心会说出「图形上下文是什么」的那几个模块。
    const 关心的: [&str; 4] = ["egui_glow", "glutin", "glutin_winit", "eframe"];
    关心的.iter().any(|m| target.starts_with(m))
}

/// 环境变量 `WREPL_LOG=debug` 时全开。**只读一次**，别每条记录都去问环境。
fn 日志全开() -> bool {
    static ONCE: OnceLock<bool> = OnceLock::new();
    *ONCE.get_or_init(|| {
        std::env::var("WREPL_LOG").is_ok_and(|v| v.eq_ignore_ascii_case("debug"))
    })
}

/// 把 `log` 门面接到本模块的日志文件上。**只生效一次**，重复调用无副作用。
///
/// 接不上（已经有别人装过 logger）就安静放弃 —— 日志是辅助，不该因为它出错。
pub fn install_log_bridge() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        if log::set_boxed_logger(Box::new(日志桥)).is_ok() {
            log::set_max_level(log::LevelFilter::Debug);
        }
    });
}

struct 日志桥;

impl log::Log for 日志桥 {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        放行(m.target(), m.level(), 日志全开())
    }

    fn log(&self, r: &log::Record<'_>) {
        if !self.enabled(r.metadata()) {
            return;
        }
        // 库里爱打多行（`egui_glow` 那三行就是**一条** `debug!`），逐行写、
        // 续行缩进，免得跟我们自己的 `[时间戳]` 行混在一起分不清谁是谁。
        for (i, line) in r.args().to_string().lines().enumerate() {
            if i == 0 {
                write_line(&format!("[{}] {line}", r.level()));
            } else {
                write_line(&format!("        {line}"));
            }
        }
    }

    fn flush(&self) {}
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
        // 最后一步：把 `log` 门面接上。之后库（egui_glow / glutin / winit）打的
        // 日志都会落到同一个文件里 —— 建窗失败时那就是关键现场。
        install_log_bridge();
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
/// 已经能**确证**的失败原因。拿不准就传 `None`，弹窗退回「最常见原因」清单。
///
/// 加这个参数的动机很实在：日志上已经明明白白写着
/// `egui_glow requires opengl 2.0+` 的时候，再并列"安全软件拦截""系统过旧"
/// 只会稀释重点 —— 对方机器上那位同事就是被这三条并列的"可能"卡住，
/// 连点五次确定都没看懂该干嘛（2026-10-13 的真实日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum 已知原因 {
    /// 驱动没给出 OpenGL 2.0+（拿到的是微软软件渲染的 1.1）。
    缺OpenGL,
}

#[cfg(windows)]
pub fn startup_failure_box(detail: &str, 已知: Option<已知原因>) {
    box_impl("wrepl —— 窗口没能打开", &失败文案(detail, 已知));
}

/// 拼「窗口没能打开」要说的那段话。
///
/// **纯字符串**，和弹窗机制分开：`box_impl` 一调用就阻塞等用户点确定，
/// 没法在测试里跑；把文案抽出来，它自己就能被单元测试盯住 —— 尤其是
/// 「确证原因之后不许再并列那三条可能」这件容易改坏的事。
fn 失败文案(detail: &str, 已知: Option<已知原因>) -> String {
    // 「确证」和「靠猜」两种情况分开写：确证时只给一条结论 + 该干什么，
    // 拿不准才退回并列清单。
    let 原因段 = match 已知 {
        Some(已知原因::缺OpenGL) => {
            "── 原因已经确认 ──\n\
             这台机器的显卡驱动没有提供 OpenGL 2.0 以上，系统现在用的是微软自带的\n\
             软件渲染（OpenGL 1.1）—— 设备管理器 →「显示适配器」里的那个\n\
             「Microsoft 基本显示适配器」，就是它。\n\
             \n\
             注意：有显卡 ≠ 驱动在干活。显卡插着但驱动没装（或没生效）时，\n\
             系统就拿这个基础适配器兜底，而它只给到 OpenGL 1.1。\n\
             \n\
             装一下显卡驱动，本程序就能打开：\n\
             \x20 · 到显卡厂商官网按型号下载（Intel / NVIDIA / AMD）；\n\
             \x20 · 或到整机厂官网（Dell / HP / 联想等）按机器型号搜「显卡驱动」；\n\
             \x20 · 认准是哪块卡：装错厂商的驱动一点用也没有 ——\n\
             \x20   显示器接在 NVIDIA 卡上时，装 Intel 的驱动修不好它。\n\
             装好后「Microsoft 基本显示适配器」会从设备管理器里消失。\n"
        }
        None => {
            "最常见的原因（按出现频率）：\n\
             1. 这台机器的显卡驱动没有提供 OpenGL 2.0 以上 ——\n\
             \x20  远程桌面、虚拟机、或只装了「Microsoft 基本显示适配器」时都是这样；\n\
             2. 安全软件 / 公司管控套件拦住了本程序创建窗口；\n\
             3. 系统过旧（本程序要求 64 位 Windows 10 及以上，且需 x64 CPU）。\n"
        }
    };
    format!(
        "wrepl 图形界面没能打开窗口，程序即将退出。\n\
         \n\
         {原因段}\
         \n\
         ── 不影响你干活 ──\n\
         同一个压缩包里的 wrepl.exe（命令行版）做的是「完全一样的事」，\n\
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
    )
}

#[cfg(not(windows))]
pub fn startup_failure_box(detail: &str, _已知: Option<已知原因>) {
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

/// `WREPL_DEBUG_PAGE=replace|header|print` —— 启动就落在指定页。
///
/// 加「页眉替换 / 批量打印」两页之后，截图与演示都要让窗口一开就停在某页；
/// 而本机的合成鼠标事件进不来窗口（见本模块开头），只能让程序自己切。
pub fn debug_page_mode() -> Option<String> {
    let v = std::env::var("WREPL_DEBUG_PAGE").ok()?;
    let v = v.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    Some(v)
}

/// `WREPL_DEBUG_TOOL=kind|A|B|OUT` —— 启动就把「诊断工具」页的槽位填好。
///
/// 为什么需要：`capture_page.py` 点得中导航项与卡片按钮，但**点不中自绘选择器**
/// 里那一长串路径（要滚动 + 双击 + 确认，一串合成鼠标事件里错一步就前功尽弃）。
/// 让程序自己把参数摆好，截图脚本只需按一下「运行」，拍到的就是真实结果区。
///
/// 返回 `(kind 小写名, A, B, OUT)`；没设或格式不对返回 `None`（不报错、不影响开窗）。
pub fn debug_tool_args() -> Option<(String, String, String, String)> {
    let v = std::env::var("WREPL_DEBUG_TOOL").ok()?;
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    // 只按 `|` 切 4 段：Windows 路径里可能有 `:` 和 `\`，不能拿它们当分隔符。
    let mut it = v.splitn(4, '|');
    let kind = it.next()?.trim().to_ascii_lowercase();
    let a = it.next().unwrap_or("").trim().to_string();
    let b = it.next().unwrap_or("").trim().to_string();
    let out = it.next().unwrap_or("").trim().to_string();
    Some((kind, a, b, out))
}

/// `WREPL_DEBUG_DROP=hover|drop` —— 复现拖放的两态。
///
/// `hover` = 悬着东西不松手（整窗遮罩那一条路）；`drop` = 直接落地（走
/// [`crate::gui::app::App`] 的落点分派）。
///
/// 存在的理由同上：本机合成鼠标进不来窗口，而拖放是**按住不放**才有的状态，
/// 真实操作根本构造不出来 —— 不给开关就只能靠"信代码写了"。
/// 形如 `WREPL_DEBUG_DROP=drop;D:/wrepl-selftest` 可带上要拖的路径（`;` 分隔）。
pub fn debug_drop_mode() -> Option<(bool, Vec<std::path::PathBuf>)> {
    let v = std::env::var("WREPL_DEBUG_DROP").ok()?;
    let mut it = v.splitn(2, ';');
    let mode = it.next()?.trim().to_ascii_lowercase();
    if mode.is_empty() {
        return None;
    }
    let paths = it
        .next()
        .map(|s| {
            s.split([';', ','])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    Some((mode == "hover", paths))
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

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── 日志桥的收词规则 ────────────────────────────────────────────────
    //
    // 这条规则的价值全在"两侧都不能错"：
    //   收少了 → 建窗失败时看不到 `opengl version/renderer/vendor`，白接一趟；
    //   收多了 → egui/winit 的逐帧 debug 把小日志刷爆，真正的现场被埋掉。

    #[test]
    fn 放行_info_及以上一律收() {
        for lv in [log::Level::Error, log::Level::Warn, log::Level::Info] {
            assert!(放行("egui::anything", lv, false), "{lv} 不该被挡");
            assert!(放行("某个我不认识的库", lv, false), "{lv} 不该被挡");
        }
    }

    #[test]
    fn 放行_debug_只认图形上下文那几个模块() {
        // 要的就是这一条：egui_glow 在 painter.rs:150-158 打的三行
        assert!(放行("egui_glow::painter", log::Level::Debug, false));
        assert!(放行("glutin::api::wgl", log::Level::Debug, false));
        assert!(放行("eframe::native::run", log::Level::Debug, false));
        // 别的模块的 debug 不进日志（egui 逐帧、winit 事件循环都在此列）
        assert!(!放行("egui::context", log::Level::Debug, false));
        assert!(!放行("winit::event_loop", log::Level::Debug, false));
        assert!(!放行("egui_sauge", log::Level::Debug, false));
    }

    #[test]
    fn 放行_全开时_debug_不再挑模块() {
        assert!(放行("egui::context", log::Level::Debug, true));
        assert!(放行("随便什么", log::Level::Trace, true));
    }

    #[test]
    fn 放行_只看前缀不看包含() {
        // `starts_with` 而不是 `contains`：免得某个叫 "my-egui_glow-wrapper"
        // 的第三方模块顺带把 debug 全放进来。
        assert!(!放行("my-egui_glow", log::Level::Debug, false));
    }

    // ── 建窗失败的文案 ──────────────────────────────────────────────────

    #[test]
    fn 文案_确证缺opengl时_只给一条结论不再并列可能() {
        let t = 失败文案("egui_glow: OpenGL: egui_glow requires opengl 2.0+. ", Some(已知原因::缺OpenGL));
        assert!(t.contains("原因已经确认"), "确证了就该直说");
        assert!(t.contains("Microsoft 基本显示适配器"), "要指名道姓，对方才知道去哪看");
        assert!(t.contains("装一下显卡驱动"), "得给出下一步干什么");
        // 2026-10-21 补：只说"去官网下驱动"不够 —— 同事那台机器上，真正卡住的是
        // 「显示器接在 NVIDIA 卡上、却去装 Intel 的驱动」。两句都要在弹窗里。
        assert!(t.contains("按机器型号搜"), "驱动去哪找要具体到能照做");
        assert!(t.contains("装错厂商"), "踩过的坑得写进去，不然下一个人再踩一次");
        // ★ 这条是重点：已经确证还并列"安全软件拦截""系统过旧"只会稀释重点。
        assert!(!t.contains("最常见的原因"), "确证后不该再退回并列清单");
        assert!(!t.contains("安全软件"), "确证后不该再提别的可能");
    }

    #[test]
    fn 文案_拿不准时_退回并列清单() {
        let t = 失败文案("winit error: 别的什么毛病", None);
        assert!(t.contains("最常见的原因"));
        assert!(t.contains("安全软件"), "拿不准时三条可能都得留着");
        assert!(!t.contains("原因已经确认"), "没确证就别把话说死");
    }

    #[test]
    fn 文案_两种都带上原始报错和日志路径() {
        for 已知 in [None, Some(已知原因::缺OpenGL)] {
            let t = 失败文案("某某错误 42", 已知);
            // 原始报错必须原样留着 —— 它是唯一没被我们二次解读的证据
            assert!(t.contains("某某错误 42"), "原始报错被吞了：{已知:?}");
            assert!(t.contains("wrepl-gui.log"), "日志路径得给出来：{已知:?}");
            // 命令行版这条路两条都要有（很多人只有 GUI 打不开、CLI 完全能用）
            assert!(t.contains("wrepl.exe apply"), "没给命令行版的用法：{已知:?}");
        }
    }
}
