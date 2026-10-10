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

use eframe::egui::{self, Color32, RichText};

use crate::app::ui_scale;

use super::com::ComInit;
use super::printers::{self, Printer};
use super::services;

const OK_C: Color32 = Color32::from_rgb(0x1B, 0x7F, 0x3B);
const ERR_C: Color32 = Color32::from_rgb(0xC0, 0x2B, 0x1D);
const DIM_C: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);
const ACCENT: Color32 = Color32::from_rgb(0x1F, 0x4E, 0x79);

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
        // ---- 卡片 1：输入 ----
        card(ui, |ui| {
            card_title(ui, "输入内容");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.hdr_mode, HeaderMode::Text, "文本");
                ui.add_space(16.0);
                ui.radio_value(&mut self.hdr_mode, HeaderMode::Image, "图片");
            });
            ui.add_space(10.0);

            match self.hdr_mode {
                HeaderMode::Text => {
                    row(ui, "英文", |ui, w| {
                        input(ui, w, &mut self.text_en, "");
                    });
                    ui.add_space(6.0);
                    row(ui, "中文", |ui, w| {
                        input(ui, w, &mut self.text_cn, "");
                    });
                }
                HeaderMode::Image => {
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized([140.0, ui_scale::ROW_H], egui::Button::new("选择图片…"))
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
                        ui.add_space(14.0);
                        ui.checkbox(&mut self.img_scale, "按表格高度等比缩放");
                    });
                    ui.add_space(8.0);
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

        ui.add_space(10.0);

        // ---- 卡片 2：文档 ----
        card(ui, |ui| {
            doc_header(ui, "文档", self.hdr_docs.len(), |ui| {
                if ui
                    .add_enabled(!self.busy(), egui::Button::new("选择文档…"))
                    .clicked()
                {
                    self.want_pick = Some(PickDoc::Header);
                }
                ui.add_space(6.0);
                if ui
                    .add_enabled(
                        !self.hdr_docs.is_empty() && !self.busy(),
                        egui::Button::new("清空"),
                    )
                    .clicked()
                {
                    self.hdr_docs.clear();
                }
            });
            ui.add_space(8.0);
            doc_list(ui, "wt_doc_scroll_h", &self.hdr_docs);
        });

        ui.add_space(10.0);

        // ---- 卡片 3：执行 ----
        let can = !self.busy()
            && !self.hdr_docs.is_empty()
            && match self.hdr_mode {
                HeaderMode::Text => {
                    !(self.text_en.trim().is_empty() && self.text_cn.trim().is_empty())
                }
                HeaderMode::Image => !self.img_path.is_empty(),
            };
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let clicked = ui
                    .add_enabled(
                        can,
                        egui::Button::new(
                            RichText::new("执行替换")
                                .size(ui_scale::BUTTON)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(ACCENT)
                        .min_size(egui::vec2(140.0, 34.0)),
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
                ui.add_space(14.0);
                self.status_rows(ui);
            });
        });

        ui.add_space(10.0);
        self.log_section(ui);
    }

    // ==================== 渲染：批量打印 ====================

    pub fn ui_print(&mut self, ui: &mut egui::Ui) {
        // 打印机只枚举一次并缓存 —— 见 `reload_printers`。
        if !self.printers_loaded {
            self.reload_printers();
        }

        // ---- 卡片 1：文档 ----
        card(ui, |ui| {
            doc_header(ui, "文档", self.print_docs.len(), |ui| {
                if ui
                    .add_enabled(!self.busy(), egui::Button::new("选择文档…"))
                    .clicked()
                {
                    self.want_pick = Some(PickDoc::Print);
                }
                ui.add_space(6.0);
                if ui
                    .add_enabled(
                        !self.print_docs.is_empty() && !self.busy(),
                        egui::Button::new("清空"),
                    )
                    .clicked()
                {
                    self.print_docs.clear();
                }
            });
            ui.add_space(8.0);
            doc_list(ui, "wt_doc_scroll_p", &self.print_docs);
        });

        ui.add_space(10.0);

        // ---- 卡片 2：打印机与页码范围 ----
        card(ui, |ui| {
            card_title(ui, "打印设置");

            row(ui, "打印机", |ui, _w| {
                let label = self.printer_label();
                // 先把「下拉项」算成一份独立数据再进闭包：闭包里只借
                // `self.printer_pick`（可变）与这份局部数据，不和 `self.printers`
                // 的借用打架 —— 少一处"能不能借过去"的隐式依赖。
                let items: Vec<(usize, String)> = std::iter::once((0usize, "（用默认打印机）".to_string()))
                    .chain(self.printers.iter().enumerate().map(|(i, p)| {
                        let t = if p.is_default {
                            format!("{}（系统默认）", p.name)
                        } else {
                            p.name.clone()
                        };
                        (i + 1, t)
                    }))
                    .collect();
                egui::ComboBox::from_id_salt("wt_printer")
                    .width(420.0)
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (k, t) in &items {
                            ui.selectable_value(&mut self.printer_pick, *k, t);
                        }
                    });
                ui.add_space(10.0);
                if ui
                    .add_enabled(!self.busy(), egui::Button::new("重新扫描"))
                    .on_hover_text("重新枚举本机打印机（刚装了打印机 / 刚映射了共享打印机时用）")
                    .clicked()
                {
                    self.reload_printers();
                }
            });
            ui.add_space(8.0);

            row(ui, "页码范围", |ui, _w| {
                input(ui, 180.0, &mut self.page_range, "");
            });
            ui.add_space(8.0);

            // 说明性文案一律不摆 —— 下拉框本身就能看出有几台、选的是哪台。
            // 只有"枚举失败"必须说出来，否则下拉框空着没人知道为什么。
            if let Some(e) = &self.printers_err {
                ui.label(
                    RichText::new(format!("× 读取打印机列表失败：{e}"))
                        .size(ui_scale::SMALL)
                        .color(ERR_C),
                );
            }
        });

        ui.add_space(10.0);

        // ---- 卡片 3：执行 ----
        let can = !self.busy() && !self.print_docs.is_empty();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let clicked = ui
                    .add_enabled(
                        can,
                        egui::Button::new(
                            RichText::new("开始打印")
                                .size(ui_scale::BUTTON)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(ACCENT)
                        .min_size(egui::vec2(140.0, 34.0)),
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
                ui.add_space(14.0);
                self.status_rows(ui);
            });
        });

        ui.add_space(10.0);
        self.log_section(ui);
    }

    // ==================== 共享：进度 / 结果 / 日志 ====================

    /// 进度条 + 结果摘要（两页共用）。
    fn status_rows(&self, ui: &mut egui::Ui) {
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
            ui.add(
                egui::ProgressBar::new(frac)
                    .text(RichText::new(text).small())
                    .desired_width(340.0),
            );
        }
        if let Some((msg, ok)) = &self.last_result {
            ui.label(
                RichText::new(msg)
                    .strong()
                    .color(if *ok { OK_C } else { ERR_C }),
            );
        }
    }

    /// 运行日志（两页共用）。
    fn log_section(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                card_title(ui, "运行日志");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("清空").clicked() {
                        self.log.clear();
                    }
                });
            });
            ui.add_space(8.0);
            well(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("wt_log_scroll")
                    .max_height(170.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label(RichText::new("（暂无）").color(DIM_C).small());
                        }
                        for line in &self.log {
                            ui.label(
                                RichText::new(line)
                                    .size(ui_scale::LOG)
                                    .color(ui.visuals().text_color()),
                            );
                        }
                    });
            });
        });
    }
}

// ==================== 自由函数（避免与 `&mut self` 的借用冲突）====================

/// 统一的卡片容器。
///
/// 两条都照搬主界面的 [`crate::app`] `section()`：
///
/// 1. **必须撑满可用宽度** —— 否则卡片缩到内容那么大，整页内容挤在左上角。
/// 2. **底色用 `faint_bg_color`（浅灰）而不是纯白** —— egui 的单行输入框
///    底色是 `extreme_bg_color`（浅色主题下就是**白**），卡片要是也白，
///    输入框的框线在卡面上就看不见了（实测：截图里输入框整个"消失"，
///    只剩一行提示文字）。浅灰卡面 + 白色输入框才是主界面那套观感。
fn card<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            f(ui)
        })
        .inner
}

/// 卡片里的「凹槽」（列表 / 日志）：与输入框同底色 —— 一眼就看得出是块可以
/// 装东西的区域。深色主题下同样由 `extreme_bg_color` 自动跟着走。
fn well<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame {
        inner_margin: egui::Margin::symmetric(10.0, 8.0),
        rounding: egui::Rounding::same(4.0),
        fill: ui.visuals().extreme_bg_color,
        stroke: hairline(ui),
        ..Default::default()
    }
    .show(ui, |ui| {
        ui.set_width(ui.available_width());
        f(ui)
    })
    .inner
}

/// 单行输入框。
///
/// **自己套一层带描边的 [`egui::Frame`]、把 `TextEdit` 自己的框关掉**
/// （`.frame(false)`）。为什么不直接用 `TextEdit` 自带的框：
/// egui 0.29 的浅色主题里**未聚焦的控件描边就是"没有"** ——
/// `Widgets::light().inactive.bg_stroke = Default::default()`，
/// 源码里紧挨着那句话是 `// TODO(emilk): we want to show something here,
/// or a text-edit field doesn't "pop".`。于是输入框只剩一层
/// `extreme_bg_color`（浅色主题下＝白）填充，落在同样接近白的卡面上就"消失"了。
/// 主界面那边靠"输入框又高又宽"还能看出边界，这里是细长条，必须自己补一条描边。
///
/// `w` 是**外框**宽度（含内边距与描边）。
fn input(ui: &mut egui::Ui, w: f32, s: &mut String, hint: &str) {
    /// 左右内边距。
    const PAD_X: f32 = 7.0;
    egui::Frame {
        inner_margin: egui::Margin::symmetric(PAD_X, 3.0),
        rounding: egui::Rounding::same(4.0),
        fill: ui.visuals().extreme_bg_color,
        stroke: hairline(ui),
        ..Default::default()
    }
    .show(ui, |ui| {
        // 宽度用 `desired_width` 而不是 `add_sized`：高度交给 TextEdit 自己定，
        // 免得把行高写死后文字被裁掉一点点。
        ui.add(
            egui::TextEdit::singleline(s)
                .hint_text(hint)
                .desired_width((w - 2.0 * PAD_X - 2.0).max(48.0))
                .frame(false),
        );
    });
}

/// 细边框用的描边。取主题里 `noninteractive.bg_stroke` 的**颜色** ——
/// 它是 egui 给"分隔线 / 卡片边"准备的那一档灰，浅深两套主题各有一份，
/// 不会在深色主题下变成一道刺眼的白线。
///
/// ⚠️ 不能取 `inactive.bg_stroke`：那一档在浅色主题下是 `Stroke::NONE`
/// （见 [`input`] 的说明）。
fn hairline(ui: &egui::Ui) -> egui::Stroke {
    egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color)
}

/// 卡片标题（蓝字，与主界面 `section()` 的标题一致）。
fn card_title(ui: &mut egui::Ui, s: &str) {
    ui.label(
        RichText::new(s)
            .size(ui_scale::SECTION)
            .strong()
            .color(ACCENT),
    );
}

/// 「标签 + 右边一整行控件」的一行。标签定宽，保证多行左边缘对齐。
///
/// `add` 拿到的是**减掉标签列之后剩下的宽度**，在这里算好再传进去。
/// ⚠️ **不要在 `ui.horizontal(..)` 内部去问 `ui.available_width()`** ——
/// 横向布局里那个值不是"剩下的这一截"，实测拿到 0，`add_sized` 于是把输入框
/// 压成 0 宽：框线画不出来，只剩一行提示文字浮在卡面上（提示文字是不裁剪的，
/// 所以这个 bug 看起来像"输入框没画背景"，很容易往配色上去查）。
fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui, f32)) {
    // 80 而不是 68：`页码范围`（4 个全角字）在 16 pt 下就要 64 px 出头，
    // 68 会把标签压到和输入框贴在一起。
    const LABEL_W: f32 = 80.0;
    let w = (ui.available_width() - LABEL_W - ui.spacing().item_spacing.x).max(120.0);
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_W, ui_scale::ROW_H),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(RichText::new(label).size(ui_scale::BODY));
            },
        );
        add(ui, w);
    });
}

/// 灰色小字。现在只用于**显示状态**（如已选图片的路径、空列表占位），
/// 不再承担"向用户解释功能"的职责 —— 那类文案按"界面清爽"的要求已全部撤掉。
fn hint(ui: &mut egui::Ui, s: &str) {
    ui.label(RichText::new(s).size(ui_scale::SMALL).color(DIM_C));
}

/// 「标题 + 按钮」那一行，右侧挂「已选 N 个」（两页的文档卡片共用）。
fn doc_header(ui: &mut egui::Ui, title: &str, count: usize, buttons: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        card_title(ui, title);
        ui.add_space(4.0);
        buttons(ui);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("已选 {count} 个"))
                    .size(ui_scale::SMALL)
                    .color(DIM_C),
            );
        });
    });
}

/// 已选文档列表（撑满宽度、定高滚动）。
fn doc_list(ui: &mut egui::Ui, id: &str, docs: &[String]) {
    well(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(id)
            .max_height(150.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if docs.is_empty() {
                    ui.label(RichText::new("（未选）").color(DIM_C).small());
                }
                for (i, d) in docs.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{:>3}.", i + 1))
                                .size(ui_scale::SMALL)
                                .color(DIM_C),
                        );
                        ui.label(RichText::new(file_name(d)).size(ui_scale::SMALL));
                    });
                }
            });
    });
}

/// 取文件名（去掉路径）。
fn file_name(p: &str) -> String {
    p.rsplit(['\\', '/']).next().unwrap_or(p).to_string()
}
