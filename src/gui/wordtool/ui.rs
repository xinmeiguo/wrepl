//! 页眉替换（文本 / 图片）与批量打印两页 —— 界面 + 任务调度。
//!
//! ## 线程模型
//!
//! 与主界面一致，也是「界面线程只画、活丢后台」：主线程跑 eframe 事件循环，
//! Word COM 在**独立 STA 线程**里执行（`ComInit::sta()` —— Word 自动化强制要求
//! STA，不初始化会拿不到 COM 对象），日志 / 进度 / 完成信号经 channel 回传，
//! 界面每帧轮询一次。
//!
//! ## 与来源的差异
//!
//! 取自 `word_header_tool_rust/src/app.rs`（v0.2.0）。原来是三个标签页
//! （图片替换 / 文本替换 / 批量打印），这里把前两页**并成「页眉替换」一页**、
//! 用单选切换 —— 它们的流程完全一样（选输入 → 选文档 → 执行），
//! 差别只在"往单元格里放什么"，分成两页纯属重复。
//!
//! ## 两点接线上的约定
//!
//! 1. **选文档走主界面那套自绘选择器**。本模块不自己弹对话框：选择器
//!    （`picker.rs`）是 `App` 的字段、全程序只有一台，而且它是**常驻**的
//!    （跨次打开记得上次目录）。按钮只把「想选」这个意图抛出去
//!    （[`PickDoc`] → [`WordTool::take_pick`]），由 `App::render` 收口。
//! 2. **打印机列表来自系统**，切换仍交给 Word。见 [`super::printers`] 的说明。

use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui::{self, RichText};

use crate::ds;

use super::com::ComInit;
use super::printers::{self, Printer};
use super::services;

/// 「选择文档」按钮想干的事。抛给 `App`，由它去开自绘选择器。
///
/// 为什么不在这里直接开：选择器挂在 `App` 上（见模块头第 1 条），
/// 而且这个函数正处在 `App` 的借用期内，够不着。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PickDoc {
    /// 页眉替换页的「选择文档」
    Header,
    /// 批量打印页的「选择文档」
    Print,
}

/// 页眉替换的两种子模式。
#[derive(PartialEq, Clone, Copy)]
pub enum HeaderMode {
    Text,
    Image,
}

/// 后台 STA 线程回传的消息。
enum WtMsg {
    Log(String),
    Progress { current: usize, total: usize },
    Done { verb: String, ok: i32, fail: i32 },
}

/// 正在跑的一趟任务。
struct Task {
    rx: Receiver<WtMsg>,
    /// 进度（当前 / 总数）；`None` 表示已结束。
    progress: Option<(usize, usize)>,
}

pub struct WordTool {
    // ---------- 页眉替换 ----------
    pub hdr_mode: HeaderMode,
    pub hdr_docs: Vec<String>,
    pub text_en: String,
    pub text_cn: String,
    pub img_path: String,
    pub img_scale: bool,
    // ---------- 批量打印 ----------
    pub print_docs: Vec<String>,
    pub page_range: String,
    /// 本机打印机（进入打印页时枚举一次并缓存）。
    pub printers: Vec<Printer>,
    /// 下拉框选择：`0` = 用系统默认；`n>0` = `printers[n-1]`。
    pub printer_pick: usize,
    /// 枚举打印机失败的原因（显示在页面上，而不是让下拉框空着让人猜）。
    pub printers_err: Option<String>,
    printers_loaded: bool,
    // ---------- 共享 ----------
    pub log: Vec<String>,
    /// 「选择文档」的待办，由 `App` 每帧取走（见 [`PickDoc`]）。
    want_pick: Option<PickDoc>,
    task: Option<Task>,
    /// 上一趟的结果摘要（文案, 是否全成功）。没跑过就是 `None`。
    last_result: Option<(String, bool)>,
}

impl Default for WordTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WordTool {
    pub fn new() -> Self {
        Self {
            hdr_mode: HeaderMode::Text,
            hdr_docs: Vec::new(),
            text_en: String::new(),
            text_cn: String::new(),
            img_path: String::new(),
            img_scale: true,
            print_docs: Vec::new(),
            page_range: String::new(),
            printers: Vec::new(),
            printer_pick: 0,
            printers_err: None,
            printers_loaded: false,
            log: Vec::new(),
            want_pick: None,
            task: None,
            last_result: None,
        }
    }

    /// 有任务在跑。主界面据此避免切页与重入。
    pub fn busy(&self) -> bool {
        self.task.is_some()
    }

    /// 取走「要选文档」的待办。`App` 每帧调一次。
    pub fn take_pick(&mut self) -> Option<PickDoc> {
        self.want_pick.take()
    }

    /// 自绘选择器选完文档之后，由 `App` 回填。`what` 决定落到哪一页的列表。
    pub fn set_docs(&mut self, what: PickDoc, docs: Vec<String>) {
        match what {
            PickDoc::Header => self.hdr_docs = docs,
            PickDoc::Print => self.print_docs = docs,
        }
    }

    /// 已选的文档（供 `App` 开选择器时猜起点目录）。
    pub fn docs_of(&self, what: PickDoc) -> &[String] {
        match what {
            PickDoc::Header => &self.hdr_docs,
            PickDoc::Print => &self.print_docs,
        }
    }

    fn log(&mut self, s: String) {
        self.log.push(s);
        if self.log.len() > 500 {
            self.log.drain(0..self.log.len() - 500);
        }
    }

    /// 重扫打印机。
    ///
    /// 同步调（`EnumPrintersW` 在装机上一般几毫秒）。**不进绘制路径的常驻部分** ——
    /// 只在进入打印页时做一次、以及用户点「重新扫描」时做一次，
    /// 所以这点耗时不会变成"每帧都卡一下"。
    pub fn reload_printers(&mut self) {
        match printers::list() {
            Ok(v) => {
                // 选择按**名字**保留：重扫后顺序可能变，按下标留会指错打印机。
                let keep = self.selected_printer().map(|p| p.name.clone());
                self.printers = v;
                self.printers_err = None;
                self.printer_pick = keep
                    .and_then(|n| {
                        self.printers
                            .iter()
                            .position(|p| p.name.eq_ignore_ascii_case(&n))
                    })
                    .map_or(0, |i| i + 1);
            }
            Err(e) => {
                self.printers.clear();
                self.printers_err = Some(e);
                self.printer_pick = 0;
            }
        }
        self.printers_loaded = true;
    }

    fn selected_printer(&self) -> Option<&Printer> {
        self.printer_pick
            .checked_sub(1)
            .and_then(|i| self.printers.get(i))
    }

    /// 下拉框当前显示的文字。
    fn printer_label(&self) -> String {
        match self.selected_printer() {
            Some(p) => p.name.clone(),
            None => "（用默认打印机）".to_string(),
        }
    }

    /// 在独立 STA 线程里跑一趟 Word COM 任务。
    fn start<F>(&mut self, verb: &str, f: F)
    where
        F: FnOnce(&mut dyn FnMut(String), &mut dyn FnMut(usize, usize)) -> (i32, i32)
            + Send
            + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.task = Some(Task {
            rx,
            progress: Some((0, 0)),
        });
        self.last_result = None;
        let verb_s = verb.to_string();
        std::thread::spawn(move || {
            let _com = match ComInit::sta() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(WtMsg::Log(format!("COM 初始化失败：{e}")));
                    let _ = tx.send(WtMsg::Done {
                        verb: verb_s,
                        ok: 0,
                        fail: 0,
                    });
                    return;
                }
            };
            let (ok, fail) = f(
                &mut |s: String| {
                    let _ = tx.send(WtMsg::Log(s));
                },
                &mut |cur: usize, total: usize| {
                    let _ = tx.send(WtMsg::Progress {
                        current: cur,
                        total,
                    });
                },
            );
            let _ = tx.send(WtMsg::Done {
                verb: verb_s,
                ok,
                fail,
            });
        });
    }

    /// 每帧轮询工作线程。
    pub fn poll(&mut self) {
        let (messages, disconnected) = {
            let Some(t) = &self.task else { return };
            let mut msgs = Vec::new();
            let mut disc = false;
            loop {
                match t.rx.try_recv() {
                    Ok(m) => msgs.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disc = true;
                        break;
                    }
                }
            }
            (msgs, disc)
        };

        let mut finished: Option<(String, i32, i32)> = None;
        for m in messages {
            match m {
                WtMsg::Log(s) => self.log(s),
                WtMsg::Progress { current, total } => {
                    if let Some(t) = &mut self.task {
                        t.progress = Some((current, total));
                    }
                }
                WtMsg::Done { verb, ok, fail } => {
                    self.log(format!("{verb}完成：成功 {ok}，失败 {fail}"));
                    finished = Some((verb, ok, fail));
                }
            }
        }

        if let Some((verb, ok, fail)) = finished {
            self.last_result = Some((format!("{verb}完成：成功 {ok}，失败 {fail}"), fail == 0));
            self.task = None;
        } else if disconnected {
            self.task = None;
        }
    }

    // ==================== 渲染：页眉替换 ====================

    pub fn ui_header(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();

        // ---- 卡片 1：输入 ----
        card(ui, |ui| {
            card_title(ui, "输入内容");
            // GUIDE §5：单选组用 `RadioGroup`（横向排版走 `.horizontal()`）。
            // 注意它和 `InputField` 一样是 `.show(ui)`，不是 `Widget`。
            ds::RadioGroup::new(&mut self.hdr_mode)
                .horizontal()
                .option(ds::RadioOption::new(HeaderMode::Text, "文本"))
                .option(ds::RadioOption::new(HeaderMode::Image, "图片"))
                .show(ui);
            ui.add_space(ds::SPACING.s3);

            match self.hdr_mode {
                HeaderMode::Text => {
                    // GUIDE §6：标签放字段**正上方**（`InputField` 自带 label 槽位），
                    // 不再用旧的「左标签 + 右输入框」那套 `row()`。
                    ds::InputField::new(&mut self.text_en)
                        .label("英文")
                        .show(ui);
                    ui.add_space(ds::SPACING.s3);
                    ds::InputField::new(&mut self.text_cn)
                        .label("中文")
                        .show(ui);
                }
                HeaderMode::Image => {
                    ui.horizontal(|ui| {
                        if ui
                            .add(ds::Button::secondary("选择图片…").leading(ds::Icon::FolderOpen))
                            .clicked()
                        {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_title("请选择要替换进单元格的图片")
                                .add_filter(
                                    "图片文件",
                                    &["png", "jpg", "jpeg", "bmp", "gif", "tif", "tiff"],
                                )
                                .pick_file()
                            {
                                self.img_path = p.to_string_lossy().into_owned();
                            }
                        }
                        ui.add_space(ds::SPACING.s4);
                        ui.add(ds::Checkbox::with_label(&mut self.img_scale, "按表格高度等比缩放"));
                    });
                    ui.add_space(ds::SPACING.s3);
                    // 图片走的是系统对话框、界面上没有对应输入框，所以这里必须留一处
                    // 能看见"到底选了哪张"—— 只显示路径本身，不加前缀说明文字。
                    let disp = if self.img_path.is_empty() {
                        "（未选）".to_string()
                    } else {
                        self.img_path.clone()
                    };
                    hint(ui, &disp);
                }
            }
        });

        ui.add_space(ds::SPACING.s4);

        // ---- 卡片 2：文档 ----
        card(ui, |ui| {
            doc_header(ui, "文档", self.hdr_docs.len(), |ui| {
                if ui
                    .add(
                        ds::Button::secondary("选择文档…")
                            .leading(ds::Icon::FolderOpen)
                            .disabled(busy),
                    )
                    .clicked()
                {
                    self.want_pick = Some(PickDoc::Header);
                }
                ui.add_space(ds::SPACING.s2);
                if ui
                    .add(
                        ds::Button::ghost("清空")
                            .leading(ds::Icon::Trash)
                            .disabled(self.hdr_docs.is_empty() || busy),
                    )
                    .clicked()
                {
                    self.hdr_docs.clear();
                }
            });
            ui.add_space(ds::SPACING.s3);
            doc_list(ui, "wt_doc_scroll_h", &self.hdr_docs);
        });

        ui.add_space(ds::SPACING.s4);

        // ---- 卡片 3：执行 ----
        let can = !busy
            && !self.hdr_docs.is_empty()
            && match self.hdr_mode {
                HeaderMode::Text => {
                    !(self.text_en.trim().is_empty() && self.text_cn.trim().is_empty())
                }
                HeaderMode::Image => !self.img_path.is_empty(),
            };
        card(ui, |ui| {
            ui.horizontal(|ui| {
                // GUIDE §4：一屏只有**一个** primary —— 本页就是「执行替换」。
                let clicked = ui
                    .add(
                        ds::Button::primary("执行替换")
                            .size(ds::ButtonSize::Lg)
                            .leading(ds::Icon::Lightning)
                            .disabled(!can),
                    )
                    .clicked();
                if clicked {
                    let docs = self.hdr_docs.clone();
                    match self.hdr_mode {
                        HeaderMode::Text => {
                            let (en, cn) = (self.text_en.clone(), self.text_cn.clone());
                            self.log(format!("开始页眉文本替换：{} 个文档", docs.len()));
                            self.start("页眉文本替换", move |log, prog| {
                                services::text_replace_run(&docs, &en, &cn, log, prog)
                            });
                        }
                        HeaderMode::Image => {
                            let (img, scale) = (self.img_path.clone(), self.img_scale);
                            self.log(format!("开始页眉图片替换：{} 个文档", docs.len()));
                            self.start("页眉图片替换", move |log, prog| {
                                services::image_replace_run(&docs, &img, scale, log, prog)
                            });
                        }
                    }
                }
                ui.add_space(ds::SPACING.s4);
                self.status_rows(ui);
            });
        });

        ui.add_space(ds::SPACING.s4);
        self.log_section(ui);
    }

    // ==================== 渲染：批量打印 ====================

    pub fn ui_print(&mut self, ui: &mut egui::Ui) {
        // 打印机只枚举一次并缓存 —— 见 `reload_printers`。
        if !self.printers_loaded {
            self.reload_printers();
        }
        let busy = self.busy();

        // ---- 卡片 1：文档 ----
        card(ui, |ui| {
            doc_header(ui, "文档", self.print_docs.len(), |ui| {
                if ui
                    .add(
                        ds::Button::secondary("选择文档…")
                            .leading(ds::Icon::FolderOpen)
                            .disabled(busy),
                    )
                    .clicked()
                {
                    self.want_pick = Some(PickDoc::Print);
                }
                ui.add_space(ds::SPACING.s2);
                if ui
                    .add(
                        ds::Button::ghost("清空")
                            .leading(ds::Icon::Trash)
                            .disabled(self.print_docs.is_empty() || busy),
                    )
                    .clicked()
                {
                    self.print_docs.clear();
                }
            });
            ui.add_space(ds::SPACING.s3);
            doc_list(ui, "wt_doc_scroll_p", &self.print_docs);
        });

        ui.add_space(ds::SPACING.s4);

        // ---- 卡片 2：打印机与页码范围 ----
        card(ui, |ui| {
            card_title(ui, "打印设置");

            // GUIDE §6：标签放字段正上方，下拉换 `SelectField`。
            // 它和 `InputField` 一样是 `.show(ui, …)`、**不是 `Widget`**。
            let cur_label = self.printer_label();
            // 先把选项文案算成 owned 的 `Vec<String>`：`SelectField::show` 要
            // `&str` 选项，而「系统默认」那档得现拼一段字符串。落成 owned 之后再
            // 借成 `Vec<(usize, &str)>`，进 `show` 时就只借 `self.printer_pick`
            // （可变）与这份局部数据，不碰 `self.printers`，少一处借用冲突。
            let labels: Vec<String> = std::iter::once("（用默认打印机）".to_string())
                .chain(self.printers.iter().map(|p| {
                    if p.is_default {
                        format!("{}（系统默认）", p.name)
                    } else {
                        p.name.clone()
                    }
                }))
                .collect();
            let opts: Vec<(usize, &str)> = labels
                .iter()
                .enumerate()
                .map(|(i, s)| (i, s.as_str()))
                .collect();
            ds::SelectField::new("wt_printer")
                .label("打印机")
                .width(420.0)
                .show(ui, &mut self.printer_pick, &cur_label, opts);
            ui.add_space(ds::SPACING.s2);

            if ui
                .add(
                    ds::Button::secondary("重新扫描")
                        .leading(ds::Icon::Refresh)
                        .disabled(busy),
                )
                .on_hover_text("重新枚举本机打印机（刚装了打印机 / 刚映射了共享打印机时用）")
                .clicked()
            {
                self.reload_printers();
            }
            ui.add_space(ds::SPACING.s3);

            ds::InputField::new(&mut self.page_range)
                .label("页码范围")
                .desired_width(180.0)
                .show(ui);

            // 说明性文案一律不摆 —— 下拉框本身就能看出有几台、选的是哪台。
            // 只有"枚举失败"必须说出来，否则下拉框空着没人知道为什么。
            if let Some(e) = &self.printers_err {
                let pal = ds::palette_of(ui.ctx());
                ui.add_space(ds::SPACING.s3);
                ui.label(
                    RichText::new(format!("× 读取打印机列表失败：{e}"))
                        .text_style(egui::TextStyle::Small)
                        .color(pal.error),
                );
            }
        });

        ui.add_space(ds::SPACING.s4);

        // ---- 卡片 3：执行 ----
        let can = !busy && !self.print_docs.is_empty();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let clicked = ui
                    .add(
                        ds::Button::primary("开始打印")
                            .size(ds::ButtonSize::Lg)
                            .leading(ds::Icon::Package)
                            .disabled(!can),
                    )
                    .clicked();
                if clicked {
                    let docs = self.print_docs.clone();
                    let range = self.page_range.trim().to_string();
                    // 打印机字符串在这里定下来（"名称 on 端口"）——
                    // 线程里不再碰 UI 状态。
                    let printer = self
                        .selected_printer()
                        .map(|p| p.active_string())
                        .filter(|s| !s.is_empty());
                    self.log(format!(
                        "开始打印：{} 个文档（{}）",
                        docs.len(),
                        match &printer {
                            Some(p) => format!("打印机 {p}"),
                            None => "默认打印机".to_string(),
                        }
                    ));
                    self.start("打印", move |log, prog| {
                        services::print_run(&docs, &range, printer.as_deref(), log, prog)
                    });
                }
                ui.add_space(ds::SPACING.s4);
                self.status_rows(ui);
            });
        });

        ui.add_space(ds::SPACING.s4);
        self.log_section(ui);
    }

    // ==================== 共享：进度 / 结果 / 日志 ====================

    /// 进度条 + 结果摘要（两页共用）。
    fn status_rows(&self, ui: &mut egui::Ui) {
        let pal = ds::palette_of(ui.ctx());
        if let Some((cur, total)) = self.task.as_ref().and_then(|t| t.progress) {
            let frac = if total > 0 {
                (cur as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let text = if total > 0 {
                format!("处理中… {cur} / {total}")
            } else {
                "正在启动 Word…".to_string()
            };
            // `ProgressBar` 是 `Widget`（`ui.add`）；`.label(..)` 收 `&str`，
            // 所以先把文案落成 owned 字符串再借进去。
            ui.add(ds::ProgressBar::new(frac).label(&text).height(8.0));
        }
        if let Some((msg, ok)) = &self.last_result {
            ui.label(
                RichText::new(msg)
                    .strong()
                    .color(if *ok { pal.success } else { pal.error }),
            );
        }
    }

    /// 运行日志（两页共用）。
    fn log_section(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                card_title(ui, "运行日志");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            ds::Button::ghost("清空")
                                .leading(ds::Icon::Trash)
                                .disabled(self.log.is_empty()),
                        )
                        .clicked()
                    {
                        self.log.clear();
                    }
                });
            });
            ui.add_space(ds::SPACING.s3);
            well(ui, |ui| {
                let pal = ds::palette_of(ui.ctx());
                egui::ScrollArea::vertical()
                    .id_salt("wt_log_scroll")
                    .max_height(170.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label(
                                RichText::new("（暂无）")
                                    .text_style(egui::TextStyle::Small)
                                    .color(pal.text_tertiary),
                            );
                        }
                        for line in &self.log {
                            // GUIDE §7：日志/等宽文本用 mono 13。
                            ui.label(
                                RichText::new(line)
                                    .font(egui::FontId::monospace(13.0))
                                    .color(pal.text_primary),
                            );
                        }
                    });
            });
        });
    }
}

// ==================== 自由函数（避免与 `&mut self` 的借用冲突）====================

/// 统一的卡片容器 —— 直接用设计系统的 [`ds::Card`]。
///
/// 仍要 `set_width(available_width)`：`Card::show` **不自动撑宽**，
/// 不设的话卡片会缩到内容那么大，整页内容挤在左上角。
fn card<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ds::Card::new().show(ui, |ui| {
        ui.set_width(ui.available_width());
        f(ui)
    })
}

/// 卡片里的「凹槽」（已选文档列表 / 运行日志）。
///
/// 底色取 palette 里**比卡面更深**的那一档，两套主题各取各的：
/// 浅色主题卡面是纯白（`bg_surface`）→ 槽用 `bg_surface_alt`（浅灰）；
/// 深色主题卡面是 `bg_surface` → 再暗一档是 `bg_app`。
/// 不写死颜色，主题一换自己跟着走。
fn well<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let pal = ds::palette_of(ui.ctx());
    let sunken = if pal.dark_mode {
        pal.bg_app
    } else {
        pal.bg_surface_alt
    };
    egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(10, 8))
        .corner_radius(egui::CornerRadius::same(ds::RADIUS.sm as u8))
        .fill(sunken)
        .stroke(egui::Stroke::new(1.0_f32, pal.border_subtle))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            f(ui)
        })
        .inner
}

// 原先这里有一个手搓的 `input()`（自己套 `egui::Frame` 补描边 + `TextEdit::frame(NONE)`）
// 和它的配套 `hairline()`。原因见旧注释：egui 0.29 浅色主题里未聚焦控件的
// `inactive.bg_stroke` 是 `Stroke::NONE`，输入框落在白卡面上会"消失"。
// 迁移到设计系统后这件事由 `ds::InputField` 接管 —— 它的边框来自 palette
// （`border_default` / 聚焦时 `brand_default`），浅深两套都有人管，不必再自己补。

/// 卡片标题：与 [`ds::Card::title`] 同一档（h3 + `text_primary`），
/// 但由调用方自己画 —— 「文档」「运行日志」这两张卡要把按钮挂在标题**同一行**
/// 的右侧，用 `Card::title` 就做不到了（它固定独占一行）。
fn card_title(ui: &mut egui::Ui, s: &str) {
    let pal = ds::palette_of(ui.ctx());
    ui.label(
        RichText::new(s)
            .text_style(egui::TextStyle::Name("h3".into()))
            .color(pal.text_primary),
    );
}

// 原先这里有一个 `row()`（左边定宽标签 + 右边一整行控件）。迁移后标签改由
// `ds::InputField` / `ds::SelectField` 自带的 label 槽位画在**字段正上方**
// （GUIDE §6），这个函数就没人用了。
// 它当年记录的那个坑仍值得留着：**不要在 `ui.horizontal(..)` 内部去问
// `ui.available_width()`** —— 横向布局里那个值不是"剩下的这一截"，实测拿到 0，
// 于是把输入框压成 0 宽、框线画不出来，只剩提示文字浮在卡面上（看起来像
// "输入框没画背景"，很容易误往配色上去查）。

/// 灰色小字。现在只用于**显示状态**（如已选图片的路径、空列表占位），
/// 不再承担"向用户解释功能"的职责 —— 那类文案按"界面清爽"的要求已全部撤掉。
fn hint(ui: &mut egui::Ui, s: &str) {
    let pal = ds::palette_of(ui.ctx());
    ui.label(
        RichText::new(s)
            .text_style(egui::TextStyle::Small)
            .color(pal.text_tertiary),
    );
}

/// 「标题 + 按钮」那一行，右侧挂「已选 N 个」（两页的文档卡片共用）。
fn doc_header(ui: &mut egui::Ui, title: &str, count: usize, buttons: impl FnOnce(&mut egui::Ui)) {
    let pal = ds::palette_of(ui.ctx());
    ui.horizontal(|ui| {
        card_title(ui, title);
        ui.add_space(ds::SPACING.s2);
        buttons(ui);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("已选 {count} 个"))
                    .text_style(egui::TextStyle::Small)
                    .color(pal.text_tertiary),
            );
        });
    });
}

/// 已选文档列表（撑满宽度、定高滚动）。
fn doc_list(ui: &mut egui::Ui, id: &str, docs: &[String]) {
    let pal = ds::palette_of(ui.ctx());
    well(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(id)
            .max_height(150.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if docs.is_empty() {
                    ui.label(
                        RichText::new("（未选）")
                            .text_style(egui::TextStyle::Small)
                            .color(pal.text_tertiary),
                    );
                }
                for (i, d) in docs.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{:>3}.", i + 1))
                                .text_style(egui::TextStyle::Small)
                                .color(pal.text_tertiary),
                        );
                        ui.label(RichText::new(file_name(d)).text_style(egui::TextStyle::Small));
                    });
                }
            });
    });
}

/// 取文件名（去掉路径）。
fn file_name(p: &str) -> String {
    p.rsplit(['\\', '/']).next().unwrap_or(p).to_string()
}
