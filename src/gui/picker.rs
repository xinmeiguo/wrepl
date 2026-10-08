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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui::{Align2, Color32, FontId, RichText, Sense, Vec2};

// ─────────────────────────── 对外接口 ───────────────────────────

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
        let entries = std::mem::take(&mut self.all);
        egui::Area::new(egui::Id::new("wrepl-picker"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                (screen.center().x - 470.0).max(8.0),
                (screen.center().y - 310.0).max(8.0),
            ))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(12.0))
                    .show(ui, |ui| {
                        ui.set_width(920.0);
                        ui.set_height(600.0);
                        act = self.body(ui, ctx, &entries);
                    });
            });
        self.all = entries;

        match act {
            Some(Act::Close(o)) => {
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
                        self.all = list;
                        self.cwd = dir.clone();
                        self.addr = dir.display().to_string();
                        self.last_dir = dir;
                        self.err = None;
                        self.rebuild_view();
                    }
                    Err(e) => {
                        // 读不到就停在原地，把原因说清楚（权限 / 网络 / 路径不存在）
                        self.err = Some(e);
                        self.addr = self.cwd.display().to_string();
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

    /// 跳到某个目录（**不在 UI 线程上读**）。
    fn navigate(&mut self, dir: PathBuf) {
        let seq = self.load.as_ref().map_or(1, |l| l.seq + 1);
        let (tx, rx) = mpsc::channel();
        let d = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send((seq, read_dir_entries(&d)));
        });
        self.addr = dir.display().to_string();
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
        self.navigate(d);
    }

    fn go_up(&mut self) {
        if let Some(p) = self.cwd.parent().map(Path::to_path_buf) {
            self.navigate(p);
        }
    }

    /// 按当前过滤 + 排序重建可见列表。**过滤条件 / 排序键一变就要调**。
    fn rebuild_view(&mut self) {
        let mut idx: Vec<usize> = (0..self.all.len())
            .filter(|&i| {
                let e = &self.all[i];
                (self.show_hidden || !e.hidden) && (e.is_dir || self.ext_ok(&e.name))
            })
            .collect();

        let (key, desc) = (self.sort, self.sort_desc);
        idx.sort_by(|&a, &b| {
            let (ea, eb) = (&self.all[a], &self.all[b]);
            // 目录永远排在文件前面，这一层不受 asc/desc 影响
            if ea.is_dir != eb.is_dir {
                return if ea.is_dir {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            let o = match key {
                SortKey::Name => ea.name.to_lowercase().cmp(&eb.name.to_lowercase()),
                SortKey::Size => ea.size.cmp(&eb.size),
                SortKey::Time => ea.mtime.cmp(&eb.mtime),
            };
            if desc { o.reverse() } else { o }
        });

        self.view = idx;
        // 行号会变，选中跟着清掉免得指错人
        self.sel.clear();
        self.anchor = None;
    }

    fn ext_ok(&self, name: &str) -> bool {
        if self.exts.is_empty() {
            return true;
        }
        match name.rsplit_once('.') {
            Some((_, e)) => self.exts.iter().any(|x| x.eq_ignore_ascii_case(e)),
            None => false,
        }
    }

    /// 确认。返回 `None` 表示当前状态下没什么可确认（按钮会置灰）。
    fn confirm(&self) -> Option<Vec<PathBuf>> {
        match self.mode {
            Mode::Folder => {
                // 恰好选中一个目录 → 选它；否则 → 当前所在目录
                let picked: Vec<PathBuf> = self
                    .sel
                    .iter()
                    .filter_map(|&v| self.view.get(v))
                    .map(|&i| &self.all[i])
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
                    .map(|&i| &self.all[i])
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

    fn body(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, entries: &[Entry]) -> Option<Act> {
        let mut act: Option<Act> = None;
        let mut enter_used = false;

        // ⓪ 标题
        let title = self.title.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(15.0).strong().color(DIR_C));
        });
        ui.add_space(6.0);

        // ① 盘符 + 快捷位
        let drv = self.drives_cache.clone();
        let places = self.places_cache.clone();
        ui.horizontal_wrapped(|ui| {
            let mut jump: Option<PathBuf> = None;
            for d in &drv {
                let label = match &d.remote {
                    Some(_) => format!("{} · 网络", d.letter),
                    None => d.letter.clone(),
                };
                let b = match &d.remote {
                    Some(r) => ui
                        .small_button(label)
                        .on_hover_text(format!("映射到 {r}")),
                    None => ui.small_button(label).on_hover_text(d.kind),
                };
                if b.clicked() {
                    jump = Some(PathBuf::from(format!("{}\\", letter_of(&d.letter))));
                }
            }
            ui.separator();
            for (label, p) in &places {
                if ui.small_button(*label).clicked() {
                    jump = Some(p.clone());
                }
            }
            if let Some(p) = jump {
                self.navigate(p);
            }
        });
        ui.add_space(6.0);

        // ② 地址栏
        ui.horizontal(|ui| {
            let up_ok = self.cwd.parent().is_some();
            if ui
                .add_enabled(up_ok, egui::Button::new("↑"))
                .on_hover_text("上一级")
                .clicked()
            {
                self.go_up();
            }
            if ui
                .button("⟳")
                .on_hover_text("刷新（网络路径可能较慢）")
                .clicked()
            {
                self.reload();
            }

            let w = (ui.available_width() - 76.0).max(160.0);
            let resp = ui.add_sized(
                [w, 24.0],
                egui::TextEdit::singleline(&mut self.addr)
                    .hint_text(r"目录，或 \\服务器\共享\子目录"),
            );
            // 地址栏回车 = 转到（**吃掉这次 Enter**，不再触发底部的确认）
            let mut go_addr = false;
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                enter_used = true;
                go_addr = true;
            }
            if ui.button("转到").clicked() {
                go_addr = true;
            }
            if go_addr {
                let t = self.addr.trim().to_string();
                if !t.is_empty() {
                    self.navigate(PathBuf::from(t));
                }
            }
        });
        ui.add_space(6.0);

        // ③ 表头（可点：切换排序）
        let row_h = 22.0;
        let full_w = ui.available_width();
        let col_size = 92.0;
        let col_time = 132.0;
        let name_w = (full_w - col_size - col_time - 30.0).max(120.0);

        let mut sort_click: Option<SortKey> = None;
        ui.horizontal(|ui| {
            let arrow = |k: SortKey, s: &Self| -> &'static str {
                if s.sort == k {
                    // ★ 只用 ↑ ↓（U+2191/2193）。`▴`/`▾`(U+25B4/BE) 在本机的字体里
                    //   是**豆腐块** —— 实测表头会显示成「名称 □」，与 §5.3 同一类坑。
                    if s.sort_desc { " ↓" } else { " ↑" }
                } else {
                    ""
                }
            };
            let mut hdr = |ui: &mut egui::Ui, w: f32, txt: String, k: SortKey| {
                if ui
                    .add_sized(
                        [w, 20.0],
                        egui::Button::new(RichText::new(txt).small().color(DIM)).frame(false),
                    )
                    .clicked()
                {
                    sort_click = Some(k);
                }
            };
            hdr(
                ui,
                name_w,
                format!("名称{}", arrow(SortKey::Name, self)),
                SortKey::Name,
            );
            hdr(
                ui,
                col_size,
                format!("大小{}", arrow(SortKey::Size, self)),
                SortKey::Size,
            );
            hdr(
                ui,
                col_time,
                format!("修改时间{}", arrow(SortKey::Time, self)),
                SortKey::Time,
            );
        });
        if let Some(k) = sort_click {
            if self.sort == k {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort = k;
                self.sort_desc = false;
            }
            self.rebuild_view();
        }

        ui.separator();

        // ④ 列表
        let list_h = (ui.available_height() - 78.0).max(120.0);
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
            egui::ScrollArea::vertical()
                .max_height(list_h)
                .auto_shrink([false, false])
                .show_rows(ui, row_h, self.view.len(), |ui, range| {
                    let stripe = stripe_bg(ui);
                    let txt_c = ui.visuals().text_color();
                    for row in range {
                        let ei = self.view[row];
                        let e = &entries[ei];
                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::new(full_w, row_h), Sense::click());
                        if !ui.is_rect_visible(rect) {
                            continue;
                        }
                        let selected = self.sel.contains(&row);
                        if selected {
                            ui.painter()
                                .rect_filled(rect, 2.0, Color32::from_rgb(0xCF, 0xE0, 0xF5));
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, 2.0, stripe);
                        }

                        let p = ui.painter();
                        let f_name = FontId::proportional(13.5);
                        let f_meta = FontId::proportional(12.0);
                        let cy = rect.center().y;
                        let (label, color) = if e.is_dir {
                            (format!("{}\\", e.name), DIR_C)
                        } else {
                            (e.name.clone(), txt_c)
                        };
                        p.text(
                            egui::pos2(rect.left() + 8.0, cy),
                            Align2::LEFT_CENTER,
                            &label,
                            f_name,
                            color,
                        );
                        if !e.is_dir {
                            p.text(
                                egui::pos2(rect.left() + name_w + 8.0, cy),
                                Align2::LEFT_CENTER,
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

            if let Some((row, dbl, ctrl, shift)) = clicked {
                let ei = self.view[row];
                let is_dir = entries[ei].is_dir;
                let name = entries[ei].name.clone();

                if dbl {
                    if is_dir {
                        self.navigate(self.cwd.join(&name));
                        return act;
                    }
                    // 双击文件 = 直接确认
                    self.sel.clear();
                    self.sel.insert(row);
                    if let Some(paths) = self.confirm() {
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

        // 读取失败的原因（权限 / 网络 / 路径不存在）
        if let Some(e) = &self.err {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("× {e}")).small().color(ERR_C));
        } else {
            ui.add_space(4.0);
        }
        ui.separator();

        // ⑤ 底部：文件名 + 过滤说明 + 按钮
        ui.horizontal(|ui| {
            let need_name = matches!(self.mode, Mode::Save | Mode::Files);
            if need_name {
                ui.label("文件名");
                let w = if self.mode == Mode::Save { 360.0 } else { 260.0 };
                let resp = ui.add_sized(
                    [w, 24.0],
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
                    } else if let Some(paths) = self.confirm() {
                        act = Some(Act::Close(Outcome::Picked(paths)));
                    }
                }
                ui.add_space(10.0);
            }
            ui.label(RichText::new(self.filter_label.clone()).small().color(DIM));
            if ui.checkbox(&mut self.show_hidden, "显示隐藏项").changed() {
                self.rebuild_view();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ok_label = match self.mode {
                    Mode::Folder => "选择此文件夹",
                    Mode::Files => "打开",
                    Mode::Save => "保存",
                };
                let can = self.confirm().is_some();
                if ui
                    .add_enabled(can, egui::Button::new(RichText::new(ok_label).strong()))
                    .clicked()
                {
                    if let Some(paths) = self.confirm() {
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

        // ⑥ 全局 Enter（地址栏 / 文件名框已经吃掉的除外）
        if !enter_used
            && act.is_none()
            && ctx.input(|i| i.key_pressed(egui::Key::Enter))
            && ctx.memory(|m| m.focused().is_none())
        {
            if let Some(paths) = self.confirm() {
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
        v.push(Entry {
            name,
            is_dir,
            size,
            mtime,
            hidden,
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

fn stripe_bg(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(0x33, 0x38, 0x41)
    } else {
        Color32::from_rgb(0xE8, 0xEE, 0xF6)
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
        }
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
        assert!(p.ext_ok("a.docx"));
        assert!(p.ext_ok("A.DOCX"), "扩展名比较要不区分大小写");
        assert!(!p.ext_ok("a.xlsx"));
        assert!(!p.ext_ok("没有扩展名"));

        p.all = vec![entry("子目录", true), entry("a.docx", false), entry("b.xlsx", false)];
        p.show_hidden = false;
        p.rebuild_view();
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
        p.rebuild_view();
        assert_eq!(p.view.len(), 1, "默认不显示隐藏项");
        p.show_hidden = true;
        p.rebuild_view();
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
        assert_eq!(p.confirm(), Some(vec![cwd.clone()]));

        // ② 选目录：恰好选中一个子目录 → 选它，而不是当前目录
        p.all = vec![entry("子目录", true)];
        p.view = vec![0];
        p.sel.insert(0);
        assert_eq!(p.confirm(), Some(vec![PathBuf::from(r"C:\proj\子目录")]));

        // ③ 选文件：多选两个文件
        let mut p = Picker::new();
        p.mode = Mode::Files;
        p.cwd = cwd.clone();
        p.all = vec![entry("a.docx", false), entry("b.docx", false), entry("子目录", true)];
        p.view = vec![0, 1, 2];
        p.sel.insert(0);
        p.sel.insert(1);
        assert_eq!(
            p.confirm(),
            Some(vec![PathBuf::from(r"C:\proj\a.docx"), PathBuf::from(r"C:\proj\b.docx")])
        );

        // ④ 选文件：一个都没选、文件名框也空 → 没什么可确认（按钮置灰）
        p.sel.clear();
        p.file_name = String::new();
        assert_eq!(p.confirm(), None);

        // ⑤ 另存为：不要求文件已存在，直接拼路径
        let mut p = Picker::new();
        p.mode = Mode::Save;
        p.cwd = cwd.clone();
        p.file_name = "新规则.xlsx".to_string();
        assert_eq!(p.confirm(), Some(vec![PathBuf::from(r"C:\proj\新规则.xlsx")]));
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

    /// 体积格式化。
    #[test]
    fn 体积格式化() {
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(2048), "2.0 KB");
        assert_eq!(fmt_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(fmt_size(2 * 1024 * 1024 * 1024), "2.00 GB");
    }
}
