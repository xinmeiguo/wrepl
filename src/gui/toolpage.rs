//! 「诊断工具」页：命令行那六个只看不改的子命令，在界面上也有一份入口。
//!
//! ## 为什么这一页存在
//!
//! `wrepl inspect / passthrough / diff / dump / probe / verify --batch` 一直是
//! "命令行才有"的能力（`docs/ROADMAP.md` §7.1）。可它们是**同一件工具的排障面**：
//! 有人说"这份产物怎么改了这么多"，第一步就是 `diff` 原件与产物；换了台机器
//! 想确认拷贝链路没动字节，就是 `passthrough`。会用到这些的时候，往往正是
//! 不方便开终端的时候。
//!
//! ## 实现上的两条硬约束
//!
//! 1. **报告文本由 `wrepl::tools` 生成**，这一页只负责画。界面另写一套解析，
//!    同一个文件在两边会给出不同结论 —— 那是最难查的那种 bug。
//! 2. **跑在后台线程**。`probe` 扫一个 26 个真实交付包的目录要几秒，`verify`
//!    批量更要逐个开 zip；放界面线程里就是"窗口不动了"。

use crate::ds;
use eframe::egui::{self, RichText};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use wrepl::tools::{self, ToolOutput};

use crate::app::{DIM_C, ERR_C, OK_C, WARN_C};

/// 哪一件工具。顺序即界面上的顺序（从"只看一个文件"到"看两个目录"）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Inspect,
    Passthrough,
    Diff,
    Dump,
    Probe,
    VerifyBatch,
}

impl Kind {
    const ALL: [Kind; 6] = [
        Kind::Inspect,
        Kind::Passthrough,
        Kind::Diff,
        Kind::Dump,
        Kind::Probe,
        Kind::VerifyBatch,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Inspect => "part 清单",
            Kind::Passthrough => "零改动透传",
            Kind::Diff => "part 比对",
            Kind::Dump => "段落转储",
            Kind::Probe => "项目侦察",
            Kind::VerifyBatch => "批量验证",
        }
    }

    /// 一句话说明"它回答什么问题"。放在按钮下面，省掉猜。
    fn hint(self) -> &'static str {
        match self {
            Kind::Inspect => "这个 docx 里有哪些 part，各多大、指纹是多少",
            Kind::Passthrough => "读进来再写出去，逐 part 比 SHA256 —— 拷贝链路的体检",
            Kind::Diff => "两个 docx 的 part 级差异在哪",
            Kind::Dump => "每个段落是怎么被切成 run 的（人工核对偏移）",
            Kind::Probe => "一个目录里出现过哪些项目编号 / 客户名（出规则前先侦察）",
            Kind::VerifyBatch => "两个目录成对验证关卡 1+2：part 字节 + XML 骨架",
        }
    }

    fn command(self) -> &'static str {
        match self {
            Kind::Inspect => "wrepl inspect <文件>",
            Kind::Passthrough => "wrepl passthrough <输入> <输出>",
            Kind::Diff => "wrepl diff <A> <B>",
            Kind::Dump => "wrepl dump <文件>",
            Kind::Probe => "wrepl probe <目录>",
            Kind::VerifyBatch => "wrepl verify <原目录> <产物目录> --batch",
        }
    }

    /// A 那一栏要的是目录还是文件。
    pub fn a_is_dir(self) -> bool {
        matches!(self, Kind::Probe | Kind::VerifyBatch)
    }

    /// B 那一栏是什么（`None` = 这个工具不要 B）。
    pub fn b_role(self) -> Option<Role> {
        match self {
            Kind::Inspect | Kind::Dump | Kind::Probe => None,
            Kind::Passthrough => Some(Role::OutFile),
            Kind::Diff => Some(Role::InFile),
            Kind::VerifyBatch => Some(Role::InDir),
        }
    }
}

/// 界面上某个路径槽该按什么挑。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    InFile,
    InDir,
    OutFile,
}

/// 一次运行的结果：成功（文本 + 结论）或直接的错误。
type Done = Result<ToolOutput, String>;

/// 「诊断工具」页的全部状态。
pub struct ToolPage {
    pub kind: Kind,
    /// A：文件或目录（含义随工具变，标签在界面上写清楚）。
    pub a: String,
    /// B：第二份输入 / 输出文件 / 产物目录。
    pub b: String,
    /// `dump` 专用：只看某个 part。
    pub part: String,
    /// `dump` 专用：只看可见文本包含该子串的段落。
    pub grep: String,
    /// `dump` 专用：每个 part 最多显示多少段（空 = 默认 20，`0` = 不限）。
    pub limit: String,
    /// `dump` 专用：是否显示空元素 / 修订删除 / 域代码。
    pub special: bool,
    /// `probe` 专用：递归子目录。
    pub recursive: bool,
    /// `probe` 专用：把建议规则写成文件（留空 = 只显示在结果里）。
    pub out: String,
    /// 后台线程的结果。
    result: Option<Done>,
    /// 后台线程的接收端（`Some` = 正在跑）。
    rx: Option<Receiver<Done>>,
    /// 起跑时刻，用来显示"已跑了多久"。
    started: Option<Instant>,
    /// 三个"要开选择器"的请求位。**主按钮 = 系统对话框**。
    ///
    /// 用 bool 而不是直接调 `App::pick_system`：这一页拿不到 `&mut App`，
    /// 而把 `&mut App` 传进来会让两边互相借（历史上踩过）。App 每帧看一眼，
    /// 消费掉就置回 false。
    pub pick_a: bool,
    pub pick_b: bool,
    pub pick_out: bool,
    /// 同一套槽位的**旁路**：三个按钮右边的「⋯」= 自绘面板。
    ///
    /// 与上面三个分开记，是因为 `App` 那边是两个不同的函数
    /// （`pick_system` / `pick_own`），混在一个 bool 里判容易判反。
    pub pick_a_own: bool,
    pub pick_b_own: bool,
    pub pick_out_own: bool,
}

impl Default for ToolPage {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolPage {
    pub fn new() -> Self {
        Self {
            kind: Kind::Inspect,
            a: String::new(),
            b: String::new(),
            part: String::new(),
            grep: String::new(),
            limit: String::new(),
            // ★ 默认**勾上**：与命令行 `--special` 的默认值一致（`bool` 的
            //   `default_value_t = true`）。界面上少勾一个就少一堆证据。
            special: true,
            recursive: false,
            out: String::new(),
            result: None,
            rx: None,
            started: None,
            pick_a: false,
            pick_b: false,
            pick_out: false,
            pick_a_own: false,
            pick_b_own: false,
            pick_out_own: false,
        }
    }

    /// 诊断开关用：按名字挑工具（`inspect` / `passthrough` / `diff` / `dump` /
    /// `probe` / `verify`）。认不出就原样返回错误，由调用方记日志。
    pub fn set_kind_by_name(&mut self, name: &str) -> Result<(), String> {
        let k = match name {
            "inspect" => Kind::Inspect,
            "passthrough" => Kind::Passthrough,
            "diff" => Kind::Diff,
            "dump" => Kind::Dump,
            "probe" => Kind::Probe,
            "verify" | "verify-batch" | "verify_batch" => Kind::VerifyBatch,
            other => return Err(format!("未知的诊断工具名：{other}")),
        };
        self.kind = k;
        Ok(())
    }

    /// 有没有活正在跑（App 用它决定要不要让 `Ctrl+O` 之类的快捷键生效）。
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// 拖进来的文件落到 A。目录也一样 —— `probe` / `verify` 要的就是目录。
    pub fn drop_paths(&mut self, paths: &[PathBuf]) {
        let Some(first) = paths.first() else {
            return;
        };
        self.a = first.display().to_string();
        // 一次拖两个文件、当前工具正好要两份输入 → 顺手填上 B。
        // 不是"猜"：`diff` 的两栏本来就是"左一个右一个"。
        if let Some(second) = paths.get(1) {
            if self.kind.b_role() != Some(Role::OutFile) {
                self.b = second.display().to_string();
            }
        }
    }

    /// 画整页。`ctx` 从 `ui` 里取，调用方不用另传。
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll(ui.ctx());

        // ── 工具选择 ───────────────────────────────────────────────────
        card(ui, |ui| {
            ui.label(RichText::new("工具").size(13.0).strong());
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for k in Kind::ALL {
                        let picked = self.kind == k;
                        let btn = if picked {
                            ds::Button::primary(k.label())
                        } else {
                            ds::Button::secondary(k.label())
                        };
                        if ui.add(btn.disabled(self.busy())).clicked() {
                            self.kind = k;
                            // 换工具就清掉上一次的结果：A/B 的含义都变了，
                            // 留着上一件的报告只会让人以为那是这一件跑出来的。
                            self.result = None;
                        }
                    }
                });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(self.kind.hint())
                        .size(12.0)
                        .color(DIM_C),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(self.kind.command())
                        .size(11.5)
                        .monospace()
                        .color(DIM_C),
                );
            });

        ui.add_space(10.0);

        // ── 参数 ───────────────────────────────────────────────────────
        card(ui, |ui| {
            let (a_label, b_label) = match self.kind {
                    Kind::Inspect | Kind::Dump => ("目标 docx", ""),
                    Kind::Passthrough => ("输入 docx", "输出 docx"),
                    Kind::Diff => ("左侧 docx（A）", "右侧 docx（B）"),
                    Kind::Probe => ("目标目录", ""),
                    Kind::VerifyBatch => ("原目录（A）", "产物目录（B）"),
                };

                ui.label(RichText::new(a_label).size(12.0).color(DIM_C));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 210.0).max(320.0);
                    let icon = if self.kind.a_is_dir() {
                        ds::Icon::Folder
                    } else {
                        ds::Icon::FileText
                    };
                    ds::InputField::new(&mut self.a)
                        .placeholder(if self.kind.a_is_dir() {
                            "目录，例如 E:/test/docx"
                        } else {
                            "一个 .docx 的完整路径"
                        })
                        .leading(icon)
                        .desired_width(w)
                        .show(ui);
                    if ui.add(pick_button(self.kind.a_is_dir())).clicked() {
                        self.pick_a = true;
                    }
                    // 「⋯」= 自绘面板（主按钮走系统对话框）
                    if ds::dots(ui, "用内置快速选择器打开（不弹系统窗口）") {
                        self.pick_a_own = true;
                    }
                });

                if let Some(role) = self.kind.b_role() {
                    ui.add_space(ds::SPACING.s3);
                    ui.label(RichText::new(b_label).size(12.0).color(DIM_C));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 210.0).max(320.0);
                        let icon = if role == Role::InDir {
                            ds::Icon::Folder
                        } else {
                            ds::Icon::FileText
                        };
                        ds::InputField::new(&mut self.b)
                            .placeholder(match role {
                                Role::InFile => "另一个 .docx 的完整路径",
                                Role::InDir => "产物所在目录",
                                Role::OutFile => "写到哪儿（可以是不存在的路径）",
                            })
                            .leading(icon)
                            .desired_width(w)
                            .show(ui);
                        if ui.add(pick_button(role == Role::InDir)).clicked() {
                            self.pick_b = true;
                        }
                        if ds::dots(ui, "用内置快速选择器打开（不弹系统窗口）") {
                            self.pick_b_own = true;
                        }
                    });
                }

                if self.kind == Kind::Dump {
                    ui.add_space(ds::SPACING.s3);
                    ui.horizontal_wrapped(|ui| {
                        ds::InputField::new(&mut self.part)
                            .label("只看某个 part")
                            .placeholder("留空 = 全部文本容器，如 word/header1.xml")
                            .desired_width(320.0)
                            .show(ui);
                        ui.add_space(ds::SPACING.s4);
                        ds::InputField::new(&mut self.grep)
                            .label("只看含此子串的段落")
                            .placeholder("留空 = 不过滤")
                            .desired_width(240.0)
                            .show(ui);
                        ui.add_space(ds::SPACING.s4);
                        ds::InputField::new(&mut self.limit)
                            .label("每个 part 最多几段")
                            .placeholder("留空 = 20；0 = 不限")
                            .desired_width(140.0)
                            .show(ui);
                    });
                    ui.add_space(ds::SPACING.s3);
                    ui.add(ds::Checkbox::with_label(
                        &mut self.special,
                        "显示空元素 / 修订删除 / 域代码",
                    ))
                    .on_hover_text(
                        "勾上（默认）：把 `<w:delText>`、域代码、空元素节点都列出来。\n\
                         取消：只看承载可见文本的 `<w:t>`。",
                    );
                }

                if self.kind == Kind::Probe {
                    ui.add_space(ds::SPACING.s3);
                    ui.add(ds::Checkbox::with_label(&mut self.recursive, "递归子目录"));
                    ui.add_space(ds::SPACING.s3);
                    ui.label(RichText::new("导出建议规则").size(12.0).color(DIM_C));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 210.0).max(320.0);
                        ds::InputField::new(&mut self.out)
                            .placeholder("留空 = 只显示在下面；填路径则写成规则文件")
                            .leading(ds::Icon::Save)
                            .desired_width(w)
                            .show(ui);
                        if ui
                            .add(
                                ds::Button::secondary("选文件")
                                    .leading(ds::Icon::FileText)
                                    .disabled(self.busy()),
                            )
                            .clicked()
                        {
                            self.pick_out = true;
                        }
                        if ds::dots(ui, "用内置快速选择器打开（不弹系统窗口）") {
                            self.pick_out_own = true;
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "写出的规则**替换列是空的、且全部标成 off**：空替换的语义是删除，\
                             填好替换内容、去掉 off 才能执行。",
                        )
                        .size(11.5)
                        .color(WARN_C),
                    );
                }

                ui.add_space(ds::SPACING.s4);
                ui.horizontal(|ui| {
                    let can = !self.busy() && !self.a.trim().is_empty();
                    let label = if self.busy() { "运行中…" } else { "运行" };
                    if ui
                        .add(ds::Button::primary(label).leading(ds::Icon::Activity).disabled(!can))
                        .clicked()
                    {
                        self.start();
                    }
                    if self.busy() {
                        ui.add_space(ds::SPACING.s3);
                        let secs = self
                            .started
                            .map(|t| t.elapsed().as_secs_f32())
                            .unwrap_or(0.0);
                        ui.label(
                            RichText::new(format!("{secs:.1} s"))
                                .size(12.0)
                                .color(DIM_C),
                        );
                        ui.add_space(ds::SPACING.s3);
                        // 工具都是"读一遍就出报告"，没有可中断的语义；
                        // 能做的只是别让界面看着像死了（下面有进度反馈）。
                        ui.label(
                            RichText::new("大目录 / 大量文件时要等一会儿，界面不会卡")
                                .size(11.5)
                                .color(DIM_C),
                        );
                    }
                });
            });

        ui.add_space(10.0);

        // ── 结果 ───────────────────────────────────────────────────────
        let Some(res) = &self.result else {
            ui.label(
                RichText::new("结果会出现在这里（长报告可以直接框选复制，或点「复制全部」）")
                    .size(12.0)
                    .color(DIM_C),
            );
            return;
        };
        match res {
            Err(e) => {
                ui.label(RichText::new(format!("× {e}")).strong().color(ERR_C));
            }
            Ok(o) => {
                ui.horizontal(|ui| {
                    let (tag, color) = if o.ok {
                        ("结论：通过", OK_C)
                    } else {
                        ("结论：不通过", ERR_C)
                    };
                    ui.label(RichText::new(tag).strong().color(color));
                    ui.add_space(ds::SPACING.s4);
                    if ui
                        .add(ds::Button::ghost("复制全部").leading(ds::Icon::Copy))
                        .clicked()
                    {
                        ui.ctx().copy_text(o.text.clone());
                    }
                });
                ui.add_space(6.0);
                card(ui, |ui| {
                        // 高度**给定值**，不取 `available_height`：外层是页面级
                        // `ScrollArea`，那里的可用高度可能是无穷大 —— 拿它当上限
                        // 等于没上限，整页会被报告撑开。380 大约是一屏能读完
                        // 「文件 / part 总数 / 前十个 part」的高度。
                        egui::ScrollArea::vertical()
                            .max_height(380.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let pal = ds::palette_of(ui.ctx());
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&o.text)
                                            .size(12.0)
                                            .monospace()
                                            .color(pal.text_primary),
                                    )
                                    .selectable(true),
                                );
                            });
                    });
            }
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        let Some(rx) = &self.rx else { return };
        match rx.try_recv() {
            Ok(done) => {
                self.result = Some(done);
                self.rx = None;
                self.started = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.result = Some(Err("后台线程意外结束（没有回传结果）".to_string()));
                self.rx = None;
                self.started = None;
            }
        }
    }

    /// 从外部触发起跑（`Ctrl+Enter`）。与点「运行」按钮同一条路 ——
    /// 差别只在于**先补上"参数不全"的判断**：按钮本身灰着，键盘没有那个约束。
    pub fn start_now(&mut self) {
        if self.busy() {
            return;
        }
        self.start();
    }

    fn start(&mut self) {
        let kind = self.kind;
        let a = self.a.trim().to_string();
        let b = self.b.trim().to_string();
        let part = self.part.trim().to_string();
        let grep = self.grep.trim().to_string();
        let limit = self.limit.trim().to_string();
        let special = self.special;
        let recursive = self.recursive;
        let out = self.out.trim().to_string();

        // 参数不全就别开线程：错误信息与"跑完发现不对"长得一样，但**快得多**。
        if let Some(e) = precheck(kind, &a, &b) {
            self.result = Some(Err(e));
            return;
        }

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run(kind, &a, &b, &part, &grep, &limit, special, recursive, &out));
        });
        self.rx = Some(rx);
        self.started = Some(Instant::now());
        self.result = None;
    }
}

/// 页内的一张卡。与「页眉替换 / 打印与导出」两页同一个视觉单位
/// （`ds::Card`，宽度铺满），只是这里不需要标题栏。
fn card<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ds::Card::new().show(ui, |ui| {
        ui.set_width(ui.available_width());
        f(ui)
    })
}

/// 按钮上的字随槽位变：文件还是目录，一眼能看出点下去会发生什么。
fn pick_button(dir: bool) -> ds::Button<'static> {
    if dir {
        ds::Button::secondary("选目录").leading(ds::Icon::Folder)
    } else {
        ds::Button::secondary("选文件").leading(ds::Icon::FileText)
    }
}

fn precheck(kind: Kind, a: &str, b: &str) -> Option<String> {
    if a.is_empty() {
        return Some("先填要看的路径".to_string());
    }
    if kind.b_role().is_some() && b.is_empty() {
        return Some(format!("「{}」这一栏也得填", kind.label()));
    }
    if kind == Kind::Passthrough {
        // 同一个文件读进来又写回去 = 自己覆盖自己，`package::passthrough` 会拒绝，
        // 但那是**跑起来之后**才拒绝；这里先拦，省一次无谓的等待。
        if let (Ok(x), Ok(y)) = (
            Path::new(a).canonicalize(),
            Path::new(b).canonicalize(),
        ) {
            if x == y {
                return Some("输入和输出是同一个文件 —— 零改动透传不会覆盖原件".to_string());
            }
        }
    }
    None
}

/// 后台线程里跑一件工具。返回的文本原样来自 `wrepl::tools`。
#[allow(clippy::too_many_arguments)]
fn run(
    kind: Kind,
    a: &str,
    b: &str,
    part: &str,
    grep: &str,
    limit: &str,
    special: bool,
    recursive: bool,
    out: &str,
) -> Done {
    let pa = PathBuf::from(a);
    let pb = PathBuf::from(b);
    // 空串 = 不过滤 / 默认值。用 `Option` 而不是空串往下传，是为了让库里
    // 那几条 `None` 分支照旧走到（界面不该有自己的"默认值"）。
    let part = (!part.is_empty()).then(|| part.to_string());
    let grep = (!grep.is_empty()).then(|| grep.to_string());
    let limit: usize = if limit.is_empty() {
        20
    } else {
        match limit.parse() {
            Ok(n) => n,
            Err(_) => return Err(format!("「{limit}」不是数字（每 part 最多几段）")),
        }
    };
    let out = (!out.is_empty()).then(|| PathBuf::from(out));

    let r = match kind {
        Kind::Inspect => tools::inspect(&pa),
        Kind::Passthrough => tools::passthrough(&pa, &pb),
        Kind::Diff => tools::diff(&pa, &pb),
        Kind::Dump => tools::dump(&pa, part.as_deref(), grep.as_deref(), limit, special),
        Kind::Probe => tools::probe_dir_report(&pa, recursive, out.as_deref()),
        Kind::VerifyBatch => tools::verify(&pa, &pb, false, true),
    };
    r.map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 六个工具都得有"界面能表达"的参数槽：要 B 的必须说清 B 是什么，
    /// 不要 B 的不能有。少一条就会出现"界面让填、工具不读"的瞎栏。
    #[test]
    fn 参数槽与工具对得上() {
        for k in Kind::ALL {
            match k {
                Kind::Inspect | Kind::Dump | Kind::Probe => {
                    assert!(k.b_role().is_none(), "{} 不该有 B 栏", k.label())
                }
                Kind::Passthrough => assert_eq!(k.b_role(), Some(Role::OutFile)),
                Kind::Diff => assert_eq!(k.b_role(), Some(Role::InFile)),
                Kind::VerifyBatch => assert_eq!(k.b_role(), Some(Role::InDir)),
            }
        }
    }

    /// A 栏是目录还是文件，必须与工具的语义一致 —— 填错一栏，用户点「选文件」
    /// 拿到的却是目录，会以为程序坏了。
    #[test]
    fn a_栏类型与工具一致() {
        assert!(Kind::Probe.a_is_dir());
        assert!(Kind::VerifyBatch.a_is_dir());
        for k in [Kind::Inspect, Kind::Passthrough, Kind::Diff, Kind::Dump] {
            assert!(!k.a_is_dir(), "{} 的 A 是文件", k.label());
        }
    }

    /// 跑起来之前就要把"必填没填"挡住：让用户等两秒才知道少填一栏，很糟。
    #[test]
    fn 缺参数在起跑前就报错() {
        assert!(precheck(Kind::Inspect, "", "x").is_some());
        assert!(precheck(Kind::Inspect, "a.docx", "").is_none());
        assert!(precheck(Kind::Diff, "a.docx", "").is_some(), "diff 要两份");
        assert!(precheck(Kind::VerifyBatch, "dirA", "").is_some());
    }

    /// 换工具要清掉上一次的结果：A/B 的含义都变了，留着旧报告会误导。
    #[test]
    fn 工具页起手是空的() {
        let t = ToolPage::new();
        assert_eq!(t.kind, Kind::Inspect);
        assert!(t.result.is_none());
        assert!(!t.busy());
        assert!(t.special, "默认与命令行 --special 的默认值一致（显示特殊节点）");
    }
}
