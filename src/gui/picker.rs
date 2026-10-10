//! 自绘「选择文件 / 选择文件夹 / 另存为」面板。
//!
//! ## 为什么不用系统对话框
//!
//! 本机装了企业管控套件（`AppInit_DLLs` 全局注入 + DLP 驱动），实测
//! **每新建一个 OS 窗口要交 0.8~2 秒的「窗口税」**，而系统文件对话框
//! 内部要建十几个窗口 —— 于是点一下「选目录」要等 9~10 秒。
//! 自绘面板跑在**已有的** egui 窗口里，一个 OS 窗口都不新建，这段税直接归零。
//!
//! ## 能力边界（对齐系统对话框）
//!
//! · 本地盘 —— `std::fs` 直接读
//! · **映射网盘**（`Z:` 这类）—— 文件系统层面与本地盘无异，`std::fs` 一视同仁；
//!   盘符栏会把它标成「网络」并显示它映射到哪个 UNC
//! · **局域网 UNC**（`\\server\share\...`）—— 地址栏直接粘，`std::fs` 照样读
//!
//! ## 三个必须记住的实现约束
//!
//! 1. **目录列举一律放后台线程**。网络路径 `read_dir` 要 1~3 秒，放 UI 线程
//!    就是点一下假死一次（见 [`Picker::poll`]）。
//! 2. **盘符 / 快捷位要缓存**。枚举盘符要调好几个 Win32 API，
//!    每帧都做纯属浪费（见 [`Picker::refresh_places`]）。
//! 3. **系统对话框不删**。右下角留了一个不显眼的「...」入口兜底
//!    （[`Outcome::Fallback`]），遇到自绘面板搞不定的场合能原地退回去。
//! 4. **界面是按帧重绘的**（egui 是立即模式），所以绘制路径上**不许有分配和 IO**。
//!    面包屑的分段、每行的图标类型这类"能提前算的"，一律在
//!    [`Picker::set_loc`] / 读目录那一次算好存下来；每帧只查表。
//!    同理**不去抽 Windows 的真实壳图标/缩略图** —— 那是每个文件一次
//!    GDI/COM 调用，几百个文件的目录会让打开面板肉眼可见地变慢。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui::{Align2, Color32, FontId, RichText, Sense, Vec2};

// 字号与行高统一取自 `app::ui_scale` —— 别在这里写裸数字（见那个模块的说明）。
use crate::app::ui_scale;

// ─────────────────────────── 对外接口 ───────────────────────────

/// 面板尺寸（点）。居中位置由它算出来，别在别处硬编码偏移量。
///
/// **与 0.2.2 一致（920×600）**：1000×640 那次放大跟着整体字号一起做，
/// 实际看着偏大。字号保持放大，面板回到原尺寸即可。
const PANEL_W: f32 = 920.0;
const PANEL_H: f32 = 600.0;

/// 面板要干什么。三种模式决定「确认」按钮的含义与返回什么。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 选一个**文件夹**（返回当前所在目录，或选中的那一个目录）。
    Folder,
    /// 选**若干文件**（返回选中的那些）。
    Files,
    /// **另存为**（返回 `当前目录 / 文件名框内容`）。
    Save,
}

/// 面板的出口。
#[derive(Clone, Debug)]
pub enum Outcome {
    /// 选好了。`Folder` / `Save` 一定是 1 个路径，`Files` 是 N 个。
    Picked(Vec<PathBuf>),
    /// 用户点了右下角的「...」——调用方改用 `rfd` 原生对话框。
    Fallback,
    /// 用户取消（Esc / 取消按钮）。
    Cancelled,
}

/// 一行（文件或目录）。
#[derive(Clone, Debug)]
struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
    /// Unix 秒；读不到（权限等）就是 `None`，界面显示 `—`。
    mtime: Option<i64>,
    hidden: bool,
    /// 图标类型。**在后台线程读目录那次就算好**，绘制时只是查表 ——
    /// 每帧按扩展名去 `match` 一次字符串也能跑，但有 N 行就是 N 次；
    /// 放在这里等于零成本（见 [`RowIcon`] 的说明）。
    icon: RowIcon,
}

/// 行图标类型 —— 参照 Files / Windows 资源管理器，**Office 系按各自的品牌色**，
/// 其余文件保持灰色描边文档。
///
/// 判定只看扩展名（不碰注册表、不抽系统壳图标）：真去抽 `.ico` 是每个文件一次
/// GDI/COM 调用，一个目录几百个文件就会让"打开选择器"明显变慢 —— 那条路不走。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowIcon {
    Dir,
    Word,
    Excel,
    Ppt,
    Pdf,
    Other,
}

impl RowIcon {
    fn of(name: &str) -> Self {
        let ext = match name.rsplit_once('.') {
            Some((_, e)) => e,
            None => return Self::Other,
        };
        // 不分配：`eq_ignore_ascii_case` 直接比切片。
        if ext.eq_ignore_ascii_case("docx") || ext.eq_ignore_ascii_case("doc") {
            Self::Word
        } else if ext.eq_ignore_ascii_case("xlsx") || ext.eq_ignore_ascii_case("xls") {
            Self::Excel
        } else if ext.eq_ignore_ascii_case("pptx") || ext.eq_ignore_ascii_case("ppt") {
            Self::Ppt
        } else if ext.eq_ignore_ascii_case("pdf") {
            Self::Pdf
        } else {
            Self::Other
        }
    }

    /// 实心方块的底色。`None` = 画成灰色描边文档（不做实心块）。
    fn solid(self) -> Option<Color32> {
        match self {
            Self::Word => Some(Color32::from_rgb(0x2B, 0x57, 0x9A)),
            Self::Excel => Some(Color32::from_rgb(0x21, 0x73, 0x46)),
            Self::Ppt => Some(Color32::from_rgb(0xC4, 0x3E, 0x1C)),
            Self::Pdf => Some(Color32::from_rgb(0xB0, 0x2B, 0x2B)),
            _ => None,
        }
    }

    /// 方块上一个白色字母（PDF 用三条横线，见 [`paint_row_icon`]）。
    fn letter(self) -> Option<&'static str> {
        match self {
            Self::Word => Some("W"),
            Self::Excel => Some("X"),
            Self::Ppt => Some("P"),
            _ => None,
        }
    }
}

/// 排序键。点表头切换。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Size,
    Time,
}

/// 一个盘符。**映射网盘就在这张表里**（`kind` 是「网络驱动器」），
/// 所以「网盘映射路径」这条需求不需要额外的 API。
#[derive(Clone, Debug)]
struct Drive {
    /// `"C:"` 或 `"Z: 项目盘"`。
    letter: String,
    kind: &'static str,
    /// 映射网盘才有：它指向的 UNC（`\\server\share`）。
    remote: Option<String>,
}

/// 一次后台读取。
struct Load {
    rx: mpsc::Receiver<(u64, Result<(PathBuf, Vec<Entry>), String>)>,
    /// 本次请求的序号；回来时对不上就丢弃（用户连点几下时只认最后一次）。
    seq: u64,
    dir: PathBuf,
    started: Instant,
}

/// 面板内部产生的即时动作。
enum Act {
    Close(Outcome),
}

/// 自绘文件选择器。**常驻 App** —— 跨次打开记住上次目录，窗口不会闪。
pub struct Picker {
    open: bool,
    mode: Mode,
    title: String,

    /// 扩展名过滤（小写、不含点）。空 = 不过滤。
    exts: Vec<String>,
    /// 过滤器的说法，给界面显示用。
    filter_label: String,

    // ── 导航 ──
    /// 当前所在目录（**已加载成功**的那个；加载中时仍是旧值）。
    cwd: PathBuf,
    /// 地址栏文本。与 `cwd` 分开：可以敲一半、敲错、粘 UNC 而不影响 `cwd`。
    addr: String,
    /// 地址栏是「正在手输路径」还是「面包屑」。默认面包屑；点空白处 /
    /// 右边那枚小图标切到手输（UNC 路径得靠它）。
    addr_editing: bool,
    /// 刚切进手输状态、**还需要抢一次焦点**。只在切换那一帧为真 ——
    /// 见 [`Picker::addr_bar`] 里为什么不每帧 `request_focus`。
    addr_focus: bool,
    /// 面包屑分段：`(显示名, 该段指向的路径)`。
    ///
    /// **必须缓存**：`body()` 每帧都会画它，若在绘制处现场切路径，
    /// 每帧都要新建 `Vec` 和一堆 `String`。改在 [`Picker::set_loc`] 里算一次。
    crumbs: Vec<(String, PathBuf)>,
    /// 浏览历史 + 当前位置（后退 / 前进）。
    history: Vec<PathBuf>,
    hist_idx: usize,
    /// 本次会话走过的目录（「最近使用」分区）。**只存内存**，不落盘 ——
    /// 启动时读一个历史文件就要碰磁盘，那才是真的会影响打开速度。
    recent: Vec<PathBuf>,

    // ── 内容 ──
    /// 全量条目（不过滤、不排序，保持 `read_dir` 的顺序）。
    all: Vec<Entry>,
    /// 可见条目在 `all` 里的下标，已按 `sort` / `sort_desc` 排好。
    view: Vec<usize>,
    /// 选中的**可见行号**（不是 `all` 的下标）。
    sel: BTreeSet<usize>,
    /// Shift 连选的锚点（可见行号）。
    anchor: Option<usize>,

    file_name: String,
    sort: SortKey,
    sort_desc: bool,
    show_hidden: bool,

    /// 加载中。
    load: Option<Load>,
    /// 上一次读取失败的原因（加载成功即清空）。
    err: Option<String>,

    /// 上次停留的目录 —— 下次 `open` 没给起点时用它，跨次打开有连续性。
    last_dir: PathBuf,

    /// 盘符与快捷位的缓存（见模块头注释第 2 条）。
    drives_cache: Vec<Drive>,
    places_cache: Vec<(&'static str, PathBuf)>,
}

impl Default for Picker {
    fn default() -> Self {
        Self::new()
    }
}

impl Picker {
    pub fn new() -> Self {
        let home = home_dir();
        let mut p = Self {
            open: false,
            mode: Mode::Files,
            title: String::new(),
            exts: Vec::new(),
            filter_label: String::new(),
            cwd: home.clone(),
            addr: String::new(),
            addr_editing: false,
            addr_focus: false,
            crumbs: Vec::new(),
            history: Vec::new(),
            hist_idx: 0,
            recent: Vec::new(),
            all: Vec::new(),
            view: Vec::new(),
            sel: BTreeSet::new(),
            anchor: None,
            file_name: String::new(),
            sort: SortKey::Name,
            sort_desc: false,
            show_hidden: false,
            load: None,
            err: None,
            last_dir: home,
            drives_cache: Vec::new(),
            places_cache: Vec::new(),
        };
        p.refresh_places();
        p
    }

    /// 打开面板。
    ///
    /// * `start` —— 起点；空则用上次停留的目录。给文件则退到它所在目录。
    /// * `exts` —— 扩展名白名单（不含点，如 `["docx"]`）；空 = 全部文件。
    /// * `default_name` —— `Save` 模式预填的文件名。
    pub fn open(
        &mut self,
        mode: Mode,
        title: impl Into<String>,
        start: PathBuf,
        exts: &[&str],
        default_name: &str,
    ) {
        self.mode = mode;
        self.title = title.into();
        self.exts = exts
            .iter()
            .map(|s| {
                s.trim_start_matches('*')
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
            })
            .collect();
        self.filter_label = if self.exts.is_empty() {
            "全部文件".to_string()
        } else {
            format!(
                "筛选：{}",
                self.exts
                    .iter()
                    .map(|e| format!("*.{e}"))
                    .collect::<Vec<_>>()
                    .join("  ")
            )
        };
        self.file_name = default_name.to_string();
        self.err = None;
        self.sel.clear();
        self.anchor = None;
        // 每次打开都从"面包屑"开始 —— 上次可能停在手输状态，留着会让人以为坏了。
        self.addr_editing = false;
        self.open = true;
        // 盘符可能刚刚挂载/断开（插 U 盘、映射网盘），每次打开重扫一遍。
        self.refresh_places();

        let dir = if start.as_os_str().is_empty() {
            self.last_dir.clone()
        } else if start.is_dir() {
            start
        } else if start.is_file() {
            start
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(home_dir)
        } else {
            // 路径不存在（常见于「输出目录还没建」）：退到它的父目录
            start
                .parent()
                .filter(|p| p.is_dir())
                .map(Path::to_path_buf)
                .unwrap_or_else(home_dir)
        };
        crate::diag::log(format!(
            "打开选择器：模式={mode:?} 标题={:?} 筛选={:?} 起点={}",
            self.title,
            self.exts,
            dir.display()
        ));
        self.navigate(dir);
    }

    /// 每帧调用。面板关着时返回 `None`；有结果时返回一次并自动关闭。
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Outcome> {
        if !self.open {
            return None;
        }
        self.poll(ctx);

        // Esc 取消。Enter 的判定在面板内部 —— 要避开地址栏 / 文件名框的回车。
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.open = false;
            self.load = None;
            return Some(Outcome::Cancelled);
        }

        let screen = ctx.screen_rect();
        let mut act: Option<Act> = None;

        // ① 遮罩：吞掉落到底下主界面的点击（不然面板开着还能点到后面的按钮）。
        //    `Order::Middle` 高于页面（`Background`）、低于面板（`Foreground`），
        //    正好夹在中间。刻意用**不同 Layer** 而不用同层绘制顺序定序 —— 后者不稳。
        egui::Area::new(egui::Id::new("wrepl-picker-veil"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .interactable(true)
            .show(ctx, |ui| {
                ui.allocate_response(screen.size(), Sense::click_and_drag());
                ui.painter()
                    .rect_filled(screen, 0.0, Color32::from_black_alpha(100));
            });

        // ② 面板本体。`all` 先 take 出来，免得闭包里同时借 `&mut self` 和 `&self.all`。
        //    尺寸用常量，居中偏移由它算出来 —— 否则改了宽度忘了改那个 `-470.0`，
        //    面板就会偏在屏幕一边（宽度一改，居中量就跟着变）。
        let entries = std::mem::take(&mut self.all);
        egui::Area::new(egui::Id::new("wrepl-picker"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                (screen.center().x - PANEL_W / 2.0).max(8.0),
                (screen.center().y - PANEL_H / 2.0).max(8.0),
            ))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(12.0))
                    .show(ui, |ui| {
                        ui.set_width(PANEL_W);
                        ui.set_height(PANEL_H);
                        act = self.body(ui, ctx, &entries);
                    });
            });
        self.all = entries;

        match act {
            Some(Act::Close(o)) => {
                crate::diag::log(format!("选择器关闭：{o:?}"));
                self.open = false;
                self.load = None;
                Some(o)
            }
            None => None,
        }
    }

    /// 重扫盘符与快捷位（只在 `new` / `open` 时调）。
    fn refresh_places(&mut self) {
        self.drives_cache = drives();
        self.places_cache = quick_places();
    }

    /// 收后台线程的结果（不阻塞）。
    fn poll(&mut self, ctx: &egui::Context) {
        let Some(load) = &self.load else { return };
        // 加载中定时叫醒：网络路径慢，不主动重绘的话转圈会停住。
        ctx.request_repaint_after(Duration::from_millis(80));

        match load.rx.try_recv() {
            Ok((seq, res)) => {
                if seq != load.seq {
                    return; // 过期结果（用户又点走了），丢弃，保持加载态
                }
                match res {
                    Ok((dir, list)) => {
                        crate::diag::log(format!("读取完成：{} 项  {}", list.len(), dir.display()));
                        self.all = list;
                        self.set_loc(&dir);
                        self.last_dir = dir.clone();
                        self.err = None;
                        // 历史与「最近使用」只在**读成功**之后记 —— 路径不存在时
                        // 读会失败，那时不该往历史里塞一条点不回来的记录。
                        self.push_history(dir.clone());
                        self.push_recent(dir);
                        // 这里 self.all 就是权威数据，本可以直接 `self.refresh_view(&self.all)`，
                        // 但那会同时借 `&mut self` 和 `&self.all`（同一个 self）——借不过去。
                        // 所以就地展开；排序/过滤仍走同一个 build_view，保证与面板内一致。
                        let view = Self::build_view(
                            &self.all,
                            &self.exts,
                            self.show_hidden,
                            self.sort,
                            self.sort_desc,
                        );
                        self.view = view;
                        self.sel.clear();
                        self.anchor = None;
                    }
                    Err(e) => {
                        // 读不到就停在原地，把原因说清楚（权限 / 网络 / 路径不存在）
                        crate::diag::log(format!("读取失败：{e}"));
                        self.err = Some(e);
                        // 回到"真的在这儿"的位置：`navigate` 是乐观更新的，失败就得撤。
                        self.set_loc(&self.cwd.clone());
                    }
                }
                self.load = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.load = None;
                self.err = Some("读取线程意外结束".to_string());
            }
        }
    }

    /// 把「当前在哪」这组展示状态一次性写好：`cwd` / 地址栏文本 / 面包屑分段。
    ///
    /// 面包屑**只在这里算**（每帧几十次 `String` 分配是没必要的开销），
    /// 绘制时直接读 `self.crumbs`。见结构体上 `crumbs` 字段的说明。
    fn set_loc(&mut self, dir: &Path) {
        self.cwd = dir.to_path_buf();
        self.addr = dir.display().to_string();
        self.crumbs = crumbs_of(dir);
    }

    /// 记一条浏览历史。**同一目录不重复记**（后退/前进落回原地时不该再压栈）。
    fn push_history(&mut self, dir: PathBuf) {
        if self.history.get(self.hist_idx) == Some(&dir) {
            return;
        }
        self.history.truncate(self.hist_idx + 1);
        self.history.push(dir);
        // 上限 64 条：够用，且不会因为长时间乱逛无限长大。
        if self.history.len() > 64 {
            self.history.remove(0);
        }
        self.hist_idx = self.history.len() - 1;
    }

    /// 记一条「最近使用」，新的在前、去重、最多 5 条。
    fn push_recent(&mut self, dir: PathBuf) {
        self.recent.retain(|p| p != &dir);
        self.recent.insert(0, dir);
        self.recent.truncate(5);
    }

    fn can_back(&self) -> bool {
        self.hist_idx > 0
    }

    fn can_forward(&self) -> bool {
        self.hist_idx + 1 < self.history.len()
    }

    fn go_back(&mut self) {
        if self.can_back() {
            self.hist_idx -= 1;
            let d = self.history[self.hist_idx].clone();
            // 走「不压栈」的路径，否则后退会被当成新导航、把前进记录截断。
            self.navigate_hist(d);
        }
    }

    fn go_forward(&mut self) {
        if self.can_forward() {
            self.hist_idx += 1;
            let d = self.history[self.hist_idx].clone();
            self.navigate_hist(d);
        }
    }

    /// 跳到某个目录（**不在 UI 线程上读**）。
    fn navigate(&mut self, dir: PathBuf) {
        // 展示状态**乐观更新**：面包屑与地址栏立刻跟着走，不等读目录回来。
        // 读失败时 `poll()` 会把 `set_loc` 撤回到真正所在的目录。
        self.set_loc(&dir);
        self.navigate_hist(dir);
    }

    /// `navigate` 的"不动展示状态"版本 —— 后退 / 前进用它。
    fn navigate_hist(&mut self, dir: PathBuf) {
        crate::diag::log(format!("导航 → {}", dir.display()));
        let seq = self.load.as_ref().map_or(1, |l| l.seq + 1);
        let (tx, rx) = mpsc::channel();
        let d = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send((seq, read_dir_entries(&d)));
        });
        self.load = Some(Load {
            rx,
            seq,
            dir,
            started: Instant::now(),
        });
        // 旧内容立刻清掉并显示「正在读取」：网络目录下留着上一处的内容会误导人。
        self.all.clear();
        self.view.clear();
        self.sel.clear();
        self.anchor = None;
    }

    fn reload(&mut self) {
        let d = self.cwd.clone();
        self.navigate_hist(d);
    }

    fn go_up(&mut self) {
        if let Some(p) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(p);
        }
    }

    /// 切到目录树里的某一段（面包屑 / 左侧导航）。与 [`Self::navigate`] 同义，
    /// 单独留个名字只是为了调用点读起来清楚。
    fn jump(&mut self, dir: PathBuf) {
        self.navigate(dir);
    }

    /// 按当前过滤 + 排序算出「可见条目在 `all` 里的下标」。
    ///
    /// ⚠️ 刻意写成**关联函数**（数据从 `entries` 参数进）而不是 `&mut self` 方法，
    /// 因为调用点分两种完全不同的处境：
    ///
    /// * `poll()` 里 —— `self.all` 是权威数据；
    /// * `body()` 里 —— `self.all` 已经被 [`Picker::show`] 借出去给绘制用了，
    ///   那一刻**它是空的**。
    ///
    /// 做成方法就必然有一边对着空表算。v0.2.0「点一下就退出」正是这么来的：
    /// 原来这里只吃 `self.all`，面板打开期间被调用就得到空视图 / 越界。**
    fn build_view(
        entries: &[Entry],
        exts: &[String],
        show_hidden: bool,
        sort: SortKey,
        sort_desc: bool,
    ) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..entries.len())
            .filter(|&i| {
                let e = &entries[i];
                (show_hidden || !e.hidden) && (e.is_dir || ext_ok(exts, &e.name))
            })
            .collect();

        idx.sort_by(|&a, &b| {
            let (ea, eb) = (&entries[a], &entries[b]);
            // 目录永远排在文件前面，这一层不受 asc/desc 影响
            if ea.is_dir != eb.is_dir {
                return if ea.is_dir {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            let o = match sort {
                SortKey::Name => ea.name.to_lowercase().cmp(&eb.name.to_lowercase()),
                SortKey::Size => ea.size.cmp(&eb.size),
                SortKey::Time => ea.mtime.cmp(&eb.mtime),
            };
            if sort_desc { o.reverse() } else { o }
        });
        idx
    }

    /// 就地刷新视图，并清掉选中（行号会变，留着旧行号会指错人）。
    /// `entries` 该传哪一份，见 [`Picker::build_view`] 上面的说明。
    fn refresh_view(&mut self, entries: &[Entry]) {
        self.view = Self::build_view(
            entries,
            &self.exts,
            self.show_hidden,
            self.sort,
            self.sort_desc,
        );
        self.sel.clear();
        self.anchor = None;
    }

    /// 确认。返回 `None` 表示当前状态下没什么可确认（按钮会置灰）。
    ///
    /// ⚠️ 数据从 `entries` 参数进，**不碰 `self.all`** —— 原因见
    /// [`Picker::build_view`]：本方法会在 `body()` 里被调用，那时 `self.all`
    /// 已经被借走、是空的。v0.2.0 的崩溃就出在这里（原来写的是
    /// `&self.all[i]`，而 `view` 里的下标还指着被借走的那份表 → 越界）。
    ///
    /// 另外一律走 `entries.get(i)` 而不是 `entries[i]`：视图下标与数据表
    /// 万一对不上（用户在面板开着时换了目录等），也只该"这一项不算数"，
    /// 而不是把整个程序带走。
    fn confirm(&self, entries: &[Entry]) -> Option<Vec<PathBuf>> {
        match self.mode {
            Mode::Folder => {
                // 恰好选中一个目录 → 选它；否则 → 当前所在目录
                let picked: Vec<PathBuf> = self
                    .sel
                    .iter()
                    .filter_map(|&v| self.view.get(v))
                    .filter_map(|&i| entries.get(i))
                    .filter(|e| e.is_dir)
                    .map(|e| self.cwd.join(&e.name))
                    .collect();
                if picked.len() == 1 {
                    Some(picked)
                } else {
                    Some(vec![self.cwd.clone()])
                }
            }
            Mode::Files => {
                let files: Vec<PathBuf> = self
                    .sel
                    .iter()
                    .filter_map(|&v| self.view.get(v))
                    .filter_map(|&i| entries.get(i))
                    .filter(|e| !e.is_dir)
                    .map(|e| self.cwd.join(&e.name))
                    .collect();
                if !files.is_empty() {
                    return Some(files);
                }
                // 一个都没选，但文件名框里写了东西 → 认它（与系统对话框一致）
                let typed = self.file_name.trim();
                if typed.is_empty() {
                    None
                } else {
                    let p = resolve_typed(&self.cwd, typed);
                    if p.is_file() { Some(vec![p]) } else { None }
                }
            }
            Mode::Save => {
                let typed = self.file_name.trim();
                if typed.is_empty() {
                    None
                } else {
                    Some(vec![resolve_typed(&self.cwd, typed)])
                }
            }
        }
    }

    // ─────────────────────────── 界面 ───────────────────────────

    fn body(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context, entries: &[Entry]) -> Option<Act> {
        let mut act: Option<Act> = None;
        let mut jump: Option<PathBuf> = None;

        let full_h = ui.available_height();
        let side_w = ui_scale::PICK_SIDE_W;
        let pal = palette(ui);

        ui.horizontal_top(|ui| {
            // ① 左导航（固定宽）。底色在这里画：`nav_side` 只负责内容，
            //    绘制顺序决定 Z 序 —— 先铺底再摆行，字不会被盖住。
            let side_rect =
                egui::Rect::from_min_size(ui.cursor().min, Vec2::new(side_w, full_h));
            ui.painter().rect_filled(side_rect, 6.0, pal.side);
            ui.allocate_ui_with_layout(
                Vec2::new(side_w, full_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    jump = self.nav_side(ui);
                },
            );

            ui.add_space(12.0);

            // ② 右内容（吃掉剩下的宽度）
            let w = ui.available_width();
            ui.allocate_ui_with_layout(
                Vec2::new(w, full_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    act = self.content(ui, entries);
                },
            );
        });

        // 跳转放在闭包外面做：闭包里 `self` 已经被借给 `nav_side` / `content` 了。
        if let Some(p) = jump {
            self.jump(p);
        }
        act
    }

    /// 左侧导航栏：标题 + 「分区标题 / 图标行」两级。
    ///
    /// 数据全部来自**已缓存**的 `drives_cache` / `places_cache`（外加内存里的
    /// `recent`）—— 这里不扫盘、不碰文件系统，见模块头第 2、4 条约束。
    /// 返回被点中的目标目录。
    fn nav_side(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        let pal = palette(ui);
        let mut pick: Option<PathBuf> = None;

        // 标题：原来横贯面板顶部一整行，现在收进侧栏 —— 内容区因此多出一行的高度。
        let title = self.title.clone();
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(
                RichText::new(title)
                    .size(ui_scale::SMALL + 1.5)
                    .strong()
                    .color(DIR_C),
            );
        });
        ui.add_space(12.0);

        let cwd = self.cwd.clone();
        let places = self.places_cache.clone();
        let drives_v = self.drives_cache.clone();
        let recent = self.recent.clone();
        // 当前目录的归一化文本（去尾部 `\`）。算一次，行内只做比较 ——
        // 每行都 `display()` 一遍会有十几次小分配，没必要（见模块头第 4 条）。
        let cwd_s = cwd.display().to_string();
        let cur_norm = cwd_s.trim_end_matches(['\\', '/']);

        // ── 快速访问 ──
        nav_section(ui, "快速访问");
        for &(label, ref p) in &places {
            // 图标随位置变：用户 / 桌面 / 文档 / 下载各一个字形与颜色。
            let (glyph, color) = match label {
                "用户" => (NavGlyph::Home, 0x378ADD),
                "桌面" => (NavGlyph::Desktop, 0x7F77DD),
                "文档" => (NavGlyph::Doc, 0x185FA5),
                _ => (NavGlyph::Download, 0x1D9E75),
            };
            if nav_row(
                ui,
                &pal,
                glyph,
                Color32::from_rgb(
                    (color >> 16) as u8,
                    ((color >> 8) & 0xFF) as u8,
                    (color & 0xFF) as u8,
                ),
                label,
                same_dir(cur_norm, p),
                &p.display().to_string(),
            ) {
                pick = Some(p.clone());
            }
        }

        // ── 此电脑（盘符）── 映射网盘单独标出它指向的 UNC。
        ui.add_space(8.0);
        nav_section(ui, "此电脑");
        for d in &drives_v {
            let target = PathBuf::from(format!("{}\\", letter_of(&d.letter)));
            let (glyph, color, tip) = match &d.remote {
                Some(r) => (NavGlyph::Net, Color32::from_rgb(0x0F, 0x6E, 0x56), format!("映射到 {r}")),
                None => (
                    NavGlyph::Drive,
                    Color32::from_rgb(0x5F, 0x5E, 0x5A),
                    d.kind.to_string(),
                ),
            };
            let short = drive_short(&d.letter);
            if nav_row(ui, &pal, glyph, color, &short, same_dir(cur_norm, &target), &tip) {
                pick = Some(target);
            }
        }

        // ── 最近使用（只记本次会话，不落盘）──
        //    先滤掉当前目录再决定要不要出这个小节 —— 否则首次打开时
        //    「最近使用」里只有当前目录这一条，滤完就剩一个**空标题**。
        let others: Vec<&PathBuf> = recent.iter().filter(|p| !same_dir(cur_norm, p)).collect();
        if !others.is_empty() {
            ui.add_space(8.0);
            nav_section(ui, "最近使用");
            for p in others {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string());
                if nav_row(
                    ui,
                    &pal,
                    NavGlyph::Clock,
                    Color32::from_rgb(0x88, 0x87, 0x80),
                    &name,
                    false,
                    &p.display().to_string(),
                ) {
                    pick = Some(p.clone());
                }
            }
        }

        pick
    }

    /// 地址栏。两档：
    ///
    /// * **面包屑**（默认）—— 路径按 `\` 分段，点哪段跳哪段；太长时从前面省略，
    ///   保留离当前目录最近的几段。点空白处或右端那枚小图标切到手输。
    /// * **手输**（`addr_editing`）—— 一个普通文本框。这一档必须留：
    ///   局域网 UNC（`\\server\share`）只能敲进来或粘进来。
    fn addr_bar(&mut self, ui: &mut egui::Ui, enter_used: &mut bool) {
        let w = ui.available_width();
        let h = ui_scale::EDIT_H;

        if self.addr_editing {
            let resp = ui.add_sized(
                [w, h],
                egui::TextEdit::singleline(&mut self.addr)
                    .hint_text(r"目录，或 \\服务器\共享\子目录"),
            );
            // 只在刚切进来的那一帧抢焦点。每帧都 `request_focus` 会和"点别处"
            // 打架：egui 先把焦点交出去、我们下一帧又抢回来，文本框就摘不掉了。
            //
            // 抢到焦点这一帧**直接返回**：不然后面那句 `lost_focus` 判定的
            // 是"焦点还没到手"的状态，刚切成手输就被判成"失去焦点"退出去了。
            if self.addr_focus {
                resp.request_focus();
                self.addr_focus = false;
                return;
            }
            // 回车 = 转到（**吃掉这次 Enter**，不再触发底部的确认）
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                *enter_used = true;
                let t = self.addr.trim().to_string();
                self.addr_editing = false;
                if !t.is_empty() {
                    self.jump(PathBuf::from(t));
                }
            } else if resp.lost_focus() {
                // 焦点走掉（点了别处）就退回面包屑，并把没提交的草稿丢掉
                self.addr_editing = false;
                self.addr = self.cwd.display().to_string();
            }
            return;
        }

        let pal = palette(ui);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
        ui.painter()
            .rect_filled(rect, 5.0, pal.bg);
        ui.painter().rect_stroke(
            rect,
            5.0,
            egui::Stroke::new(1.0_f32, pal.border),
        );

        let font = FontId::proportional(ui_scale::SMALL + 1.0);
        let txt_c = ui.visuals().text_color();

        // 先量宽（一次），再决定要不要从前面省略。
        let n = self.crumbs.len();
        let mut widths: Vec<f32> = Vec::with_capacity(n);
        for (i, (name, _)) in self.crumbs.iter().enumerate() {
            let col = if i + 1 == n { DIR_C } else { txt_c };
            let g = ui.painter().layout_no_wrap(name.clone(), font.clone(), col);
            widths.push(g.size().x);
        }

        /// 段与段之间的分隔箭头占的宽。
        const CHEV: f32 = 15.0;
        /// 每段左右各留的内边距合计。
        const PAD: f32 = 12.0;
        let avail = (rect.width() - 40.0).max(60.0);
        let mut from = 0usize;
        loop {
            let mut total = if from > 0 { 20.0 } else { 0.0 }; // 省出来的那个「…」
            for x in &widths[from..] {
                total += x + PAD + CHEV;
            }
            if total <= avail || from + 2 >= n {
                break;
            }
            from += 1;
        }

        let mut x = rect.left() + 8.0;
        let cy = rect.center().y;
        let mut hit: Option<PathBuf> = None;
        if from > 0 {
            ui.painter().text(
                egui::pos2(x + 2.0, cy),
                Align2::LEFT_CENTER,
                "…",
                font.clone(),
                DIM,
            );
            x += 18.0;
        }
        for i in from..n {
            let last = i + 1 == n;
            let col = if last { DIR_C } else { txt_c };
            let bw = widths[i] + PAD;
            let brect = egui::Rect::from_min_size(
                egui::pos2(x, rect.top() + 2.0),
                Vec2::new(bw, rect.height() - 4.0),
            );
            // 自绘的段没有 egui 自动生成的 id，得自己给一个（还要唯一）。
            let cr = ui
                .interact(brect, ui.id().with(("crumb", i)), Sense::click())
                .on_hover_text(self.crumbs[i].1.display().to_string());
            if cr.hovered() {
                ui.painter().rect_filled(brect, 3.0, pal.hover);
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            ui.painter().text(
                brect.center(),
                Align2::CENTER_CENTER,
                &self.crumbs[i].0,
                font.clone(),
                col,
            );
            if cr.clicked() {
                hit = Some(self.crumbs[i].1.clone());
            }
            x += bw;
            if !last {
                ui.painter().add(egui::Shape::line(
                    vec![
                        egui::pos2(x + 4.5, cy - 3.0),
                        egui::pos2(x + 7.5, cy),
                        egui::pos2(x + 4.5, cy + 3.0),
                    ],
                    egui::Stroke::new(1.2_f32, DIM),
                ));
                x += CHEV;
            }
        }

        // 右端的「编辑路径」小按钮（铅笔）。UNC 路径就是从这儿进去敲的。
        let ebr = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 26.0, rect.top() + 3.0),
            Vec2::new(22.0, rect.height() - 6.0),
        );
        let er = ui
            .interact(ebr, ui.id().with("crumb-edit"), Sense::click())
            .on_hover_text("输入路径（局域网 UNC 粘这里）");
        if er.hovered() {
            ui.painter().rect_filled(ebr, 3.0, pal.hover);
        }
        paint_pencil(ui.painter(), ebr.center(), er.hovered());

        let mut edit = er.clicked();
        // 点在段之间的空白上 = 直接进编辑（与资源管理器一致）
        if resp.clicked() && hit.is_none() {
            edit = true;
        }
        if edit {
            self.addr_editing = true;
            self.addr_focus = true;
        }
        if let Some(p) = hit {
            self.jump(p);
        }
    }

    /// 右侧内容区：工具栏（导航 + 地址栏）→ 表头 → 列表 → 状态栏 → 动作条。
    fn content(&mut self, ui: &mut egui::Ui, entries: &[Entry]) -> Option<Act> {
        let mut act: Option<Act> = None;
        let mut enter_used = false;

        // ① 工具栏：四枚自绘导航图标 + 地址栏，同占一行（Files 也是这么摆的）。
        ui.horizontal(|ui| {
            if nav_button(ui, NavIcon::Back, self.can_back(), "后退").clicked() {
                self.go_back();
            }
            if nav_button(ui, NavIcon::Forward, self.can_forward(), "前进").clicked() {
                self.go_forward();
            }
            let up_ok = self.cwd.parent().is_some();
            if nav_button(ui, NavIcon::Up, up_ok, "上一级").clicked() {
                self.go_up();
            }
            if nav_button(ui, NavIcon::Refresh, true, "刷新（网络路径可能较慢）").clicked() {
                self.reload();
            }
            ui.add_space(4.0);
            self.addr_bar(ui, &mut enter_used);
        });
        ui.add_space(8.0);

        // ② 表头（可点：切换排序）
        let row_h = ui_scale::PICK_ROW_H;
        let full_w = ui.available_width();
        let col_size = 92.0;
        let col_time = 132.0;
        let name_w = (full_w - col_size - col_time - 30.0).max(120.0);
        let head_h = ui_scale::PICK_ROW_H;

        let mut sort_click: Option<SortKey> = None;
        let pal = palette(ui);
        // 表头底纹：先占一个空绘制槽，等表头排完版、知道它实际占了多高，
        // 再回填成一块底色 —— 底色的 Z 序按 `add` 的时间点算，所以仍在按钮**下面**。
        // （直接先画会画错高度：表头高度得排完版才知道。）
        let head_shape = ui.painter().add(egui::Shape::Noop);
        let head_top = ui.cursor().min;
        ui.horizontal(|ui| {
            // 表头**自己画**，不用 Button：Button 会把文字摆在自己正中，
            // 于是「名称」比下面的文件名缩进半个列宽（字号小的时候看不出来，
            // 放大之后一眼就不齐）。这里三格与数据走同一条基准线：
            // 名称左对齐到 `NAME_DX`、大小右对齐、时间左对齐。
            let name_r = ui
                .allocate_response(Vec2::new(name_w, head_h), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let size_r = ui
                .allocate_response(Vec2::new(col_size, head_h), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let time_r = ui
                .allocate_response(Vec2::new(col_time, head_h), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let p = ui.painter();
            // 悬停的那一列压一层淡底（Files 的表头就是这样提示"这列能点"）。
            for r in [&name_r, &size_r, &time_r] {
                if r.hovered() {
                    p.rect_filled(r.rect, 3.0, pal.hover);
                }
            }
            let f = FontId::proportional(ui_scale::SMALL);
            let (n_txt, s_txt, t_txt) = ("名称", "大小", "修改时间");
            p.text(
                egui::pos2(name_r.rect.left() + NAME_DX, name_r.rect.center().y),
                Align2::LEFT_CENTER,
                n_txt,
                f.clone(),
                DIM,
            );
            p.text(
                egui::pos2(size_r.rect.right() - 10.0, size_r.rect.center().y),
                Align2::RIGHT_CENTER,
                s_txt,
                f.clone(),
                DIM,
            );
            p.text(
                egui::pos2(time_r.rect.left() + 8.0, time_r.rect.center().y),
                Align2::LEFT_CENTER,
                t_txt,
                f.clone(),
                DIM,
            );
            // 排序指示：**自己画一个小三角**，不用 `↑`/`↓` 字符 ——
            // 字符的字重跟着字体走，且 `▴`/`▾`(U+25B4/BE) 在本机是豆腐块
            // （表头会显示成「名称 □」）。画出来的三角跨机器都长一样。
            let tri = |cx: f32, cy: f32, up: bool| {
                let (a, b) = (4.0_f32, 2.6_f32);
                let pts = if up {
                    vec![
                        egui::pos2(cx - a, cy + b),
                        egui::pos2(cx + a, cy + b),
                        egui::pos2(cx, cy - b),
                    ]
                } else {
                    vec![
                        egui::pos2(cx - a, cy - b),
                        egui::pos2(cx + a, cy - b),
                        egui::pos2(cx, cy + b),
                    ]
                };
                p.add(egui::Shape::convex_polygon(pts, DIR_C, egui::Stroke::NONE));
            };
            // 三角摆在各自列标题的右侧（名称列按文字宽估个位）。
            let w_txt = |s: &str, ui: &egui::Ui| -> f32 {
                ui.painter()
                    .layout_no_wrap(s.to_string(), FontId::proportional(ui_scale::SMALL), DIM)
                    .size()
                    .x
            };
            let cy = name_r.rect.center().y;
            if self.sort == SortKey::Name {
                tri(
                    name_r.rect.left() + NAME_DX + w_txt(n_txt, ui) + 10.0,
                    cy,
                    !self.sort_desc,
                );
            }
            if self.sort == SortKey::Size {
                tri(size_r.rect.right() - 10.0 + 12.0, cy, !self.sort_desc);
            }
            if self.sort == SortKey::Time {
                tri(
                    time_r.rect.left() + 8.0 + w_txt(t_txt, ui) + 10.0,
                    cy,
                    !self.sort_desc,
                );
            }
            for (r, k) in [
                (name_r, SortKey::Name),
                (size_r, SortKey::Size),
                (time_r, SortKey::Time),
            ] {
                if r.clicked() {
                    sort_click = Some(k);
                }
            }
        });
        ui.painter().set(
            head_shape,
            egui::Shape::rect_filled(
                egui::Rect::from_min_max(head_top, egui::pos2(head_top.x + full_w, ui.cursor().min.y)),
                3.0,
                pal.head,
            ),
        );
        if let Some(k) = sort_click {
            if self.sort == k {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort = k;
                self.sort_desc = false;
            }
            // 数据要传 entries：此刻 self.all 是被 show() 借走的空壳
            self.refresh_view(entries);
        }

        ui.separator();

        // ④ 列表
        //    扣掉的 100 是**下面还要摆的东西**：状态栏 + 分隔线 + 底部动作条。
        let list_h = (ui.available_height() - 100.0).max(120.0);
        // 列表底板 + 外框。**必须在内容之前画**（Z 序 = 绘制顺序），
        // 否则这块实心底会把行里的文字整个盖住。
        let list_rect = egui::Rect::from_min_size(ui.cursor().min, Vec2::new(full_w, list_h));
        ui.painter().rect_filled(list_rect, 4.0, pal.bg);
        ui.painter().rect_stroke(
            list_rect,
            4.0,
            egui::Stroke::new(1.0_f32, pal.border),
        );
        let loading = self
            .load
            .as_ref()
            .map(|l| (l.dir.display().to_string(), l.started.elapsed().as_secs_f32()));
        if let Some((dir_txt, secs)) = loading {
            ui.allocate_ui(Vec2::new(full_w, list_h), |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((list_h / 2.0 - 30.0).max(0.0));
                    ui.spinner();
                    ui.add_space(6.0);
                    let note = if secs > 1.5 {
                        "（网络路径或休眠盘符会慢，请稍候）"
                    } else {
                        ""
                    };
                    ui.label(RichText::new(format!("正在读取 {dir_txt}{note}")).small());
                });
            });
        } else {
            // (可见行号, 是否双击, ctrl, shift)
            let mut clicked: Option<(usize, bool, bool, bool)> = None;
            if self.view.is_empty() {
                // 空目录：给一句话，否则列表底板就是一整块空白，看着像没读出来。
                let msg = if self.show_hidden || self.exts.is_empty() {
                    "此文件夹是空的"
                } else {
                    "此文件夹里没有符合条件的文件"
                };
                ui.allocate_ui(Vec2::new(full_w, list_h), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((list_h / 2.0 - 14.0).max(0.0));
                        ui.label(RichText::new(msg).small().color(DIM));
                    });
                });
            } else {
                egui::ScrollArea::vertical()
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, self.view.len(), |ui, range| {
                        let txt_c = ui.visuals().text_color();
                        // 图标高度跟着行高走 —— 改行高时图标不会脱节。
                        let icon_h = (row_h * 0.62).max(12.0);
                        for row in range {
                            let ei = self.view[row];
                            let e = &entries[ei];
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::new(full_w, row_h), Sense::click());
                            if !ui.is_rect_visible(rect) {
                                continue;
                            }
                            // 行底色四周内缩 1 px：不去压列表外框那一条边线。
                            let bg = rect.shrink2(egui::vec2(1.0, 0.5));
                            let selected = self.sel.contains(&row);
                            let p = ui.painter();
                            if selected {
                                p.rect_filled(bg, 4.0, pal.sel);
                                // 左侧强调条：选中的是哪几行一眼能认出来。
                                p.rect_filled(
                                    egui::Rect::from_min_size(
                                        bg.left_top(),
                                        Vec2::new(3.0, bg.height()),
                                    ),
                                    1.5,
                                    DIR_C,
                                );
                            } else if resp.hovered() {
                                p.rect_filled(bg, 4.0, pal.hover);
                            } else if row % 2 == 1 {
                                p.rect_filled(bg, 0.0, pal.zebra);
                            }

                            let f_name = FontId::proportional(ui_scale::PICK_NAME);
                            let f_meta = FontId::proportional(ui_scale::PICK_META);
                            let cy = rect.center().y;
                            // 图标列定宽 → 目录与文件的**文件名左边缘对齐**。
                            // 目录不再靠结尾的 `\` 表示（图标已经说明），那样对齐才干净。
                            match e.icon {
                                RowIcon::Dir => {
                                    paint_dir_icon(p, rect.left() + ICON_DX, cy, icon_h)
                                }
                                other => {
                                    paint_row_icon(p, rect.left() + ICON_DX, cy, icon_h, other, DIM)
                                }
                            }
                            let color = if e.is_dir { DIR_C } else { txt_c };
                            p.text(
                                egui::pos2(rect.left() + NAME_DX, cy),
                                Align2::LEFT_CENTER,
                                &e.name,
                                f_name,
                                color,
                            );
                            if !e.is_dir {
                                // 大小**右对齐** —— 位数不同也能对齐，与系统资源管理器一致。
                                p.text(
                                    egui::pos2(rect.left() + name_w + col_size - 10.0, cy),
                                    Align2::RIGHT_CENTER,
                                    fmt_size(e.size),
                                    f_meta.clone(),
                                    DIM,
                                );
                            }
                            if let Some(t) = e.mtime {
                                p.text(
                                    egui::pos2(rect.left() + name_w + col_size + 8.0, cy),
                                    Align2::LEFT_CENTER,
                                    fmt_mtime(t),
                                    f_meta,
                                    DIM,
                                );
                            }

                            if resp.clicked() {
                                let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
                                let shift = ui.input(|i| i.modifiers.shift);
                                clicked = Some((row, resp.double_clicked(), ctrl, shift));
                            }
                        }
                    });
            }

            // 诊断开关：这台机器上合成鼠标输入到不了窗口（`SendInput` 与
            // `PostMessage` 都被管控套件拦掉），要复现"点一下"只能让程序自己做。
            // 不设 `WREPL_DEBUG_CLICK` 时这一整段空转，不影响正常使用。
            if clicked.is_none() && !self.view.is_empty() {
                if let Some((row, dbl)) = crate::diag::debug_click() {
                    let row = row.min(self.view.len() - 1);
                    crate::diag::log(format!("[诊断] 模拟点击第 {row} 行（双击={dbl}）"));
                    clicked = Some((row, dbl, false, false));
                }
            }

            if let Some((row, dbl, ctrl, shift)) = clicked {
                let ei = self.view[row];
                let is_dir = entries[ei].is_dir;
                let name = entries[ei].name.clone();
                crate::diag::log(format!(
                    "点击行 {row}：{name:?} 是目录={is_dir} 双击={dbl} ctrl={ctrl} shift={shift}"
                ));

                if dbl {
                    if is_dir {
                        self.navigate(self.cwd.join(&name));
                        return act;
                    }
                    // 双击文件 = 直接确认
                    self.sel.clear();
                    self.sel.insert(row);
                    if let Some(paths) = self.confirm(entries) {
                        return Some(Act::Close(Outcome::Picked(paths)));
                    }
                } else if shift {
                    if let Some(a) = self.anchor {
                        let (lo, hi) = (a.min(row), a.max(row));
                        self.sel.clear();
                        for r in lo..=hi {
                            self.sel.insert(r);
                        }
                    } else {
                        self.sel.clear();
                        self.sel.insert(row);
                        self.anchor = Some(row);
                    }
                } else if ctrl {
                    if !self.sel.remove(&row) {
                        self.sel.insert(row);
                    }
                    self.anchor = Some(row);
                } else {
                    self.sel.clear();
                    self.sel.insert(row);
                    self.anchor = Some(row);
                    // 文件 / 另存为模式下点文件名，顺手填进文件名框（少敲一次）
                    if !is_dir && self.mode != Mode::Folder {
                        self.file_name = name.clone();
                    }
                }
            }
        }

        // ⑤ 状态栏：左边「几个项目」（筛选时补一句筛出多少）、右边「选了几项」，
        //    读取失败的原因也挂在这一行。参照 Files 的 "N items / 1 item selected"。
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let n_all = entries.len();
            let n_view = self.view.len();
            let line = if n_view != n_all {
                format!("{n_all} 个项目　已筛出 {n_view}")
            } else {
                format!("{n_all} 个项目")
            };
            ui.label(RichText::new(line).size(ui_scale::SMALL).color(DIM));
            if let Some(e) = &self.err {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("× {e}"))
                        .size(ui_scale::SMALL)
                        .color(ERR_C),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.sel.is_empty() {
                    ui.label(
                        RichText::new(format!("已选 {} 项", self.sel.len()))
                            .size(ui_scale::SMALL)
                            .color(DIM),
                    );
                }
            });
        });
        ui.separator();

        // ⑥ 底部：文件名 + 过滤说明 + 按钮
        ui.horizontal(|ui| {
            let need_name = matches!(self.mode, Mode::Save | Mode::Files);
            if need_name {
                ui.label("文件名");
                let w = if self.mode == Mode::Save { 360.0 } else { 260.0 };
                let resp = ui.add_sized(
                    [w, ui_scale::EDIT_H],
                    egui::TextEdit::singleline(&mut self.file_name).hint_text("可直接敲名字"),
                );
                let mut enter_name = false;
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    enter_used = true;
                    enter_name = true;
                }
                if enter_name {
                    let t = self.file_name.trim().to_string();
                    let sub = self.cwd.join(&t);
                    if !t.is_empty() && sub.is_dir() {
                        // 敲的是目录名 → 进目录，比报错友好
                        self.navigate(sub);
                    } else if let Some(paths) = self.confirm(entries) {
                        act = Some(Act::Close(Outcome::Picked(paths)));
                    }
                }
                ui.add_space(10.0);
            }
            ui.label(RichText::new(self.filter_label.clone()).small().color(DIM));
            if ui.checkbox(&mut self.show_hidden, "显示隐藏项").changed() {
                // 同上：self.all 此刻是空的，必须用 entries
                self.refresh_view(entries);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ok_label = match self.mode {
                    Mode::Folder => "选择此文件夹",
                    Mode::Files => "打开",
                    Mode::Save => "保存",
                };
                let can = self.confirm(entries).is_some();
                if ui
                    .add_enabled(can, egui::Button::new(RichText::new(ok_label).strong()))
                    .clicked()
                {
                    if let Some(paths) = self.confirm(entries) {
                        act = Some(Act::Close(Outcome::Picked(paths)));
                    }
                }
                ui.add_space(8.0);
                if ui.button("取消").clicked() {
                    act = Some(Act::Close(Outcome::Cancelled));
                }
                ui.add_space(10.0);
                // 兜底：自绘面板搞不定的场合退回系统对话框。
                // 故意做得小而不显眼 —— 它是逃生口，不是主路径。
                // 文字用 ASCII 的 `...`：`⋯`(U+22EF) 在本机是豆腐块（见上面 arrow 那段）。
                if ui
                    .small_button(RichText::new("...").color(DIM))
                    .on_hover_text("改用系统对话框（原生）")
                    .clicked()
                {
                    act = Some(Act::Close(Outcome::Fallback));
                }
            });
        });

        // ⑦ 全局 Enter（地址栏 / 文件名框已经吃掉的除外）。
        //    这里用 `ui.input` / `ui.memory` 而不是 `ctx` —— 绘制已经挪进
        //    `content(&mut self, ui, entries)`，不再往外接 `Context`。
        if !enter_used
            && act.is_none()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && ui.memory(|m| m.focused().is_none())
        {
            if let Some(paths) = self.confirm(entries) {
                act = Some(Act::Close(Outcome::Picked(paths)));
            }
        }

        act
    }
}

// ─────────────────────── 读目录（跑在后台线程里）───────────────────────

fn read_dir_entries(dir: &Path) -> Result<(PathBuf, Vec<Entry>), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{} —— {e}", dir.display()))?;
    let mut v = Vec::new();
    for item in rd {
        let Ok(de) = item else { continue }; // 单个条目读不到就跳过，不因一行坏掉整目录
        let name = de.file_name().to_string_lossy().into_owned();
        let (is_dir, size, mtime, hidden) = match de.metadata() {
            Ok(m) => (
                m.is_dir(),
                m.len(),
                m.modified().ok().and_then(to_unix),
                is_hidden(&m),
            ),
            Err(_) => (false, 0, None, false),
        };
        // 图标类型先算（`name` 马上要被 move 进结构体）。
        let icon = if is_dir {
            RowIcon::Dir
        } else {
            RowIcon::of(&name)
        };
        v.push(Entry {
            name,
            is_dir,
            size,
            mtime,
            hidden,
            icon,
        });
    }
    Ok((dir.to_path_buf(), v))
}

fn to_unix(t: SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

#[cfg(windows)]
fn is_hidden(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & 0x2 != 0 // FILE_ATTRIBUTE_HIDDEN
}

#[cfg(not(windows))]
fn is_hidden(_md: &std::fs::Metadata) -> bool {
    false
}

// ─────────────────────────── 小工具 ───────────────────────────

const DIM: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);
const DIR_C: Color32 = Color32::from_rgb(0x1F, 0x4E, 0x79);
const ERR_C: Color32 = Color32::from_rgb(0xB0, 0x30, 0x30);

/// 列表行内两个横向锚点（都相对行的左边缘）：图标中心、文件名起点。
///
/// 放到模块级是因为**表头的「名称」也必须用同一个 `NAME_DX`** ——
/// 表头与列里的数据差半格，正是「表头看着不齐」的成因。
const ICON_DX: f32 = 15.0;
const NAME_DX: f32 = 29.0;

/// 列表区的一套配色。
///
/// 单独拎出来是因为**浅色/深色主题必须各给一套** —— 深色主题下沿用浅灰底
/// 会变成一块刺眼的白斑（`stripe_bg` 早就踩过同一个坑）。
struct Palette {
    /// 列表空白处的底色。
    bg: Color32,
    /// 隔行底色（斑马纹）。
    zebra: Color32,
    /// 鼠标悬停行。
    hover: Color32,
    /// 选中行。
    sel: Color32,
    /// 列表外框。
    border: Color32,
    /// 表头底。
    head: Color32,
    /// 左侧导航栏的底色（比列表底略灰一档，才分得出两栏）。
    side: Color32,
}

fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode {
        Palette {
            bg: Color32::from_rgb(0x1B, 0x1E, 0x23),
            zebra: Color32::from_rgb(0x21, 0x25, 0x2B),
            hover: Color32::from_rgb(0x2A, 0x30, 0x39),
            sel: Color32::from_rgb(0x27, 0x3E, 0x5C),
            border: Color32::from_rgb(0x3B, 0x42, 0x4D),
            head: Color32::from_rgb(0x2C, 0x31, 0x39),
            side: Color32::from_rgb(0x20, 0x24, 0x2A),
        }
    } else {
        Palette {
            bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            zebra: Color32::from_rgb(0xF7, 0xF9, 0xFC),
            hover: Color32::from_rgb(0xEC, 0xF2, 0xFB),
            sel: Color32::from_rgb(0xDC, 0xE9, 0xFA),
            border: Color32::from_rgb(0xD2, 0xD9, 0xE2),
            head: Color32::from_rgb(0xEA, 0xEF, 0xF6),
            side: Color32::from_rgb(0xF6, 0xF7, 0xF9),
        }
    }
}

/// 当前目录与某个落脚点是不是同一处。
///
/// 只比文本、**不碰文件系统**（`canonicalize` 要做 IO，绘制路径上不能用）；
/// 末尾分隔符的差异不算不同（`D:` 与 `D:\` 是同一处）。
fn same_dir(cur_norm: &str, p: &Path) -> bool {
    let s = p.display().to_string();
    s.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(cur_norm)
}

/// 太长就砍掉尾巴加省略号（侧栏一行的宽度是死的）。
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 侧栏里盘符那一行的显示名。卷标可能很长（「C: 本地磁盘」甚至更长），
/// 但侧栏一行就 172 px，超了直接截断 —— 完整信息在悬停提示里。
fn drive_short(label: &str) -> String {
    ellipsize(label, 11)
}

/// 侧栏的分区小标题（「快速访问」「此电脑」…）。
fn nav_section(ui: &mut egui::Ui, label: &str) {
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(
            RichText::new(label)
                .size(ui_scale::SMALL - 0.5)
                .color(DIM),
        );
    });
    ui.add_space(3.0);
}

/// 侧栏行的图标类型。**手绘，不用任何字体符号** ——
/// `📁`、`▸` 这类在本机是豆腐块（见 `body` 里 arrow 那段注释）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavGlyph {
    Home,
    Download,
    Doc,
    Desktop,
    Drive,
    /// 映射网盘：两个叠起来的方块，像「共享」。
    Net,
    /// 最近使用：钟面。
    Clock,
}

/// 侧栏一行「图标 + 文字」。返回是否被点中。
///
/// 选中 = 淡蓝底 + 左侧 3 px 强调条；悬停 = 淡底。与 Files 的 NavigationView 一致。
fn nav_row(
    ui: &mut egui::Ui,
    pal: &Palette,
    glyph: NavGlyph,
    color: Color32,
    label: &str,
    selected: bool,
    tip: &str,
) -> bool {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(w, ui_scale::PICK_ROW_H),
        Sense::click(),
    );
    let resp = resp.on_hover_text(tip);
    let txt_c = ui.visuals().text_color();
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 4.0, pal.sel);
        // 左侧强调条：与列表里的选中行同一套语言。
        p.rect_filled(
            egui::Rect::from_min_size(rect.left_top(), Vec2::new(3.0, rect.height())),
            1.5,
            DIR_C,
        );
    } else if resp.hovered() {
        p.rect_filled(rect, 4.0, pal.hover);
    }
    let cy = rect.center().y;
    paint_place_icon(p, rect.left() + 16.0, cy, 15.0, glyph, color);
    p.text(
        egui::pos2(rect.left() + 32.0, cy),
        Align2::LEFT_CENTER,
        ellipsize(label, 13),
        FontId::proportional(ui_scale::SMALL + 1.0),
        txt_c,
    );
    resp.clicked()
}

/// 画侧栏行图标：一个圆角实心方块 + 白色细线字形。
///
/// 方块颜色区分"哪一类位置"，字形说明"是什么" —— 两者都靠 `painter` 的
/// 矩形/折线，不依赖字体。
fn paint_place_icon(p: &egui::Painter, cx: f32, cy: f32, h: f32, g: NavGlyph, color: Color32) {
    p.rect_filled(
        egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(h, h)),
        h * 0.26,
        color,
    );
    let st = egui::Stroke::new((h * 0.10).max(1.1), Color32::WHITE);
    let s = h * 0.27;
    match g {
        NavGlyph::Home => {
            p.add(egui::Shape::line(
                vec![
                    egui::pos2(cx - s, cy - s * 0.15),
                    egui::pos2(cx, cy - s * 0.95),
                    egui::pos2(cx + s, cy - s * 0.15),
                ],
                st,
            ));
            p.line_segment(
                [egui::pos2(cx - s * 0.6, cy - s * 0.15), egui::pos2(cx - s * 0.6, cy + s * 0.85)],
                st,
            );
            p.line_segment(
                [egui::pos2(cx + s * 0.6, cy - s * 0.15), egui::pos2(cx + s * 0.6, cy + s * 0.85)],
                st,
            );
            p.line_segment(
                [egui::pos2(cx - s * 0.6, cy + s * 0.85), egui::pos2(cx + s * 0.6, cy + s * 0.85)],
                st,
            );
        }
        NavGlyph::Download => {
            p.line_segment([egui::pos2(cx, cy - s * 0.9), egui::pos2(cx, cy + s * 0.45)], st);
            p.line_segment(
                [egui::pos2(cx - s * 0.55, cy - s * 0.1), egui::pos2(cx, cy + s * 0.45)],
                st,
            );
            p.line_segment(
                [egui::pos2(cx + s * 0.55, cy - s * 0.1), egui::pos2(cx, cy + s * 0.45)],
                st,
            );
            p.line_segment(
                [egui::pos2(cx - s * 0.7, cy + s * 0.85), egui::pos2(cx + s * 0.7, cy + s * 0.85)],
                st,
            );
        }
        NavGlyph::Doc => {
            let r = egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(s * 1.35, s * 1.8));
            p.rect_stroke(r, 0.5, st);
            p.line_segment(
                [egui::pos2(r.left() + s * 0.2, r.center().y), egui::pos2(r.right() - s * 0.2, r.center().y)],
                st,
            );
        }
        NavGlyph::Desktop => {
            let r = egui::Rect::from_center_size(
                egui::pos2(cx, cy - s * 0.18),
                egui::vec2(s * 2.0, s * 1.3),
            );
            p.rect_stroke(r, 0.5, st);
            p.line_segment([egui::pos2(cx, r.bottom()), egui::pos2(cx, cy + s * 0.8)], st);
            p.line_segment(
                [egui::pos2(cx - s * 0.5, cy + s * 0.8), egui::pos2(cx + s * 0.5, cy + s * 0.8)],
                st,
            );
        }
        NavGlyph::Drive => {
            let r = egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(s * 2.0, s * 1.4));
            p.rect_stroke(r, 0.8, st);
            p.line_segment(
                [
                    egui::pos2(r.left() + s * 0.3, r.center().y + s * 0.3),
                    egui::pos2(r.left() + s * 0.7, r.center().y + s * 0.3),
                ],
                st,
            );
        }
        NavGlyph::Net => {
            let a = egui::Rect::from_min_size(
                egui::pos2(cx - s, cy - s * 0.9),
                egui::vec2(s * 1.25, s * 1.25),
            );
            let b = egui::Rect::from_min_size(
                egui::pos2(cx - s * 0.25, cy - s * 0.35),
                egui::vec2(s * 1.25, s * 1.25),
            );
            p.rect_stroke(a, 0.4, st);
            p.rect_stroke(b, 0.4, st);
        }
        NavGlyph::Clock => {
            p.circle_stroke(egui::pos2(cx, cy), s * 0.95, st);
            p.line_segment([egui::pos2(cx, cy), egui::pos2(cx, cy - s * 0.55)], st);
            p.line_segment([egui::pos2(cx, cy), egui::pos2(cx + s * 0.45, cy)], st);
        }
    }
}

/// 地址栏右端那只「编辑路径」的小铅笔。
fn paint_pencil(p: &egui::Painter, c: egui::Pos2, hot: bool) {
    let col = if hot {
        Color32::from_rgb(0x10, 0x14, 0x1A)
    } else {
        DIM
    };
    let s = 5.5;
    p.line_segment(
        [egui::pos2(c.x - s, c.y + s), egui::pos2(c.x + s * 0.7, c.y - s * 0.7)],
        egui::Stroke::new(1.5_f32, col),
    );
    // 笔尖：左下角一个小三角
    p.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(c.x - s, c.y + s),
            egui::pos2(c.x - s + 3.4, c.y + s),
            egui::pos2(c.x - s, c.y + s - 3.4),
        ],
        col,
        egui::Stroke::NONE,
    ));
}

/// 把一块**凸多边形**按 `col(点)` 的线性渐变填满。
///
/// 实现是「重心 + 轮廓」的三角扇。之所以能用它做渐变：颜色只要取成位置的
/// **仿射**函数（这里是 `lerp(浅, 深, 0.42u + 0.58v)`），重心插值出来的就正是
/// 这个函数本身 —— 三角形之间不会有接缝，圆角也是真的圆弧。
/// （逐条画横带的做法在圆角处会露出方角，也做不出斜向的渐变。）
fn fill_shaded(
    p: &egui::Painter,
    pts: &[egui::Pos2],
    col: impl Fn(egui::Pos2) -> Color32,
) {
    if pts.len() < 3 {
        return;
    }
    let n = pts.len() as f32;
    let (mut sx, mut sy) = (0.0, 0.0);
    for q in pts {
        sx += q.x;
        sy += q.y;
    }
    let center = egui::pos2(sx / n, sy / n);
    let mut m = egui::Mesh::default();
    m.colored_vertex(center, col(center));
    for q in pts {
        m.colored_vertex(*q, col(*q));
    }
    let k = pts.len() as u32;
    for i in 0..k {
        m.add_triangle(0, 1 + i, 1 + (i + 1) % k);
    }
    p.add(egui::Shape::mesh(m));
}

/// 圆角矩形的**轮廓点**（顺时针，从左上角起），四角半径可以分别给 —— 画文件夹时
/// 主体的左上角要留成直角（与标签共用同一条左边线），另外三角才圆。
///
/// `seg` 是每个圆弧分几段；半径小到看不清时自动退化成 1 段（直角）。
fn rr_outline(rect: egui::Rect, r: [f32; 4], seg: usize) -> Vec<egui::Pos2> {
    // 顺序：左上 / 右上 / 右下 / 左下
    let lim = (rect.width() * 0.5).min(rect.height() * 0.5);
    let r = [r[0].min(lim), r[1].min(lim), r[2].min(lim), r[3].min(lim)];
    let c = [
        egui::pos2(rect.left() + r[0], rect.top() + r[0]),
        egui::pos2(rect.right() - r[1], rect.top() + r[1]),
        egui::pos2(rect.right() - r[2], rect.bottom() - r[2]),
        egui::pos2(rect.left() + r[3], rect.bottom() - r[3]),
    ];
    // 每角顺时针扫 90°：左上 180°→270°、右上 -90°→0°、右下 0°→90°、左下 90°→180°。
    let a0 = [
        std::f32::consts::PI,
        -std::f32::consts::FRAC_PI_2,
        0.0,
        std::f32::consts::FRAC_PI_2,
    ];
    let mut out = Vec::with_capacity(4 * seg);
    for i in 0..4 {
        let steps = if r[i] < 0.35 { 1 } else { seg };
        for k in 0..steps {
            let a = a0[i] + std::f32::consts::FRAC_PI_2 * (k as f32) / (steps as f32);
            out.push(egui::pos2(
                c[i].x + r[i] * a.cos(),
                c[i].y + r[i] * a.sin(),
            ));
        }
    }
    out
}

/// 两色线性插值（`t` 会被夹到 0..1）。画文件夹的渐变用。
fn lerp_c(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// 画一个**文件夹**，造型与配色照 Files（`files-community/files`）/ Windows 11
/// 资源管理器量出来的：左上凸起的**斜肩标签**（深金）+ 主体（**左上浅 → 右下深**
/// 的斜向渐变）+ 左边一条贯通的直线，**通体无描边**。
///
/// 与上一版（后板 / 前盖 / 一道暗金分隔线的三层矩形）的区别就在这：那版有硬边、
/// 像两块积木叠起来；Fluent 这版是一整块带体积感的色块，缩小之后更干净。
///
/// 配色固定、**不跟深浅主题走** —— Windows / Files 在深色底上同样是这只金色文件夹，
/// 且金在白底与深灰底上对比都够。
///
/// **仍然手绘**：`📁`、`▸`、`⋯` 这类符号在本机字体里缺字形会渲染成豆腐块
/// （表头那个 `▴` 就是这么翻车的，见 `body` 里 arrow 的注释），自己画不挑字体。
fn paint_dir_icon(p: &egui::Painter, cx: f32, cy: f32, h: f32) {
    /// 标签：比主体深一整档，两者之间靠这个色差分开（不靠描边）。
    const TAB: Color32 = Color32::from_rgb(0xFC, 0xBB, 0x19);
    /// 主体的浅端（左上）。
    const LIGHT: Color32 = Color32::from_rgb(0xFF, 0xE8, 0xA5);
    /// 主体的深端（右下）。
    const DEEP: Color32 = Color32::from_rgb(0xFF, 0xCA, 0x34);

    let w = h * 1.24;
    let left = cx - w / 2.0;
    let right = cx + w / 2.0;
    let top = cy - h / 2.0;
    let bottom = cy + h / 2.0;
    let r = h * 0.13; // 圆角：按高度取，改行高时不会走形
    // 下面三个比例都是拿 Files 的截图逐像素量出来的（不是估的）：
    let body_top = top + h * 0.20; // 标签比主体高出这一截
    let tab_end = left + w * 0.35; // 标签顶边的右端
    let shoulder = w * 0.11; // 斜肩的水平投影（落进主体顶边）

    // ① 标签（后层）。底边多压 0.03h 进主体，免得两层色块之间露出一条发丝缝。
    let over = h * 0.03;
    let tab_h = (body_top - top + over).max(1.0);
    let rr_t = r.min(tab_h);
    let mut tab = Vec::with_capacity(10);
    tab.push(egui::pos2(left + rr_t, top));
    tab.push(egui::pos2(tab_end, top));
    tab.push(egui::pos2(tab_end + shoulder, body_top + over));
    tab.push(egui::pos2(left, body_top + over));
    tab.push(egui::pos2(left, top + rr_t));
    // 左上圆角（180°→270°），与主体左下角同一半径 → 左边是一条连贯的直线。
    for k in 1..4 {
        let a = std::f32::consts::PI + std::f32::consts::FRAC_PI_2 * (k as f32) / 4.0;
        tab.push(egui::pos2(
            left + rr_t + rr_t * a.cos(),
            top + rr_t + rr_t * a.sin(),
        ));
    }
    fill_shaded(p, &tab, |_| TAB);

    // ② 主体（前层）：左上角**直角**（与标签共用左边线），另外三角圆。
    let body = egui::Rect::from_min_max(egui::pos2(left, body_top), egui::pos2(right, bottom));
    let pts = rr_outline(body, [0.0, r, r, r], 4);
    let span = (bottom - body_top).max(1.0);
    fill_shaded(p, &pts, |q| {
        // 斜向渐变：横向 0.42 + 纵向 0.58（比例是按截图逐像素采样拟合的）。
        let u = ((q.x - left) / w).clamp(0.0, 1.0);
        let v = ((q.y - body_top) / span).clamp(0.0, 1.0);
        lerp_c(LIGHT, DEEP, 0.42 * u + 0.58 * v)
    });
}

/// 画一个「文档」图标（空心矩形 + 右上折角），中心在 `cx/cy`，整体高 `h`。
///
/// 非 Office 类型的文件用它（灰色描边）。
fn paint_doc_outline(p: &egui::Painter, cx: f32, cy: f32, h: f32, color: Color32) {
    let w = h * 0.84;
    let r = egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(w, h));
    p.rect_stroke(r, 1.5, egui::Stroke::new(1.2_f32, color));
    // 折角：右上角那块实心三角。
    let c = h * 0.34;
    p.add(egui::Shape::convex_polygon(
        vec![
            r.right_top(),
            egui::pos2(r.right() - c, r.top()),
            egui::pos2(r.right(), r.top() + c),
        ],
        color,
        egui::Stroke::NONE,
    ));
}

/// 画非目录行的图标：**Office 系按各自的品牌色**（Word 蓝 / Excel 绿 / PPT 橙），
/// PDF 用红底 + 三条白线，其余文件保持灰色描边文档。
///
/// 这是从 Files（Windows 资源管理器那套）借来的观感：一眼就能分出文件类型。
/// **完全自绘**，不去抽系统壳图标 —— 那是每个文件一次 GDI/COM 调用，
/// 一个几百文件的目录会让打开面板肉眼可见地变慢（见模块头第 4 条）。
fn paint_row_icon(p: &egui::Painter, cx: f32, cy: f32, h: f32, kind: RowIcon, dim: Color32) {
    let Some(fill) = kind.solid() else {
        paint_doc_outline(p, cx, cy, h, dim);
        return;
    };
    let side = h * 0.98;
    let r = egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(side, side));
    p.rect_filled(r, side * 0.20, fill);
    match kind {
        RowIcon::Pdf => {
            // 三条白横线。**画出来而不是写 `≡`** —— 那类符号在本机是豆腐块。
            let st = egui::Stroke::new((side * 0.11).max(1.0), Color32::WHITE);
            for k in [-1.0_f32, 0.0, 1.0] {
                let y = cy + k * side * 0.21;
                p.line_segment(
                    [
                        egui::pos2(cx - side * 0.27, y),
                        egui::pos2(cx + side * 0.27, y),
                    ],
                    st,
                );
            }
        }
        _ => {
            if let Some(l) = kind.letter() {
                p.text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    l,
                    FontId::proportional(side * 0.70),
                    Color32::WHITE,
                );
            }
        }
    }
}

/// 导航图标（自绘，见 [`nav_button`]）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavIcon {
    /// 后退（历史栈往回）。
    Back,
    /// 前进（历史栈往前）。
    Forward,
    /// 上一级：向上的箭头。
    Up,
    /// 刷新：带缺口的圆环 + 箭头。
    Refresh,
}

/// 自绘一枚导航图标按钮，返回它的 `Response`（调用方自己判 `clicked()`）。
///
/// **为什么不用 `↑` / `⟳` 字符**：一，本机字体里这类符号缺字形就是豆腐块
/// （表头那个 `▴` 已经翻车过一次，见 `body` 里 `arrow` 的注释，`⟳`(U+27F3)
/// 只是碰巧有字形，换台机器就不敢保证）；二，字符的字重、大小都跟着字体走，
/// 放大字号后会跟着变粗，跟旁边的小按钮搭不上。自己画线最稳，也最像
/// 资源管理器：细笔画（1.6）、圆头、可用/不可用两档灰度。
fn nav_button(ui: &mut egui::Ui, kind: NavIcon, enabled: bool, tip: &str) -> egui::Response {
    let pal = palette(ui);
    let dark = ui.visuals().dark_mode;
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::splat(ui_scale::NAV_BTN),
        if enabled { Sense::click() } else { Sense::hover() },
    );
    let resp = resp.on_hover_text(tip);
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect.shrink(2.0), 4.0, pal.hover);
    }
    // 三档颜色：点不动（淡）、悬停（最显眼）、常态（正文色）。深浅主题各一套。
    let color = if !enabled {
        if dark {
            Color32::from_rgb(0x53, 0x5A, 0x66)
        } else {
            Color32::from_rgb(0xBB, 0xC2, 0xCC)
        }
    } else if resp.hovered() {
        if dark {
            Color32::from_rgb(0xFF, 0xFF, 0xFF)
        } else {
            Color32::from_rgb(0x10, 0x14, 0x1A)
        }
    } else if dark {
        Color32::from_rgb(0xD8, 0xDD, 0xE4)
    } else {
        Color32::from_rgb(0x38, 0x3D, 0x46)
    };

    let st = egui::Stroke::new(1.6_f32, color);
    let p = ui.painter();
    let c = rect.center();
    // 半径按控件边长取：改 `NAV_BTN` 时图形自动跟着缩放。
    let s = ui_scale::NAV_BTN * 0.28;
    match kind {
        NavIcon::Back => {
            // 横杆 + 左尖：与资源管理器的「后退」同形，只是把箭头朝左。
            p.line_segment(
                [egui::pos2(c.x - s, c.y), egui::pos2(c.x + s, c.y)],
                st,
            );
            p.line_segment(
                [egui::pos2(c.x - s, c.y), egui::pos2(c.x - s * 0.15, c.y - s * 0.85)],
                st,
            );
            p.line_segment(
                [egui::pos2(c.x - s, c.y), egui::pos2(c.x - s * 0.15, c.y + s * 0.85)],
                st,
            );
        }
        NavIcon::Forward => {
            p.line_segment(
                [egui::pos2(c.x + s, c.y), egui::pos2(c.x - s, c.y)],
                st,
            );
            p.line_segment(
                [egui::pos2(c.x + s, c.y), egui::pos2(c.x + s * 0.15, c.y - s * 0.85)],
                st,
            );
            p.line_segment(
                [egui::pos2(c.x + s, c.y), egui::pos2(c.x + s * 0.15, c.y + s * 0.85)],
                st,
            );
        }
        NavIcon::Up => {
            let tail = s * 1.0;
            // 竖杆 + 两撇，和资源管理器的「上一级」同形。
            p.line_segment([egui::pos2(c.x, c.y + tail), egui::pos2(c.x, c.y - tail)], st);
            p.line_segment(
                [egui::pos2(c.x - s, c.y - s * 0.1), egui::pos2(c.x, c.y - tail)],
                st,
            );
            p.line_segment(
                [egui::pos2(c.x + s, c.y - s * 0.1), egui::pos2(c.x, c.y - tail)],
                st,
            );
        }
        NavIcon::Refresh => {
            let r = s * 0.95;
            // 从右上（-40°）**顺时针**扫 320°，缺口留在右上角给箭头。
            let a0 = -40.0_f32.to_radians();
            let a1 = 280.0_f32.to_radians();
            let n = 26;
            let pts: Vec<egui::Pos2> = (0..=n)
                .map(|i| {
                    let a = a0 + (a1 - a0) * (i as f32 / n as f32);
                    egui::pos2(c.x + r * a.cos(), c.y + r * a.sin())
                })
                .collect();
            p.add(egui::Shape::line(pts, st));
            // 箭头：在弧的终点沿**切线**方向补一个实心小三角（指向顺时针前方）。
            let tan = egui::vec2(-a1.sin(), a1.cos());
            let rad = egui::vec2(a1.cos(), a1.sin());
            let end = egui::pos2(c.x + r * a1.cos(), c.y + r * a1.sin());
            p.add(egui::Shape::convex_polygon(
                vec![
                    end + tan * (r * 0.80),
                    end + rad * (r * 0.46),
                    end - rad * (r * 0.46),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
    }
    resp
}

/// 扩展名白名单判定。空表 = 全部放行；目录不该走到这里（调用方先判 `is_dir`）。
///
/// 是**模块级自由函数**而不是 `&self` 方法：`build_view` 是关联函数、没有 `self`，
/// 而它必须能调到这里。
fn ext_ok(exts: &[String], name: &str) -> bool {
    if exts.is_empty() {
        return true;
    }
    match name.rsplit_once('.') {
        Some((_, e)) => exts.iter().any(|x| x.eq_ignore_ascii_case(e)),
        None => false,
    }
}

fn fmt_size(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / (K * K))
    } else {
        format!("{:.2} GB", f / (K * K * K))
    }
}

/// 只写文件名时补到当前目录；给了绝对路径（含盘符或 UNC）就原样用。
fn resolve_typed(cwd: &Path, typed: &str) -> PathBuf {
    let p = Path::new(typed);
    if p.is_absolute() || typed.starts_with(r"\\") {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// 从盘符标签里取回 `"Z:"` 这样的裸字母（标签可能带卷标或「· 网络」后缀）。
fn letter_of(label: &str) -> String {
    let mut it = label.chars();
    match (it.next(), it.next()) {
        (Some(c), Some(':')) if c.is_ascii_alphabetic() => format!("{c}:"),
        _ => label.split_whitespace().next().unwrap_or(label).to_string(),
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

/// 把目录拆成面包屑分段：`(显示名, 该段指向的路径)`。
///
/// * `C:\Users\me\Documents`
///   → `[("C:", C:\), ("Users", …\Users), ("me", …), ("Documents", …)]`
/// * `\\server\share\sub` → `[("\\server\share", …), ("sub", …)]`
///   —— UNC 的服务器与共享名**必须合成一段**：`\\server` 单独作为路径
///   Windows 是访问不了的，点它只会报错。
///
/// 只在 [`Picker::set_loc`] 里调用（每次真正换目录一次），不在绘制里跑。
fn crumbs_of(dir: &Path) -> Vec<(String, PathBuf)> {
    use std::path::Component;
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut prefix: Option<String> = None;
    let mut acc = PathBuf::new();
    for c in dir.components() {
        match c {
            Component::Prefix(p) => {
                prefix = Some(p.as_os_str().to_string_lossy().into_owned());
            }
            Component::RootDir => {
                let pre = prefix.take().unwrap_or_default();
                // 根段路径 = 前缀 + `\`；UNC 时前缀本身就是 `\\server\share`。
                acc = PathBuf::from(format!("{pre}\\"));
                // 显示名：盘符段就写 `C:`，UNC 写 `\\server\share`。
                let label = if pre.is_empty() { acc.display().to_string() } else { pre };
                out.push((label, acc.clone()));
            }
            Component::Normal(n) => {
                acc.push(n);
                out.push((n.to_string_lossy().into_owned(), acc.clone()));
            }
            _ => {}
        }
    }
    if out.is_empty() {
        // 相对路径（理论上到不了这儿）：退成整串，至少别空着。
        out.push((dir.display().to_string(), dir.to_path_buf()));
    }
    out
}

/// 常用落脚点。只列**真的存在**的，免得点了跳进空目录。
fn quick_places() -> Vec<(&'static str, PathBuf)> {
    let home = home_dir();
    let mut v: Vec<(&'static str, PathBuf)> = vec![("用户", home.clone())];
    for (label, sub) in [
        ("桌面", "Desktop"),
        ("文档", "Documents"),
        ("下载", "Downloads"),
    ] {
        let p = home.join(sub);
        if p.is_dir() {
            v.push((label, p));
        }
    }
    v
}

// ─────────────────────────── Windows 原生调用 ───────────────────────────

#[cfg(windows)]
mod win {
    use super::Drive;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FILETIME {
        lo: u32,
        hi: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
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

    // Rust 2024：extern 块本身要写 unsafe
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetVolumeInformationW(
            root: *const u16,
            vol: *mut u16,
            vol_len: u32,
            serial: *mut u32,
            max_len: *mut u32,
            flags: *mut u32,
            fs: *mut u16,
            fs_len: u32,
        ) -> i32;
        fn WNetGetConnectionW(local: *const u16, remote: *mut u16, len: *mut u32) -> u32;
        fn FileTimeToLocalFileTime(src: *const FILETIME, dst: *mut FILETIME) -> i32;
        fn FileTimeToSystemTime(src: *const FILETIME, dst: *mut SYSTEMTIME) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 枚举本机盘符。映射网盘就在这张表里（类型 REMOTE），不用额外 API。
    pub fn drives() -> Vec<Drive> {
        let mask = unsafe { GetLogicalDrives() };
        let mut v = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let ch = (b'A' + i as u8) as char;
            let root = format!("{ch}:\\");
            let w = wide(&root);
            let t = unsafe { GetDriveTypeW(w.as_ptr()) };
            let kind = match t {
                2 => "可移动磁盘",
                3 => "本地磁盘",
                4 => "网络驱动器",
                5 => "光驱",
                6 => "内存盘",
                _ => "未知",
            };
            let mut remote = None;
            if t == 4 {
                let local = wide(&format!("{ch}:"));
                let mut buf = vec![0u16; 1024];
                let mut len = buf.len() as u32;
                // 返回 0 = 成功
                if unsafe { WNetGetConnectionW(local.as_ptr(), buf.as_mut_ptr(), &mut len) } == 0 {
                    let s = String::from_utf16_lossy(&buf)
                        .trim_end_matches('\0')
                        .to_string();
                    if !s.is_empty() {
                        remote = Some(s);
                    }
                }
            }
            // 卷标只用来点缀，读不到不影响主功能
            let mut vol = vec![0u16; 261];
            let _ = unsafe {
                GetVolumeInformationW(
                    w.as_ptr(),
                    vol.as_mut_ptr(),
                    vol.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            let label = String::from_utf16_lossy(&vol);
            let label = label.trim_end_matches('\0');
            let letter = if label.is_empty() {
                format!("{ch}:")
            } else {
                format!("{ch}: {label}")
            };
            v.push(Drive {
                letter,
                kind,
                remote,
            });
        }
        v
    }

    /// 本地时区下的「YYYY-MM-DD HH:MM」。走 Win32 的 FILETIME 转换，
    /// 不引 chrono —— 本工具只跑 Windows，这一条够用且没有额外依赖。
    pub fn fmt_mtime(unix: i64) -> String {
        // 1970-01-01 相对 1601-01-01 的 100ns 刻度数
        const EPOCH: u64 = 116_444_736_000_000_000;
        if unix < 0 {
            return "—".to_string();
        }
        let ticks = EPOCH + (unix as u64) * 10_000_000;
        let ft = FILETIME {
            lo: (ticks & 0xFFFF_FFFF) as u32,
            hi: (ticks >> 32) as u32,
        };
        let mut local = FILETIME { lo: 0, hi: 0 };
        let mut st = SYSTEMTIME::default();
        unsafe {
            if FileTimeToLocalFileTime(&ft, &mut local) == 0 {
                return "—".to_string();
            }
            if FileTimeToSystemTime(&local, &mut st) == 0 {
                return "—".to_string();
            }
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            st.year, st.month, st.day, st.hour, st.minute
        )
    }
}

#[cfg(windows)]
fn drives() -> Vec<Drive> {
    win::drives()
}

#[cfg(windows)]
fn fmt_mtime(unix: i64) -> String {
    win::fmt_mtime(unix)
}

#[cfg(not(windows))]
fn drives() -> Vec<Drive> {
    Vec::new()
}

#[cfg(not(windows))]
fn fmt_mtime(_unix: i64) -> String {
    "—".to_string()
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir,
            size: 10,
            mtime: Some(1_700_000_000),
            hidden: false,
            icon: if is_dir {
                RowIcon::Dir
            } else {
                RowIcon::of(name)
            },
        }
    }

    /// 测试里刷新视图，等价于 `poll()` 里那段（数据取 `p.all`）。
    fn refresh(p: &mut Picker) {
        let view = Picker::build_view(&p.all, &p.exts, p.show_hidden, p.sort, p.sort_desc);
        p.view = view;
        p.sel.clear();
        p.anchor = None;
    }

    /// 面板要能在**无窗口**环境下跑若干帧而不 panic。
    ///
    /// egui 的 `Context` 是纯 CPU 的（字体光栅 + 布局），不需要 GPU ——
    /// 这正是 `--selftest` 一直在用的路子。这里额外验证**真去读了目录**：
    /// 后台线程把结果灌回来之后，`all` 里应该有东西。
    #[test]
    fn headless_跑若干帧并把目录读进来() {
        let ctx = egui::Context::default();
        let mut p = Picker::new();
        p.open(Mode::Files, "测试", PathBuf::from(r"C:\"), &["docx"], "");
        for _ in 0..60 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                let _ = p.show(ctx);
            });
            if p.load.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(p.load.is_none(), "后台读取没有回来");
        assert!(!p.all.is_empty(), r"C:\ 一个条目都没读到");
    }

    /// 扩展名过滤：目录永远留下，文件按白名单。
    #[test]
    fn 扩展名过滤() {
        let mut p = Picker::new();
        p.exts = vec!["docx".to_string()];
        assert!(ext_ok(&p.exts, "a.docx"));
        assert!(ext_ok(&p.exts, "A.DOCX"), "扩展名比较要不区分大小写");
        assert!(!ext_ok(&p.exts, "a.xlsx"));
        assert!(!ext_ok(&p.exts, "没有扩展名"));

        p.all = vec![entry("子目录", true), entry("a.docx", false), entry("b.xlsx", false)];
        p.show_hidden = false;
        refresh(&mut p);
        // 目录 + docx，xlsx 被挡在外面
        assert_eq!(p.view.len(), 2);
        assert!(p.all[p.view[0]].is_dir, "目录要排在前面");
    }

    /// 隐藏项开关要真的起作用。
    #[test]
    fn 隐藏项开关() {
        let mut p = Picker::new();
        p.all = vec![entry("正常.txt", false), {
            let mut e = entry("藏起来的.txt", false);
            e.hidden = true;
            e
        }];
        refresh(&mut p);
        assert_eq!(p.view.len(), 1, "默认不显示隐藏项");
        p.show_hidden = true;
        refresh(&mut p);
        assert_eq!(p.view.len(), 2, "勾上之后要看得见");
    }

    /// 三种模式各自的「确认」返回什么。
    #[test]
    fn 三种模式的确认结果() {
        let cwd = PathBuf::from(r"C:\proj");

        // ① 选目录：没选中任何目录 → 就是当前目录
        let mut p = Picker::new();
        p.mode = Mode::Folder;
        p.cwd = cwd.clone();
        assert_eq!(p.confirm(&p.all), Some(vec![cwd.clone()]));

        // ② 选目录：恰好选中一个子目录 → 选它，而不是当前目录
        p.all = vec![entry("子目录", true)];
        p.view = vec![0];
        p.sel.insert(0);
        assert_eq!(p.confirm(&p.all), Some(vec![PathBuf::from(r"C:\proj\子目录")]));

        // ③ 选文件：多选两个文件
        let mut p = Picker::new();
        p.mode = Mode::Files;
        p.cwd = cwd.clone();
        p.all = vec![entry("a.docx", false), entry("b.docx", false), entry("子目录", true)];
        p.view = vec![0, 1, 2];
        p.sel.insert(0);
        p.sel.insert(1);
        assert_eq!(
            p.confirm(&p.all),
            Some(vec![PathBuf::from(r"C:\proj\a.docx"), PathBuf::from(r"C:\proj\b.docx")])
        );

        // ④ 选文件：一个都没选、文件名框也空 → 没什么可确认（按钮置灰）
        p.sel.clear();
        p.file_name = String::new();
        assert_eq!(p.confirm(&p.all), None);

        // ⑤ 另存为：不要求文件已存在，直接拼路径
        let mut p = Picker::new();
        p.mode = Mode::Save;
        p.cwd = cwd.clone();
        p.file_name = "新规则.xlsx".to_string();
        assert_eq!(p.confirm(&p.all), Some(vec![PathBuf::from(r"C:\proj\新规则.xlsx")]));
    }

    /// **回归**：面板打开期间（`self.all` 被借走、当场是空的）确认与刷新都不能崩。
    ///
    /// v0.2.0 的「点一下就退出」就是这么来的：`confirm()` 原来读 `&self.all[i]`，
    /// 而 `body()` 执行时 `self.all` 已经被 `show()` take 出去给绘制用了，
    /// `view` 里的下标却还指着那份表 → 越界 panic。
    ///
    /// 为什么一"点"就中：底部那排按钮**每帧**都要调 `confirm()` 来判断
    /// 「打开/保存」按钮该不该置灰（`let can = self.confirm(...).is_some()`），
    /// 所以只要面板里有任何选中项，下一帧就必崩。
    /// 而双击目录反而不会 —— 那条路 `navigate()` 之后直接 `return`，
    /// 走不到底部按钮。（用户报的正是"单击目录就退出"。）
    #[test]
    fn 数据被借走时确认与刷新都不能越界() {
        let mut p = Picker::new();
        p.mode = Mode::Folder;
        p.cwd = PathBuf::from(r"C:\proj");
        p.all = vec![entry("子目录", true)];
        p.view = vec![0];
        p.sel.insert(0);

        // 模拟 show() 把 all take 出去、交给 body 绘制的那一刻
        let entries = std::mem::take(&mut p.all);
        assert!(p.all.is_empty(), "这一刻 self.all 必须是空的（复现前提）");

        // ① 数据从 entries 进来 —— 照样能算出结果
        assert_eq!(
            p.confirm(&entries),
            Some(vec![PathBuf::from(r"C:\proj\子目录")])
        );

        // ② 兜底：调用方一份空表都没给，也只能"没有选中"，绝不能 panic
        assert_eq!(
            p.confirm(&[]),
            Some(vec![PathBuf::from(r"C:\proj")]),
            "Folder 模式没选中目录时退化为「当前目录」"
        );
        p.mode = Mode::Files;
        assert_eq!(p.confirm(&[]), None, "Files 模式没有文件可选时按钮该置灰");
        p.mode = Mode::Folder;

        // ③ 面板打开期间刷新视图（勾「显示隐藏项」/ 点表头）同样必须基于 entries，
        //    否则列表会被清空 —— 那是同一个错误的另一副面孔。
        p.show_hidden = true;
        p.refresh_view(&entries);
        assert_eq!(p.view, vec![0], "视图该按 entries 重建，而不是对着空的 self.all 算");
    }

    /// 只写文件名 → 补到当前目录；绝对路径 / UNC → 原样用。
    /// **UNC 这条就是"局域网共享路径"能不能走通的关键。**
    #[test]
    fn 路径补全规则() {
        let cwd = Path::new(r"C:\proj");
        assert_eq!(resolve_typed(cwd, "a.docx"), PathBuf::from(r"C:\proj\a.docx"));
        assert_eq!(resolve_typed(cwd, r"Z:\net\a.docx"), PathBuf::from(r"Z:\net\a.docx"));
        assert_eq!(
            resolve_typed(cwd, r"\\server\share\a.docx"),
            PathBuf::from(r"\\server\share\a.docx"),
            "UNC 必须原样保留，不能拼到当前目录后面"
        );
    }

    /// 映射网盘的盘符标签要能还原成裸字母（标签上可能挂着卷标或「· 网络」）。
    #[test]
    fn 盘符标签还原() {
        assert_eq!(letter_of("C:"), "C:");
        assert_eq!(letter_of("Z: 项目盘"), "Z:");
        assert_eq!(letter_of("Y: · 网络"), "Y:");
    }

    /// **栈预算**：面板必须能在 **1 MB** 线程栈上跑得动。
    ///
    /// 为什么死钉 1 MB —— 这是 v0.2.0 用户端「点一下就退出」的根因所在：
    ///
    /// * 本机 GNU 工具链产物主线程栈是 **2 MB**（PE 头 `SizeOfStackReserve`）
    /// * CI 的 MSVC 产物只有 **1 MB**（rustc 对 MSVC 目标的默认值）
    ///
    /// 同一个面板在开发机（2 MB）从不出事，到用户手里（1 MB）一开就
    /// 「窗口凭空消失」—— 因为 Windows 上栈溢出是**立即终止进程**，
    /// 图形子系统又没有控制台，Rust 那句 "has overflowed its stack"
    /// 写进了丢失的 stderr：没有对话框、没有崩溃转储、什么都没有。
    /// 而且本机的企业管控套件（AppInit_DLLs 全局注入）会在每个进程上
    /// 额外吃一截栈，余量比正常机器更紧。
    ///
    /// 所以这个测试是**回归防线**：面板的布局层级一旦又变深，它会当场变红。
    #[test]
    fn 一兆栈上也要跑得动() {
        let n = std::thread::Builder::new()
            .name("one-mb-stack".to_string())
            .stack_size(1024 * 1024)
            .spawn(|| {
                let ctx = egui::Context::default();
                let mut p = Picker::new();
                p.open(
                    Mode::Files,
                    "栈预算",
                    PathBuf::from(r"C:\Windows"),
                    &["docx"],
                    "",
                );
                for _ in 0..300 {
                    let _ = ctx.run(egui::RawInput::default(), |c| {
                        let _ = p.show(c);
                    });
                    if p.load.is_none() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                // 列表就绪后再多跑几帧：真正吃栈的是「有内容的完整布局」，
                // 空列表那几帧反而轻。
                for _ in 0..20 {
                    let _ = ctx.run(egui::RawInput::default(), |c| {
                        let _ = p.show(c);
                    });
                }
                p.view.len()
            })
            .expect("线程起不来")
            .join()
            .expect("1 MB 栈上的面板把线程搞崩了 —— 多半就是栈溢出");
        assert!(n > 0, "面板一个条目都没列出，测试没跑到位");
    }

    /// 体积格式化。
    #[test]
    fn 体积格式化() {
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(2048), "2.0 KB");
        assert_eq!(fmt_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(fmt_size(2 * 1024 * 1024 * 1024), "2.00 GB");
    }
}
