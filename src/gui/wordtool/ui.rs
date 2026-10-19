//! 页眉替换（文本 / 图片）与「打印与导出」两页 —— 界面 + 任务调度。
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
//! （图片替换 / 文本替换 / 打印与导出），这里把前两页**并成「页眉替换」一页**、
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
use wrepl::hdr;

use super::com::ComInit;
use super::engine::{self, EngineProbe, WordKind};
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
    /// 打印与导出页的「选择文档」
    Print,
    /// 页眉替换页的「选择图片…」（**只选一张**）。
    ///
    /// 2026-10-11 起并入自绘选择器 —— 原来它单独弹 `rfd` 系统对话框，
    /// 是本程序里最后一个绕开自绘面板的"选文件"入口。结果落到
    /// [`WordTool::img_path`]（单值，不是列表），见 [`WordTool::set_image`]。
    HeaderImage,
}

/// 页眉替换的两种子模式。
#[derive(PartialEq, Clone, Copy)]
pub enum HeaderMode {
    Text,
    Image,
}

/// 「打印与导出」页的下游：送打印机，还是落成 PDF 文件。
///
/// 两条路的输入完全一样（同一份文档清单、同一个「页码范围」字段），
/// 差别只在**产物去哪**。所以它们是同一页上的一个单选，而不是两页 ——
/// 分成两页等于让用户把同一批文档选两遍，而实际交付里"先打一份纸质、
/// 再导一份 PDF 归档"是连着做的。
///
/// 页码语义两边**不完全一样**（这是它们唯一的实质差异）：
/// 打印按 `"1-3,5,7-9"` 原样交给 Word，多段随便写；
/// 导出走 `ExportAsFixedFormat`，一次只认**一段连续**页码 —— 见
/// [`super::services::parse_pdf_pages`]。
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Sink {
    Printer,
    Pdf,
}

/// 点「执行」这一刻该怎么走。**纯判断，不碰 Word、不碰界面状态** ——
/// 所以能直接对着假数据测，不需要开窗（见本文件末尾的测试）。
///
/// 这个枚举存在的理由：引擎探测（[`engine`]）与界面动作之间必须有一道
/// **显式**的分叉。"偷偷降级"正是要防的东西，把它写成一个类型，
/// 就没有地方能"顺手"绕过去。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClickPlan {
    /// 照常跑，用这个引擎。
    Run(WordKind),
    /// 只有 WPS：**先弹确认框**，使用者点过才跑。
    ConfirmWps,
    /// 这条下游干不了，附一句人话说明。
    Blocked(&'static str),
}

/// 决策：这一档下游 + 本机引擎状况 → 该怎么走。
///
/// 打印**不接受** WPS 降级：WPS 的 `PrintOut` 没实测过，而"打到纸上"不可撤销。
/// 导出接受，但必须先确认（分页会变，见 [`engine::WPS_CONFIRM_BODY`]）。
pub fn plan_click(sink: Sink, probe: EngineProbe) -> ClickPlan {
    match (sink, probe) {
        // 有真 Word：一切照旧，一个字都不提示
        (_, EngineProbe::Word) => ClickPlan::Run(WordKind::Word),
        // 只有 WPS：导出问一声，打印直接拦
        (Sink::Pdf, EngineProbe::WpsOnly) => ClickPlan::ConfirmWps,
        (Sink::Printer, EngineProbe::WpsOnly) => ClickPlan::Blocked(engine::PRINT_NEEDS_WORD),
        // 什么都没有
        (_, EngineProbe::None) => ClickPlan::Blocked(engine::NO_ENGINE),
    }
}

/// 确认框点「继续」之后要跑的那一趟。
///
/// 为什么把参数**先落定**再弹框：确认框和点击之间隔着若干帧，这期间用户
/// 可能切了页码、改了输出目录。若等确认后再从界面读，跑的就是"另一趟活"了。
struct PendingExport {
    docs: Vec<String>,
    out: Option<std::path::PathBuf>,
    pages: services::PdfPages,
    pdf_a: bool,
}

/// 页眉替换的落盘位置 —— 就一个勾选框，所以界面侧用 `bool` 就够
/// （见 [`WordTool::hdr_in_place`]），拼活的时候再翻成 [`hdr::Out`]。
///
/// 为什么不直接存 `hdr::Out`：勾选框要的是 `&mut bool`，为它绕一层 enum
/// 只会多出"两个真相"（勾选框与 enum 谁说了算）。
///
/// ## 一道要跑的活
///
/// 为什么不是直接存闭包：闭包只能跑一次，而"就地覆盖"要先弹确认框、
/// 用户点了确认才真跑 —— 中间隔着若干帧。所以这里存的是**参数**，
/// 真要跑的时候现拼闭包（见 [`WordTool::spawn_job`]）。
struct HeaderJob {
    /// 改什么（文本 / 图片）
    action: hdr::Action,
    /// 落哪（就地覆盖 / 写到副本）
    out: hdr::Out,
}

/// 后台线程回传的消息。
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
    /// 直接改写源文件？（默认 **false = 写到源文件旁的 out 子目录**）
    ///
    /// 界面侧用 `bool` 而不是 `hdr::Out`：勾选框要的就是 `&mut bool`，
    /// 绕一层 enum 只会多出"两个真相"（勾选框与 enum 谁说了算）。
    /// 拼活时再翻成 [`hdr::Out`]（见 [`WordTool::build_job`]）。
    ///
    /// 默认**不勾**：这一页处理的是交付件，先拿副本验证比事后恢复便宜得多。
    pub hdr_in_place: bool,
    /// 「就地覆盖」的确认框是否打开。
    ///
    /// `pub` 是为了让无头自检能把它置上、从而**照到确认框那一帧** ——
    /// 本机的合成鼠标事件进不到窗口（见 `diag` 模块头），不这么做这条分支
    /// 永远没被渲染过。运行期只有本页自己写它。
    pub hdr_confirm: bool,
    /// 等确认后要跑的活（确认框点"确认"时取走）。
    hdr_pending: Option<HeaderJob>,
    pub hdr_docs: Vec<String>,
    pub text_en: String,
    pub text_cn: String,
    pub img_path: String,
    pub img_scale: bool,
    // ---------- 打印与导出 ----------
    pub print_docs: Vec<String>,
    /// 页码范围。**两种下游共用这一个字段**（打印原样交给 Word，导出先解析，见 [`Sink`]）。
    pub page_range: String,
    /// 这一页的下游：送打印机，还是落 PDF。
    pub sink: Sink,
    /// 导出 PDF 的输出目录。**留空 = 每份文档旁的 `pdf/` 子目录**。
    pub pdf_dir: String,
    /// 勾了就按 PDF/A 归档格式导出 —— 走 Word 的 `UseISO19005_1` 开关。
    /// **实测（Microsoft 365 / 2026-10-19）产出 PDF/A-3b**，不是 1b；
    /// 界面文案因此不写 ISO 编号，改在悬停说明里讲清楚。
    pub pdf_a: bool,
    /// 本机打印机（进入打印页时枚举一次并缓存）。
    pub printers: Vec<Printer>,
    /// 下拉框选择：`0` = 用系统默认；`n>0` = `printers[n-1]`。
    pub printer_pick: usize,
    /// 枚举打印机失败的原因（显示在页面上，而不是让下拉框空着让人猜）。
    pub printers_err: Option<String>,
    printers_loaded: bool,
    /// 本机能用哪套 Word 兼容引擎。**进本页时探一次**（只读注册表，几微秒），
    /// `None` = 还没探过。装了 Office 之后可以用「重新检测」刷新。
    pub engine: Option<EngineProbe>,
    /// 「改用 WPS 导出」的确认框是否打开。
    ///
    /// `pub` 是为了让无头自检能把这一帧照下来 —— 本机的合成鼠标事件进不到
    /// 窗口，不主动置上就永远渲染不到这个弹窗。
    pub wps_confirm: bool,
    /// 确认后要跑的那一趟（点「继续」时取走）。见 [`PendingExport`]。
    wps_pending: Option<PendingExport>,
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
            hdr_in_place: false,
            hdr_confirm: false,
            hdr_pending: None,
            hdr_docs: Vec::new(),
            text_en: String::new(),
            text_cn: String::new(),
            img_path: String::new(),
            img_scale: true,
            print_docs: Vec::new(),
            page_range: String::new(),
            sink: Sink::Printer,
            pdf_dir: String::new(),
            pdf_a: false,
            printers: Vec::new(),
            printer_pick: 0,
            printers_err: None,
            printers_loaded: false,
            engine: None,
            wps_confirm: false,
            wps_pending: None,
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
            // 图片是**单值**，走 [`WordTool::set_image`]；这里收不到它。
            PickDoc::HeaderImage => {}
        }
    }

    /// 自绘选择器选完图片之后，由 `App` 回填（只取第一张）。
    pub fn set_image(&mut self, path: String) {
        self.img_path = path;
    }

    /// 已选的文档（供 `App` 开选择器时猜起点目录）。
    pub fn docs_of(&self, what: PickDoc) -> &[String] {
        match what {
            PickDoc::Header => &self.hdr_docs,
            PickDoc::Print => &self.print_docs,
            // 图片只一张，没有"已选列表"；把当前那张借出去当起点 ——
            // 再选一次时会停在上次选的那张图旁边，而不是回到默认目录。
            // 空串时 `App` 那边的 `doc_start` 会当作"没有起点"处理。
            PickDoc::HeaderImage => std::slice::from_ref(&self.img_path),
        }
    }

    /// 往界面日志里追加一行，**并同时镜像到 `wrepl-gui.log`**。
    ///
    /// 镜像为什么收口在这里、而不是各调用点各写一遍：界面线程自己发的那些行
    /// （"开始页眉文本替换…：N 个文档"这类，记录的正是**这一趟请求了什么**）
    /// 原来只落在界面上，关窗即失；工作线程回传的行（`WtMsg::Log`）倒是镜像了。
    /// 两头不齐的直接后果：日志里看得到"已完成：x.docx"，却看不出用的是哪套引擎、
    /// 请求的是什么内容 —— 排查"替换没生效"时最需要的那半句恰好缺了。
    ///
    /// 收口到一处，两个来源都不会漏；`WtMsg::Log` 那边也不要再单独镜像，
    /// 否则工作线程的每一行都会落两遍。
    fn log(&mut self, s: String) {
        crate::diag::log(format!("[wordtool] {s}"));
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

    /// 在独立线程里跑一趟活。
    ///
    /// `verb` 只用于日志；`need_com` 决定要不要在这个线程上初始化 STA。
    ///
    /// **纯 Rust 的活必须走 `need_com = false`**：`ComInit::sta()` 会 `CoInitializeEx`
    /// 一次，那本身无害，但一旦失败（没装 Word、被管控套件拦了）就会在**真正开跑之前**
    /// 直接把整趟活判死 —— 而纯 Rust 根本不需要 COM。页眉替换改成纯 Rust 之后，
    /// 这里是最容易被漏掉的一根线头。
    fn start<F>(&mut self, verb: &str, need_com: bool, f: F)
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
            if need_com {
                match ComInit::sta() {
                    Ok(c) => {
                        // 保活到活干完（Drop 时 CoUninitialize）
                        let (ok, fail) = run_body(f, &tx);
                        drop(c);
                        let _ = tx.send(WtMsg::Done {
                            verb: verb_s,
                            ok,
                            fail,
                        });
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(WtMsg::Log(format!("COM 初始化失败：{e}")));
                        let _ = tx.send(WtMsg::Done {
                            verb: verb_s,
                            ok: 0,
                            fail: 0,
                        });
                        return;
                    }
                }
            }
            let (ok, fail) = run_body(f, &tx);
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
                WtMsg::Log(s) => {
                    // 镜像收口在 `WordTool::log` 里，这里只管把它塞进界面日志。
                    // 这里再镜像一次的话，工作线程的每行都会落两遍。
                    self.log(s);
                }
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
                        // 选图片和「选择文档」走同一台自绘选择器（`PickDoc::HeaderImage`）：
                        // 原来它是全程序最后一个还在弹系统对话框的入口。
                        if ui
                            .add(
                                ds::Button::secondary("选择图片…")
                                    .leading(ds::Icon::FolderOpen)
                                    .disabled(busy),
                            )
                            .clicked()
                        {
                            self.want_pick = Some(PickDoc::HeaderImage);
                        }
                        ui.add_space(ds::SPACING.s4);
                        ui.add(ds::Checkbox::with_label(&mut self.img_scale, "按表格高度等比缩放"));
                    });
                    ui.add_space(ds::SPACING.s3);
                    // 图片没有对应的输入框，所以这里必须留一处能看见"到底选了哪张"
                    // —— 只显示路径本身，不加前缀说明文字。
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

        // ---- 卡片 3：落盘方式 ----
        //
        // 为什么把"改什么"和"怎么改"分开成两张卡：上面那张卡是内容（文本 / 图片），
        // 这张是**手段**（产物落哪）。两件事的"要不要动"彼此无关，
        // 混在一张卡里会让人以为换个落点要重填内容。
        //
        // 2026-10-19：引擎单选已删（Word COM 那条路退役，见 CHANGELOG），
        // 这张卡只剩落盘一个勾选框。
        card(ui, |ui| {
            card_title(ui, "落盘方式");
            ui.add(ds::Checkbox::with_label(
                &mut self.hdr_in_place,
                "直接改写源文件（不勾 = 写到源文件旁的 out 子目录）",
            ));
        });

        ui.add_space(ds::SPACING.s4);

        // ---- 卡片 4：执行 ----
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
                    let job = self.build_job();
                    // 就地覆盖 = 唯一"可能弄坏源文件"的路径，先确认一次再跑。
                    // 引擎内部还有 self_check 兜底（写回原字节），但那是**出错时**
                    // 的保护；确认框管的是"我确实要这个动作"。
                    self.hdr_confirm = matches!(job.out, hdr::Out::InPlace);
                    self.hdr_pending = Some(job);
                    if !self.hdr_confirm {
                        self.spawn_job();
                    }
                }
                ui.add_space(ds::SPACING.s4);
                self.status_rows(ui);
            });
        });

        ui.add_space(ds::SPACING.s4);
        self.log_section(ui);

        // 确认框最后画：它要浮在整页之上（含底部那排动作按钮）。
        self.hdr_confirm_dialog(ui.ctx());
    }

    /// 按界面上的当前选择拼出这一趟要跑的活。**只读不写** —— 尤其不能在这里
    /// 动 `hdr_pending`：`build_job` 之后紧跟着就是"要不要先确认"的判断，
    /// 顺手把活存进去、又顺手跑掉，就会变成"确认框还没点、文件已经改了"。
    fn build_job(&self) -> HeaderJob {
        let action = match self.hdr_mode {
            HeaderMode::Text => hdr::Action::Text {
                en: self.text_en.clone(),
                cn: self.text_cn.clone(),
            },
            HeaderMode::Image => hdr::Action::Image {
                path: std::path::PathBuf::from(&self.img_path),
                scale_to_table: self.img_scale,
            },
        };
        let out = if self.hdr_in_place {
            hdr::Out::InPlace
        } else {
            hdr::Out::SiblingOut
        };
        HeaderJob { action, out }
    }

    /// 把 [`WordTool::hdr_pending`] 里那道活开跑。没有活就什么也不做。
    fn spawn_job(&mut self) {
        let Some(HeaderJob { action, out }) = self.hdr_pending.take() else {
            return;
        };
        let docs = self.hdr_docs.clone();
        let kind = mode_cn(self.hdr_mode);
        let verb = format!("页眉{kind}替换");
        self.log(format!(
            "开始{verb}：{} 个文档；落盘：{}",
            docs.len(),
            match out {
                hdr::Out::InPlace => "就地覆盖源文件",
                hdr::Out::SiblingOut => "写到各源文件旁的 out 子目录（源文件不动）",
            }
        ));
        // ★ need_com = false：页眉替换不需要 Word，也不该因为
        //   `CoInitializeEx` 失败而整趟不做。
        self.start(&verb, false, move |log, prog| {
            hdr::run(&docs, &action, out, log, prog)
        });
    }

    /// 「就地覆盖」确认框。只做一件事：在真的动源文件之前停一下。
    fn hdr_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.hdr_confirm {
            return;
        }
        match ds::ConfirmDialog::new(
            "直接改写源文件？",
            "此操作不可撤销：选中的文档会被逐个就地覆盖，且不额外留备份。\
             \n不放心就取消，把「直接改写源文件」的勾去掉 —— \
             产物会写到每份文档旁的 out\\ 子目录，源文件一个字节不动。",
        )
        .danger()
        .confirm_label("确认改写")
        .cancel_label("取消")
        .show(ctx)
        {
            // 先关框、再开跑，否则下一帧还会画这个确认框。
            Some(true) => {
                self.hdr_confirm = false;
                self.spawn_job();
            }
            Some(false) => {
                self.hdr_confirm = false;
                self.hdr_pending = None;
            }
            None => {}
        }
    }

    // ==================== 渲染：打印与导出 ====================

    pub fn ui_print(&mut self, ui: &mut egui::Ui) {
        // 打印机只枚举一次并缓存 —— 见 `reload_printers`。
        if !self.printers_loaded {
            self.reload_printers();
        }
        // 引擎同样只探一次：**只读注册表**，不启动 Word / WPS，几微秒返回。
        // 装了 Office 之后由卡片上那个「重新检测」刷新。
        if self.engine.is_none() {
            self.engine = Some(engine_override().unwrap_or_else(EngineProbe::detect));
        }
        let probe = self.engine.unwrap_or(EngineProbe::None);
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

        // ---- 引擎状况：只在"不是真 Word"时出现 ----
        //
        // 有 Word 的机器上**一个字都不多写** —— 提示只该出现在真需要判断的地方。
        if probe != EngineProbe::Word {
            ui.add_space(ds::SPACING.s4);
            card(ui, |ui| {
                let (level, body) = match probe {
                    EngineProbe::WpsOnly => (ds::Level::Warning, engine::WPS_ONLY_BANNER),
                    _ => (ds::Level::Error, engine::NO_ENGINE),
                };
                ui.add(ds::Alert::new(level, body));
                ui.add_space(ds::SPACING.s2);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            ds::Button::ghost("重新检测")
                                .leading(ds::Icon::Refresh)
                                .disabled(busy),
                        )
                        .on_hover_text("装完 Office / WPS 后点一下")
                        .clicked()
                    {
                        self.engine = Some(EngineProbe::detect());
                    }
                    let pal = ds::palette_of(ui.ctx());
                    ui.label(
                        RichText::new(format!("当前判定：{}", probe.label_cn()))
                            .text_style(egui::TextStyle::Small)
                            .color(pal.text_secondary),
                    );
                });
            });
        }

        ui.add_space(ds::SPACING.s4);

        // ---- 卡片 2：输出设置（打印机 / PDF）----
        card(ui, |ui| {
            card_title(ui, "输出设置");
            // 两种下游的输入完全一样，只有产物去处不同 —— 所以是同一页上的
            // 一个单选，不是两页（见 [`Sink`] 的说明）。
            ds::RadioGroup::new(&mut self.sink)
                .horizontal()
                .option(ds::RadioOption::new(Sink::Printer, "打印机"))
                .option(ds::RadioOption::new(Sink::Pdf, "导出 PDF"))
                .show(ui);
            ui.add_space(ds::SPACING.s3);

            let sink = self.sink;
            match sink {
                Sink::Printer => {
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
                        .on_hover_text("重新枚举本机打印机")
                        .clicked()
                    {
                        self.reload_printers();
                    }
                }
                // 导出 PDF 不碰打印机：**一个系统窗口都不建**，也就没有
                // "选打印机"这一步。目录留空时落到各文档旁的 `pdf/`。
                Sink::Pdf => {
                    ds::InputField::new(&mut self.pdf_dir)
                        .label("输出目录")
                        .placeholder("留空 = 每份文档旁的 pdf 子目录")
                        .desired_width(420.0)
                        .show(ui);
                    ui.add_space(ds::SPACING.s2);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                ds::Button::secondary("用源文件目录")
                                    .leading(ds::Icon::FolderOpen)
                                    .disabled(busy),
                            )
                            .on_hover_text("填第一份文档所在目录")
                            .clicked()
                        {
                            if let Some(p) = self.print_docs.first() {
                                if let Some(parent) = std::path::Path::new(p).parent() {
                                    self.pdf_dir = parent.to_string_lossy().to_string();
                                }
                            }
                        }
                        ui.add_space(ds::SPACING.s4);
                        ui.add(ds::Checkbox::with_label(&mut self.pdf_a, "PDF/A 归档格式"))
                            .on_hover_text("归档用 PDF/A，体积更大。日常传阅不必勾。");
                    });
                }
            }

            ui.add_space(ds::SPACING.s3);

            // 页码：两档共用同一个字段。导出那档先解析 —— 解析不了就把原因画在
            // 输入框下面，**绝不弹窗、也不静默按"全部"跑**。
            let pages_err = match sink {
                Sink::Printer => None,
                Sink::Pdf => services::parse_pdf_pages(&self.page_range).err(),
            };
            let hint_txt = match sink {
                Sink::Printer => "留空 = 全部；也可写 1-3,5,7-9",
                Sink::Pdf => "留空 = 全部；也可写 3 或 3-7",
            };
            let mut pages_field = ds::InputField::new(&mut self.page_range)
                .label("页码范围")
                .placeholder(hint_txt)
                .desired_width(180.0);
            if let Some(e) = pages_err.as_deref() {
                pages_field = pages_field.error(e);
            }
            pages_field.show(ui);

            // 说明性文案一律不摆 —— 下拉框本身就能看出有几台、选的是哪台。
            // 只有"枚举失败"必须说出来，否则下拉框空着没人知道为什么。
            if sink == Sink::Printer {
                if let Some(e) = &self.printers_err {
                    let pal = ds::palette_of(ui.ctx());
                    ui.add_space(ds::SPACING.s3);
                    ui.label(
                        RichText::new(format!("× 读取打印机列表失败：{e}"))
                            .text_style(egui::TextStyle::Small)
                            .color(pal.error),
                    );
                }
            }
        });

        ui.add_space(ds::SPACING.s4);

        // ---- 卡片 3：执行 ----
        let pages_ok = match self.sink {
            Sink::Printer => true,
            Sink::Pdf => services::parse_pdf_pages(&self.page_range).is_ok(),
        };
        let plan = plan_click(self.sink, probe);
        // 引擎干不了这一档（只有 WPS 却要打印 / 两样都没装）时，按钮也要灰掉 ——
        // 让它可点、点完只写一行日志，比直接灰掉更让人困惑。
        let can = !busy && !self.print_docs.is_empty() && pages_ok
            && !matches!(plan, ClickPlan::Blocked(_));
        card(ui, |ui| {
            // 灰着的按钮不说话最招人烦：把原因先摆在按钮上面。
            if let ClickPlan::Blocked(why) = plan {
                let pal = ds::palette_of(ui.ctx());
                ui.label(
                    RichText::new(format!("× {why}"))
                        .text_style(egui::TextStyle::Small)
                        .color(pal.error),
                );
                ui.add_space(ds::SPACING.s2);
            }
            ui.horizontal(|ui| {
                // 一屏仍然只有一个 primary —— 换下游只换措辞与图标。
                let (action_label, action_icon) = match self.sink {
                    Sink::Printer => ("开始打印", egui_phosphor::regular::PRINTER),
                    Sink::Pdf => ("导出 PDF", egui_phosphor::regular::FILE_PDF),
                };
                let clicked = ui
                    .add(
                        ds::Button::primary(action_label)
                            .size(ds::ButtonSize::Lg)
                            // 图标走 Phosphor：DS 的 `Icon` 枚举里没有打印机 / PDF
                            // 语义（原先借的是 `Package` —— 一个包裹/箱子，与"打印"
                            // 没关系）。`Icon::Glyph` 是 egui_sauge 特意留的逃生口，
                            // 直接塞 Phosphor 码点即可，不用去动 ds.rs 的公共 API
                            // （GUIDE §12 也是这么说的）。
                            .leading(ds::Icon::Glyph(action_icon))
                            .disabled(!can),
                    )
                    .clicked();
                if clicked {
                    // 引擎分叉**只有这一处**：有 Word 就照旧，只有 WPS 就先问，
                    // 什么都没有就明说。判断逻辑在 `plan_click` 里，能单测。
                    match plan_click(self.sink, probe) {
                        ClickPlan::Blocked(why) => {
                            self.log(format!("没有执行：{why}"));
                        }
                        ClickPlan::ConfirmWps => {
                            // 走到这里一定是「导出 PDF + 只有 WPS」——
                            // 打印那一档已被 `plan_click` 拦在外面。
                            self.stage_wps_export();
                        }
                        ClickPlan::Run(kind) => match self.sink {
                            Sink::Printer => self.launch_print(),
                            Sink::Pdf => self.launch_export(kind),
                        },
                    }
                }
                ui.add_space(ds::SPACING.s4);
                self.status_rows(ui);
            });
        });

        ui.add_space(ds::SPACING.s4);
        self.log_section(ui);

        // 确认框最后画：它要浮在整页之上（含底部那排动作按钮）。
        self.wps_confirm_dialog(ui.ctx());
    }

    // ---------- 打印与导出：三条出口 ----------

    /// 「打印机」这一档开跑。只有真 Word 会走到这里（见 [`plan_click`]）——
    /// WPS 的 `PrintOut` 没实测过，而"打到纸上"不可撤销，不赌。
    fn launch_print(&mut self) {
        let docs = self.print_docs.clone();
        let range = self.page_range.trim().to_string();
        // 打印机字符串在这里定下来（"名称 on 端口"）—— 线程里不再碰 UI 状态。
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
        self.start("打印", true, move |log, prog| {
            services::print_run(&docs, &range, printer.as_deref(), log, prog)
        });
    }

    /// 「导出 PDF」这一档开跑（真 Word，或使用者已经在确认框里点过"继续"）。
    fn launch_export(&mut self, kind: WordKind) {
        // 按钮在解析失败时本来就是灰的，这里再守一道：
        // 界面状态与点击之间隔着若干帧，别指望它一定同步。
        let pages = match services::parse_pdf_pages(&self.page_range) {
            Ok(p) => p,
            Err(e) => {
                self.log(format!("页码范围不能用，已停止：{e}"));
                return;
            }
        };
        let job = self.pending_from_ui(pages);
        self.log(format!(
            "开始导出 PDF：{} 个文档（{}；输出到 {}；{}；{}）",
            job.docs.len(),
            job.pages.describe(),
            where_text(&job.out),
            if job.pdf_a {
                "PDF/A 归档格式"
            } else {
                "标准 PDF"
            },
            kind.label_cn(),
        ));
        self.run_export(job, kind);
    }

    /// 只有 WPS 时：把这一趟的参数**定住**、打开确认框 —— **不跑**。
    ///
    /// 为什么不在确认后再从界面读参数：确认框与点击之间隔着若干帧，
    /// 这期间用户可能改了页码或输出目录，那样跑的就是"另一趟活"了。
    fn stage_wps_export(&mut self) {
        let pages = match services::parse_pdf_pages(&self.page_range) {
            Ok(p) => p,
            Err(e) => {
                self.log(format!("页码范围不能用，已停止：{e}"));
                return;
            }
        };
        let job = self.pending_from_ui(pages);
        self.log(format!(
            "待确认：WPS 导出 · {} 个文档 · {} · {}",
            job.docs.len(),
            job.pages.describe(),
            where_text(&job.out),
        ));
        self.wps_pending = Some(job);
        self.wps_confirm = true;
    }

    /// 从界面当前状态摘出这一趟的参数（不启动任何任务）。
    fn pending_from_ui(&self, pages: services::PdfPages) -> PendingExport {
        let dir = self.pdf_dir.trim().to_string();
        PendingExport {
            docs: self.print_docs.clone(),
            out: if dir.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(&dir))
            },
            pages,
            pdf_a: self.pdf_a,
        }
    }

    /// 真正把导出丢进后台线程。**两条入口共用**（直接跑 / 确认后跑），
    /// 免得"确认后跑"那条悄悄少一个参数。
    fn run_export(&mut self, job: PendingExport, kind: WordKind) {
        let PendingExport {
            docs,
            out,
            pages,
            pdf_a,
        } = job;
        self.start("导出 PDF", true, move |log, prog| {
            services::export_pdf_run(&docs, out.as_deref(), pages, pdf_a, kind, log, prog)
        });
    }

    /// 「改用 WPS 导出」的确认框。
    fn wps_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.wps_confirm {
            return;
        }
        match ds::ConfirmDialog::new("改用 WPS 导出？", engine::WPS_CONFIRM_BODY)
            .danger()
            .confirm_label("仍要导出")
            .cancel_label("取消")
            .show(ctx)
        {
            // 先关框、再开跑，否则下一帧还会画这个确认框。
            Some(true) => {
                self.wps_confirm = false;
                if let Some(job) = self.wps_pending.take() {
                    self.log("改用 WPS 导出（分页与 Word 不同）".to_string());
                    self.run_export(job, WordKind::Wps);
                }
            }
            Some(false) => {
                self.wps_confirm = false;
                self.wps_pending = None;
                self.log("已取消导出".to_string());
            }
            None => {}
        }
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

/// 页眉替换子模式的中文名（日志用）。
fn mode_cn(m: HeaderMode) -> &'static str {
    match m {
        HeaderMode::Text => "文本",
        HeaderMode::Image => "图片",
    }
}

/// 把一趟活的日志 / 进度接到 channel 上，然后跑它。
///
/// 抽出来是因为 [`WordTool::start`] 现在有两个入口（要 COM / 不要 COM），
/// 而"接线"这一段两者一模一样。留两份只会让哪天改日志格式时漏掉一支。
fn run_body<F>(f: F, tx: &mpsc::Sender<WtMsg>) -> (i32, i32)
where
    F: FnOnce(&mut dyn FnMut(String), &mut dyn FnMut(usize, usize)) -> (i32, i32),
{
    f(
        &mut |s: String| {
            let _ = tx.send(WtMsg::Log(s));
        },
        &mut |cur: usize, total: usize| {
            let _ = tx.send(WtMsg::Progress {
                current: cur,
                total,
            });
        },
    )
}

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

/// 输出目录的人话描述（留空 = 跟着源文件走）。日志里用。
fn where_text(out: &Option<std::path::PathBuf>) -> String {
    match out {
        Some(p) => p.display().to_string(),
        None => "每份文档旁的 pdf 子目录".to_string(),
    }
}

/// 诊断开关：`WREPL_DEBUG_ENGINE=wps|none|word` 强制指定引擎判定。
///
/// 只为一种场景存在：**手上这台机器装了 Word，却要看"只有 WPS"时的界面长什么样**
/// （截图、演示、给客户说明）。真机上永远走 [`EngineProbe::detect`]。
/// 与既有的 `WREPL_DEBUG_PAGE` / `WREPL_DEBUG_PICKER` 同类。
fn engine_override() -> Option<EngineProbe> {
    let v = std::env::var("WREPL_DEBUG_ENGINE").ok()?;
    match v.trim().to_ascii_lowercase().as_str() {
        "wps" => Some(EngineProbe::WpsOnly),
        "none" => Some(EngineProbe::None),
        "word" => Some(EngineProbe::Word),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 有真 Word：导出照旧，不需要任何确认。
    #[test]
    fn word_engine_exports_without_asking() {
        assert_eq!(
            plan_click(Sink::Pdf, EngineProbe::Word),
            ClickPlan::Run(WordKind::Word)
        );
    }

    /// ★ 只有 WPS：**必须**先问一声，不许直接跑。
    #[test]
    fn wps_engine_asks_before_exporting() {
        assert_eq!(
            plan_click(Sink::Pdf, EngineProbe::WpsOnly),
            ClickPlan::ConfirmWps
        );
    }

    /// 只有 WPS 时打印直接拦下来 —— 打错机器/打出错件的代价不可撤销。
    #[test]
    fn wps_engine_blocks_printing() {
        match plan_click(Sink::Printer, EngineProbe::WpsOnly) {
            ClickPlan::Blocked(why) => assert!(why.contains("Microsoft Word"), "{why}"),
            other => panic!("只有 WPS 时打印不该放行，实际：{other:?}"),
        }
    }

    /// 两样都没装：两条下游都拦，且理由指向"装一个"。
    #[test]
    fn no_engine_blocks_both() {
        for sink in [Sink::Printer, Sink::Pdf] {
            match plan_click(sink, EngineProbe::None) {
                ClickPlan::Blocked(why) => assert!(why.contains("装"), "{why}"),
                other => panic!("什么都没装却放行了 {sink:?}：{other:?}"),
            }
        }
    }

    /// 有 Word 的机器上，打印**永远不会**走到确认框 —— 别把日常路径也拦一下。
    #[test]
    fn word_engine_prints_directly() {
        assert_eq!(
            plan_click(Sink::Printer, EngineProbe::Word),
            ClickPlan::Run(WordKind::Word)
        );
    }
}
