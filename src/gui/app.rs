//! wrepl 图形界面。
//!
//! ## 边界
//!
//! 与命令行**共用** [`wrepl::pipeline`]、[`wrepl::rules`]、[`wrepl::naming`]、
//! [`wrepl::verify`] 的全部实现。本模块只做三件事：
//!
//! 1. 把界面上的东西收成 [`Options`]
//! 2. 调内核
//! 3. 把结果摊开给人看
//!
//! 任何"顺手在这里算一下"的念头都要压住——那正是两个前端行为漂移的起点。
//!
//! ## 关于"同步替换文件名"的默认值
//!
//! **默认勾上**（2026-10-08 起）。同一套规则也作用到文件名上是这套工具的常规用法，
//! 勾了之后报告里会多出「文件名对照」表。
//! 唯一的例外是 `--preset` / `--selftest` 预置模式（见 [`Preset::rename`]）：
//! 它直接拿测试语料开跑，改名会动到语料文件名，所以那条路仍默认不勾。
//!
//! ## 关于执行方式
//!
//! **默认就地替换源文件**（在原文件上直接覆盖改写）。备份默认**不留**——源目录里
//! 不会多出任何文件；要安全垫就在「选项」里勾「保留 .bak 备份」。
//! 勾上「输出到子文件夹」才改成写副本（默认 `<输入目录>/out`），源文件不动。
//! 勾选框默认 **不勾**——目的就是把"改动落在哪儿"明明白白摆在一个复选框上，
//! 而不是让人去猜输出目录留空会怎样。
//!
//! ## 界面上只留"要做什么"
//!
//! 规则三列（查找内容 / 替换为 / 命中），底下一条执行按钮。其余能省则省：
//!
//! - 括号里的补充说明、作用域速查、验证关卡解释——全删。看过一次就不再看，
//!   却天天占着地方。
//! - **「启用」列删掉**：规则既然写进表里，就是要替换的，再勾一次没意义。
//!   但规则文件里标着「禁用」「使用通配符」的行**照样会被认出来并挡在表外**
//!   （见 [`LoadStat`]）——界面没这一列，不等于可以悄悄跑掉。
//! - 「预演」「执行后自动验证」也不再是选项：验证是**执行本身的一部分**
//!   （两个关卡都在内存里比字节，不额外花时间）；预演只在无头自检里用。
//! - 报告与运行日志跟着产物走，界面上不再有它们各自的路径输入框。
//!
//! ## 关于执行线程
//!
//! 执行**不在界面线程里跑**。整批交给后台线程调 [`wrepl::pipeline::run`]
//! （内核再按文件并行），线程每处理完一个文件就往通道里塞一条进度，
//! 界面每帧取一次、顺带刷新——所以文件再多，窗口也不会"卡住不动"，
//! 日志是一条条往外滚的。
//!
//! 之前是同步跑：`do_run` 调完 `run` 才返回，整段执行期间窗口完全不能重绘，
//! 几百个文件的时候看上去就像死了。改成"按帧推进"并不改变总耗时，
//! 改变的是**这段时间里屏幕上有东西在动**。

use crate::ds;
use eframe::egui::{self, Color32, RichText};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use wrepl::pipeline::{self, Options, RunResult};
use wrepl::rules::{self, Rule, Scope};
// `naming` 原本只被「文件名改名预览」用 —— 那块 2026-10-11 下线后就没人引了，
// 于是从导入里摘掉。真要把预览放回来，记得把它加回：
//     use wrepl::{naming, report};
use wrepl::report;

use crate::picker::{Mode, Outcome, Picker};
use crate::wordtool::{HeaderMode, Sink};

const OK_C: Color32 = Color32::from_rgb(0x1B, 0x7F, 0x3B);
const WARN_C: Color32 = Color32::from_rgb(0xB5, 0x6A, 0x00);
const ERR_C: Color32 = Color32::from_rgb(0xC0, 0x2B, 0x1D);
const DIM_C: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);

/// 界面上的「选路径」入口。
///
/// 自绘面板是**同一个**（[`Picker`]），靠这个枚举记住"这一趟是替谁选的" ——
/// 面板只负责把路径拿回来，填到哪个字段由这里决定。
///
/// 也用于「... → 系统对话框」兜底：走 `rfd` 时按同一套分派。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PickFor {
    /// 「输入」行的「选目录…」
    InputDir,
    /// 「输入」行的「选文件…」（可多选 .docx）
    InputFiles,
    /// 「输出」行的「选目录…」
    OutputDir,
    /// 规则区「从文本导入」
    ImportTxt,
    /// 规则区「从 Excel 导入」
    ImportXlsx,
    /// 规则区「导出到 Excel」
    ExportXlsx,
    /// 「页眉替换」页的「选择文档（可多选）」
    HeaderDocs,
    /// 「打印与导出」页的「选择文档（可多选）」
    PrintDocs,
    /// 「页眉替换」页的「选择图片…」（**只选一张**）
    HeaderImage,
    /// 「打开源目录 / 打开输出目录」—— 用自绘面板浏览那个目录。
    ///
    /// 这两个入口原先会 `explorer` 拉开资源管理器：那是一个新的 OS 窗口，
    /// 本机要为此交 0.8~2 秒的「窗口税」（见 `picker` 模块头）。改成自绘面板后
    /// 一个窗口都不新建；面板里双击文件照样能把它打开（见 [`App::open_file`]）。
    BrowseDir,
    /// 「打开报告」—— 同上，只是起点是报告文件所在的目录。
    BrowseReport,
}

/// 界面里的一行规则。
///
/// ## 为什么界面只剩三列，结构里却留着作用域和各种开关
///
/// 「查找内容 / 替换为 / 命中」是给人用的；其余字段是**给 Excel 和文本规则
/// 文件走的**——从表里读进来什么样，导出时照着写回去，不因为界面上没这一列
/// 就把信息丢掉。也正因为界面上改不了，它们不该自己变。
#[derive(Clone, Debug)]
struct Row {
    find: String,
    replace: String,
    /// 作用域串（`全部` / `正文,页眉页脚`…）。界面不显示，随规则文件往返。
    scope: String,
    case_sensitive: bool,
    whole_word: bool,
    kana_sensitive: bool,
    note: String,
    /// 上一次执行里这条规则的命中数
    hits: Option<usize>,
}

impl Row {
    fn new(find: impl Into<String>, replace: impl Into<String>, scope: &str) -> Self {
        Row {
            find: find.into(),
            replace: replace.into(),
            scope: scope.to_string(),
            case_sensitive: false,
            whole_word: false,
            kana_sensitive: false,
            note: String::new(),
            hits: None,
        }
    }
}

/// 把一条内核规则摊成界面行（界面不显示的字段照样带过来，供导出时写回去）。
fn row_of(r: &Rule) -> Row {
    let mut row = Row::new(r.find.clone(), r.replace.clone(), &r.scope.display());
    row.case_sensitive = r.case_sensitive;
    row.whole_word = r.whole_word;
    row.kana_sensitive = r.kana_sensitive;
    row.note = r.note.clone();
    row
}

/// 规则文件里带「禁用」或「通配符」标记的行，界面装不进去。
///
/// 装上界面也没有列能显示它，等于埋一颗雷：人以为在跑，其实没跑；
/// 或者以为按普通替换跑，其实要按通配符跑。所以一律不装，并把条数说出来。
struct LoadStat {
    added: usize,
    disabled: usize,
    wildcard: usize,
}

/// 后台执行线程回传的消息。
enum Msg {
    /// 一个文件处理完：已完成序数 / 总数 / 要写进日志的若干行
    Progress(usize, usize, Vec<String>),
    /// **收尾阶段**的进度：阶段名 / 已完成 / 总数。
    ///
    /// 阶段 3（逐文件改写）之外还有两段要走——「同步文件名」与「验证产物」，
    /// 它们各有自己的计数（从 0 开始），在此之前界面拿不到任何一条消息，
    /// 于是进度条满格之后就"卡"在那儿。这条消息就是补这个洞。
    Stage(String, usize, usize),
    /// 整批跑完
    Done(Box<RunResult>),
    /// 整批失败（连结果都没拿到）
    Failed(String),
}

/// 正在跑的一批。
///
/// 界面每帧从 `rx` 取消息，取到就刷新——**不再等整批跑完**。
/// 执行期间要用的参数（规则、选项、输入路径）先寄存在这里，
/// 线程报完工后再由界面线程做收尾（回填命中数、写报告、写运行日志）。
struct Job {
    rx: Receiver<Msg>,
    handle: Option<std::thread::JoinHandle<()>>,
    total: usize,
    done: usize,
    /// 当前所处的收尾阶段（阶段名, 已完成, 总数）。
    ///
    /// 为 `None` 表示还停在阶段 3（逐文件改写）；一旦收到，界面就切到那个阶段的计数。
    /// 记在 `Job` 上而不是每帧现取，是因为阶段消息可能一帧内来好几条——只留最后一条，
    /// 进度条才不会来回跳。
    stage: Option<(String, usize, usize)>,
    started: std::time::Instant,
    rules: Vec<Rule>,
    opts: Options,
    paths: Vec<PathBuf>,
    dry: bool,
    /// 报告与运行日志的落点。**开跑时就冻住**——执行期间人还能改输入框，
    /// 收尾时再算就可能落到另一个目录去。
    report_path: PathBuf,
    log_path: PathBuf,
}

/// 开窗时预先填好的参数。
///
/// 用途有二：① `--selftest` 共用同一套"装载"逻辑；
/// ② `--preset <输入> <输出> [规则文件]` 直接把界面填好再开窗，
///    省掉每次手点选目录——截图、演示、反复试规则时都用得上。
#[derive(Debug, Clone, Default)]
pub struct Preset {
    pub input: String,
    pub output: String,
    pub rules_file: Option<String>,
    /// 是否预先勾上「同步替换文件名」。
    ///
    /// 默认 **false**，与界面默认值（**勾上**）不同：预置模式拿现成语料直接开跑，
    /// 改名会连语料文件名一起改掉。要演示 / 截图改名效果时显式加 `--rename`。
    pub rename: bool,
    /// 是否按「就地替换源文件」开窗。
    ///
    /// 默认 **false**：只要给了输出目录，预置模式就走"写副本"，
    /// 这样 `--preset` / `--selftest` 永远不可能因为默认值变化而动到源样本。
    /// 要演示/截图就地模式时显式加 `--in-place`。
    pub in_place: bool,
}

impl Preset {
    /// 从 `--preset <输入> <输出> [规则文件] [--rename] [--in-place]` 解析。
    pub fn from_args(args: &[String]) -> Option<Preset> {
        let mut it = args.iter().skip_while(|a| *a != "--preset").skip(1);
        let input = it.next()?.clone();
        let output = it.next()?.clone();
        let rules_file = it.next().filter(|a| !a.starts_with("--")).cloned();
        Some(Preset {
            input,
            output,
            rules_file,
            rename: args.iter().any(|a| a == "--rename"),
            in_place: args.iter().any(|a| a == "--in-place"),
        })
    }
}

pub struct App {
    // 输入 / 输出
    /// 输入：一个目录、一个 .docx，或**多个 .docx**。
    ///
    /// 存原始文本，按需拆（见 [`App::input_paths`]）。一行一个路径，也认
    /// `;` / `；` 分隔——多选文件时是一行一个，手输时可以一行写一个，
    /// 也可以一行写完用分号隔开。目录与文件可以混着给。
    ///
    /// 与命令行同构：`wrepl <PATH>...` 本来就收多个位置参数，界面这里
    /// 只是把"多个"从一个参数列表变成一栏文本。
    input: String,
    /// 勾上「输出到子文件夹」时才用：产物目录。默认 `<输入目录>/out`。
    output: String,
    /// 落盘位置开关。**false（默认）＝ 就地替换源文件**；
    /// true ＝ 写到 `output` 目录，源文件不动。
    out_to_subdir: bool,
    /// **完整镜像**：写副本时，一处都没改动的文件也原样复制到输出目录。
    ///
    /// **默认 false**：输出目录里只有本次真正改过的文件（既定设计）。
    /// 勾上则输出目录是输入目录的完整镜像（未改动的那些逐字节相同，文件名一并归一），
    /// 可以直接当交付包拿走。只对写副本有意义——就地替换本就在原地，界面上置灰。
    mirror: bool,
    recursive: bool,
    exclude: String,

    // 规则
    rows: Vec<Row>,
    /// 「清空规则」是否已经按过第一下（等第二下确认）。
    ///
    /// 清空是**不可逆**的：手工一条条填进去的规则，清掉就没了，而且界面里
    /// 没有"撤销"。所以这个按钮走两步——第一次点只是把按钮换成一对
    /// 「确认清空 / 取消」，再点一次才真清。代价是一次点击，只在真要清的时候付。
    clear_armed: bool,
    chain: bool,
    /// 「最长匹配优先」：规则命中区间重叠时，只让「查找内容更长」的那条生效，
    /// 被挤掉的记进报告。不勾（默认）则两条都不改、报冲突。
    ///
    /// 与「链式替换」互斥——链式下规则先后作用，不存在重叠。界面里勾了链式会把它自动清掉。
    longest_first: bool,
    /// Excel 规则表的**列布局**：固定两列（查找内容 / 替换为）之外，
    /// 哪些可选列参与读 / 写。「规则」区「Excel 列」那行复选框改的就是它。
    xlsx_cols: rules::XlsxLayout,

    // 输出与校验
    /// 「同步替换文件名」：勾上后同一套规则也作用到文件名上。**默认勾上**。
    ///
    /// 这是文件名维度的**唯一**开关——勾了才动文件名，不勾一律不动，
    /// 规则文件里「作用域」写没写「文件名」都不影响这个判断。
    /// （`--preset` / `--selftest` 预置模式由 [`Preset::rename`] 覆盖，仍是不勾。）
    rename_files: bool,
    /// 就地替换时是否保留一份 `.docx.bak`（默认**不勾**：源目录不留多余文件）。
    ///
    /// 只对「就地替换」这一种落盘方式有意义——「输出到子文件夹」本来就不动源文件。
    /// 勾上也是**只首次生成**：重复跑同一批时，`.bak` 始终是最初那一版。
    backup: bool,
    /// 只有无头自检会把执行按成"不落盘"。界面上没有预演入口。
    dry_run: bool,
    light_theme: bool,
    /// 已应用到 ctx 的主题（None = 尚未应用）
    applied_light: Option<bool>,

    // 运行状态
    result: Option<RunResult>,
    last_rule_hits: std::collections::HashMap<u32, usize>,
    log: Vec<String>,
    error: Option<String>,
    toast: Option<(String, bool)>,

    /// 正在跑的那一批（None = 空闲）
    job: Option<Job>,
    /// 「就地覆盖」确认框是否打开。
    ///
    /// 就地替换把源文件**直接覆盖、不可撤销**，而界面上触发它只是**一个单击**，
    /// 默认又不留备份——所以点「执行替换」后先弹这个确认，人点了头才真跑
    /// （见 [`App::confirm_inplace_dialog`]）。写副本不动源文件，不走这道确认。
    confirm_inplace: bool,
    /// 「就地覆盖」本次已获确认的放行票。
    ///
    /// 就地覆盖的确认**统一兜在 `start_run` 入口**（而不是散在各按钮里），
    /// 这样不管从哪个入口触发都拦得住。对话框点「确认替换」时把这张票置位，
    /// `start_run` 见到它就放行一次并立刻作废——于是下一次执行仍会再问一遍。
    inplace_confirmed: bool,
    /// 本次执行的日志在 `log` 里的起点——落盘的「运行日志.txt」只取这一段，
    /// 不把开窗提示和上一次执行也捎进去。
    run_log_from: usize,

    // ── 自绘文件选择器 ──
    /// 自绘「选文件 / 选目录 / 另存为」面板。**常驻**——跨次打开记得上次停在哪儿，
    /// 盘符表也不用每开一次重建。
    ///
    /// 不用系统对话框的原因见 `picker` 模块头：本机的企业管控套件会给
    /// **每个新建的 OS 窗口**记一笔 0.8~2 秒的账，而系统文件对话框内部要建十几个。
    /// 系统对话框没删干净——面板右下角「...」就是退回它的入口（[`PickFor`] 那套分派）。
    picker: Picker,
    /// 面板正在替哪个入口选（`None` = 面板没开）。
    picker_for: Option<PickFor>,

    // ── 顶部三页切换（0.3.0 起）──
    /// 当前页：批量替换（原有）/ 页眉替换 / 打印与导出。
    page: Page,
    /// 页眉替换与打印与导出的状态 + 任务（页眉替换纯 Rust，打印 / 导出走 Word COM）。
    wt: crate::wordtool::WordTool,
}

/// 顶部的三个页。
///
/// 「批量替换」是工具原本的能力，与另两页**没有共同依赖**——它纯 Rust 改 OOXML，
/// 不需要 Word；「页眉替换」2026-10-19 起也是纯 Rust（COM 引擎已退役），
/// 只有「打印与导出」需要目标机器装 Word（没有 Word 时导出可降级到 WPS）。
///
/// ★ 导航上的标签 2026-10-11 由「正文替换」改成「批量替换」（用户要求）。
///   `Page::Replace` 这个**内部名字没动** —— 它对应的是"在正文里做替换"这份能力，
///   改它要连带动 CLI、报告与一堆注释，收益为零。标签只是给人看的。
#[derive(PartialEq, Clone, Copy)]
enum Page {
    /// 正文批量替换（原有）。界面标签叫「批量替换」。
    Replace,
    /// 页眉首单元格替换（文本 / 图片）
    Header,
    /// 打印与导出
    Print,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        // 导出规则表时按界面勾的列写；「启用」不在界面上，也就不写这一列。
        // （读表时另走 `for_read()`，那一列照认不误。）
        let mut xlsx_cols = rules::XlsxLayout::all();
        xlsx_cols.set("启用", false);

        let mut app = App {
            input: String::new(),
            output: String::new(),
            // ★ 默认不勾：不勾就是就地替换源文件（有 .bak 备份兜底）。
            out_to_subdir: false,
            mirror: false,
            recursive: false,
            exclude: String::new(),
            rows: Vec::new(),
            clear_armed: false,
            chain: false,
            // ★ 默认不勾：保持了工具一贯的"不猜"——重叠时两条都不改，把冲突报出来。
            longest_first: false,
            xlsx_cols,
            // ★ 默认**勾上**：同一套规则也作用到文件名上，是这套工具的常规用法
            // （2026-10-08 起由"默认不勾"改过来，按 xin 的要求）。
            // 预置模式想不勾，走 [`Preset::rename`] 覆盖。
            rename_files: true,
            // ★ 默认不勾：就地替换不留 .bak，源目录里一个多余文件都没有。
            // 换来的代价是"写盘失败时没有回退文件"，所以要留的人自己勾。
            backup: false,
            dry_run: false,
            light_theme: true,
            applied_light: None,
            result: None,
            last_rule_hits: std::collections::HashMap::new(),
            log: Vec::new(),
            error: None,
            toast: None,
            job: None,
            confirm_inplace: false,
            inplace_confirmed: false,
            run_log_from: 0,
            picker: Picker::new(),
            picker_for: None,
            page: Page::Replace,
            wt: crate::wordtool::WordTool::new(),
        };
        app.push_log("就绪：选文件或目录（文件可多选）→ 填规则（或「从 Excel 导入」）→ 点「执行替换」");
        // 诊断开关：`WREPL_DEBUG_PICKER=dir|file|out|save|hdrdocs|printdocs|img|browse`
        // 时启动就把选择器打开，再配合 `WREPL_DEBUG_CLICK` 就能无人值守地复现"点一下"。
        // 本机的合成鼠标输入进不来窗口（见 `diag` 里的说明），只能让程序自己做。
        // 不设这个环境变量时，下面整段是空转。
        match crate::diag::debug_picker_mode().as_deref() {
            Some("dir") => app.open_picker(PickFor::InputDir),
            Some("file") => app.open_picker(PickFor::InputFiles),
            Some("out") => app.open_picker(PickFor::OutputDir),
            Some("save") => app.open_picker(PickFor::ExportXlsx),
            // 另两页的「选择文档 / 选择图片」——同一个面板、同一个入口分派，
            // 只是结果落到 `WordTool` 那边（见 `apply_picked`）。
            Some("hdrdocs") => app.open_picker(PickFor::HeaderDocs),
            Some("printdocs") => app.open_picker(PickFor::PrintDocs),
            Some("img") => app.open_picker(PickFor::HeaderImage),
            // 浏览模式（底部「打开源目录 / 打开报告」）——它有自己的一套底部按钮
            // （「打开」+「关闭」，没有「...」），值得单独照一帧。
            Some("browse") => app.open_picker(PickFor::BrowseDir),
            Some(other) => {
                crate::diag::log(format!("[诊断] 未知的 WREPL_DEBUG_PICKER={other:?}，忽略"))
            }
            None => {}
        }
        // 同款诊断开关：`WREPL_DEBUG_PAGE=replace|header|header-image|print` 启动即落在那页。
        // 加了「页眉替换 / 打印与导出」两页之后，截图与演示都需要让窗口一开就停在
        // 指定页——合成鼠标事件进不来（同上），只能让程序自己切。不设则停在首页。
        match crate::diag::debug_page_mode().as_deref() {
            Some("replace") => app.page = Page::Replace,
            Some("header") => app.page = Page::Header,
            Some("header-image") => {
                app.page = Page::Header;
                app.wt.hdr_mode = HeaderMode::Image;
            }
            Some("print") => app.page = Page::Print,
            Some(other) => {
                crate::diag::log(format!("[诊断] 未知的 WREPL_DEBUG_PAGE={other:?}，忽略"))
            }
            None => {}
        }
        app
    }

    /// 用预置参数构造：开窗即可用，省掉每次手点选目录。
    ///
    /// **给了输出目录就自动切到"写副本"模式**——预置路径（`--preset` / `--selftest`）
    /// 一律是"输出到目录"，绝不会因为默认值的变化而去动源样本。
    pub fn with_preset(p: &Preset) -> Self {
        let mut app = App::new();
        app.input = p.input.clone();
        app.output = p.output.clone();
        // 给了输出目录就走"写副本"，除非显式要求 --in-place
        app.out_to_subdir = !p.in_place && !p.output.trim().is_empty();
        app.rename_files = p.rename;
        if app.out_to_subdir {
            app.push_log(format!("预置输出目录：{}", app.output));
        } else {
            app.push_log("预置：就地替换源文件".to_string());
        }
        if let Some(f) = &p.rules_file {
            match app.load_rules_file(f) {
                Ok(n) => app.push_log(format!("已从规则文件读入 {n} 条规则：{f}")),
                Err(e) => app.error = Some(e),
            }
        }
        app
    }

    /// 从文本规则文件装载规则。
    ///
    /// 复用库里**同一个**解析器（`rules::from_text_file`），不另写一份——
    /// 界面里能读的规则格式，命令行也必须能读，反之亦然。
    ///
    /// 返回**实际装进表里的条数**（不是文件里的行数）：标着「禁用」或
    /// 「使用通配符」的行界面上没地方显示，不会装进来，跳了几条看日志。
    pub fn load_rules_file(&mut self, path: &str) -> Result<usize, String> {
        let defaults = Rule::new(0, "", "");
        let rs = rules::from_text_file(Path::new(path), &defaults, 1)
            .map_err(|e| format!("规则文件读取失败：{e:#}"))?;
        let mut st = LoadStat {
            added: 0,
            disabled: 0,
            wildcard: 0,
        };
        for r in &rs {
            if !r.enabled {
                st.disabled += 1;
                continue;
            }
            if r.use_wildcard {
                st.wildcard += 1;
                continue;
            }
            self.rows.push(row_of(r));
            st.added += 1;
        }
        self.report_skipped(&st, "文本规则文件");
        Ok(st.added)
    }

    /// 把"跳过了几条、为什么"写进日志。不静默丢弃是这个工具的一贯做法。
    fn report_skipped(&mut self, st: &LoadStat, what: &str) {
        if st.disabled > 0 {
            self.push_log(format!(
                "跳过 {} 条标着「禁用」的规则（界面只放会执行的规则）——来自{what}",
                st.disabled
            ));
        }
        if st.wildcard > 0 {
            self.push_log(format!(
                "跳过 {} 条要求「使用通配符」的规则（界面不支持通配符，需要时用命令行版）——来自{what}",
                st.wildcard
            ));
        }
    }

    /// 从 Excel 规则表装载规则。
    ///
    /// **表名不固定**：从第一张工作表起依次往后找，取第一张读得出条款的表
    /// （叫 `Sheet1`、`替换清单`、`2026-003Aa` 都行，不必叫「规则」）。
    ///
    /// 与命令行的 `--rules-book` **走同一套解析**（`rules::from_xlsx_book` →
    /// `rules::from_xlsx_rows`）。列怎么认：第 1 列＝查找内容、第 2 列＝替换为；
    /// 带表头时其后的列**按标题名**认，读哪几列由 「规则」区的「Excel 列」决定。
    /// 早期版本那张带「序号」的表也能直接读——那一列会被忽略。
    ///
    /// 读表用 `for_read()`：界面上虽然没有「启用」这一列，表里带的
    /// 「启用 = 否」照样认——认出来就**不装进表**，界面上没地方显示它，
    /// 装进去只会让人以为这条规则会跑。
    ///
    /// 返回 `(装入条数, 实际采用的工作表名)`。
    pub fn load_rules_xlsx(&mut self, path: &str) -> Result<(usize, String), String> {
        let p = Path::new(path);
        let defaults = Rule::new(0, "", "");
        let (rs, warns, sheet) =
            rules::from_xlsx_book(p, &self.xlsx_cols.for_read(), &defaults, 1)
                .map_err(|e| format!("读 Excel 规则表失败：{e:#}"))?;

        for w in &warns {
            self.push_log(format!("注意：{w}"));
        }
        let mut st = LoadStat {
            added: 0,
            disabled: 0,
            wildcard: 0,
        };
        // 先整表收进局部变量：解析/构造中途出错时，界面上原有的规则表不受影响。
        let mut new_rows: Vec<Row> = Vec::with_capacity(rs.len());
        for r in &rs {
            if !r.enabled {
                st.disabled += 1;
                continue;
            }
            if r.use_wildcard {
                // 界面表达不了通配符语义 → 不装进来（降级成普通替换才是真的危险）
                st.wildcard += 1;
                continue;
            }
            new_rows.push(row_of(r));
            st.added += 1;
        }
        let old = self.rows.len();
        self.rows = new_rows;
        if old > 0 {
            self.push_log(format!("原有 {old} 条规则已清空，整表来自 Excel"));
        }
        self.report_skipped(&st, "Excel 规则表");
        Ok((st.added, sheet))
    }

    /// 把界面上的规则表导出成 Excel。
    ///
    /// 列布局由 「规则」区的「Excel 列」决定：前两列恒为 查找内容 / 替换为，
    /// 其后是勾选的可选列。**没有序号列** —— 行序本身就是序号。
    /// 渲染走内核的 `rules::xlsx_row`，与 `rules template` 生成的模板同一条路。
    pub fn export_rules_xlsx(&self, path: &str) -> Result<usize, String> {
        let layout = &self.xlsx_cols;
        let headers = layout.headers();

        let mut out: Vec<Vec<report::Cell>> = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            if r.find.trim().is_empty() {
                continue; // 空行不导出
            }
            let mut rule = Rule::new(0, r.find.clone(), r.replace.clone());
            // 界面上只放会执行的规则，所以导出就是"启用"（表里也不再写这一列）
            rule.enabled = true;
            rule.case_sensitive = r.case_sensitive;
            rule.whole_word = r.whole_word;
            // 界面没有通配符开关：写死 false，不谎报
            rule.use_wildcard = false;
            rule.kana_sensitive = r.kana_sensitive;
            rule.scope = self.scope_of(r, i)?;
            rule.note = r.note.clone();

            out.push(
                rules::xlsx_row(layout, &rule)
                    .iter()
                    .map(|c| report::Cell::text(c))
                    .collect(),
            );
        }
        if out.is_empty() {
            return Err("规则表是空的，没有可导出的行".into());
        }
        let n = out.len();
        report::write_single_sheet(Path::new(path), "规则", &headers, out)
            .map_err(|e| format!("写 Excel 失败：{e:#}"))?;
        Ok(n)
    }

    fn push_log(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
        let cap = self.log_cap();
        self.trim_log(cap);
    }

    // ─────────────────────── 参数收集 ───────────────────────

    /// 把输入框里的文本拆成路径清单。
    ///
    /// 分隔符：换行、`;`、`；`（不认半角逗号——Windows 路径里没有它，
    /// 但文件名里可能有，拆错比不拆更糟）。空项与重复项丢掉：同一个文件
    /// 选两遍没有意义，内核虽然会去重，报告里却会留两条一样的「输入」。
    fn input_paths(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        // 去重走集合：以前是 `out.contains(&p)`（O(n²)），一次粘几十上百个路径时
        // 会肉眼可见地慢。结果仍按粘贴顺序排列（报告里"输入"那一栏保持原样）。
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for piece in self.input.split(['\n', '\r', ';', '；']) {
            let s = piece.trim();
            if s.is_empty() {
                continue;
            }
            let p = PathBuf::from(s);
            if seen.insert(p.clone()) {
                out.push(p);
            }
        }
        out
    }

    /// 输入的"家"：这批文件落在哪个目录。
    ///
    /// 取**第一个**输入的落点目录（是目录就是它本身，是文件就是它的父目录）。
    /// 用途有二：① 勾了「输出到子文件夹」却没填目录时，默认 `<它>/out`；
    /// ② 就地替换时，报告与运行日志落在这里——跟产物挨着，回头好找。
    ///
    /// 多个输入散在不同目录时这里不瞎猜：就用第一个。真实产物落点由内核
    /// 按每个文件自己的来源目录决定（就地替换是各自写回各自那儿），
    /// 这一处只影响"报告放哪"，取第一个是人能预期的那种。
    fn input_dir(&self) -> PathBuf {
        self.input_paths()
            .first()
            .map(|p| {
                if p.is_dir() {
                    p.to_path_buf()
                } else {
                    p.parent().map(|x| x.to_path_buf()).unwrap_or_default()
                }
            })
            .unwrap_or_default()
    }

    /// 产物落点：写副本＝输出目录；就地替换＝输入目录（那是这批文件的"家"）。
    ///
    /// 报告与运行日志都落在这里——跟产物挨着，回头找的时候不用回忆。
    fn product_dir(&self) -> PathBuf {
        if self.out_to_subdir && !self.output.trim().is_empty() {
            PathBuf::from(self.output.trim())
        } else {
            self.input_dir()
        }
    }

    /// 报告落点。界面没有这一栏，位置由产物落点决定。
    fn report_path(&self) -> PathBuf {
        let d = self.product_dir();
        if d.as_os_str().is_empty() {
            PathBuf::from("替换报告.xlsx")
        } else {
            d.join("替换报告.xlsx")
        }
    }

    /// 界面行 → 内核作用域。
    ///
    /// 作用域串本身来自规则文件（界面上没有这一列，新建的行是 `全部`）；
    /// **「文件名」那一维由 「规则」区的「同步替换文件名」开关说了算**——
    /// 勾了才作用到文件名。这样规则文件里写没写「文件名」都不需要人去猜。
    fn scope_of(&self, r: &Row, i: usize) -> Result<Scope, String> {
        let mut sc = Scope::parse(r.scope.trim()).map_err(|e| {
            format!(
                "第 {} 条规则的「作用域」`{}` 无法解析（这一列在界面上没有，来自规则文件）：{e:#}",
                i + 1,
                r.scope
            )
        })?;
        sc.filename = self.rename_files;
        if sc.is_empty() {
            return Err(format!(
                "第 {} 条规则没有可用作用域：它只作用到「文件名」，但「同步替换文件名」没勾上",
                i + 1
            ));
        }
        Ok(sc)
    }

    fn build_rules(&self) -> Result<Vec<Rule>, String> {
        let mut out: Vec<Rule> = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            if r.find.is_empty() {
                continue;
            }
            let mut rule = Rule::new((i + 1) as u32, r.find.clone(), r.replace.clone());
            rule.enabled = true;
            rule.case_sensitive = r.case_sensitive;
            rule.whole_word = r.whole_word;
            rule.kana_sensitive = r.kana_sensitive;
            rule.note = r.note.clone();
            rule.scope = self.scope_of(r, i)?;
            out.push(rule);
        }
        if out.is_empty() {
            return Err("没有可用的规则：至少填一条「查找内容」".into());
        }
        Ok(out)
    }

    fn options_for(&self, dry: bool) -> Options {
        // 落盘位置只由「输出到子文件夹」这一个开关决定：
        //   不勾 → 就地替换源文件（在原文件上直接覆盖改写）
        //   勾上 → 写副本到 output 目录
        // 预演一律不落盘，所以两个都留空（pipeline 只在非 dry 时要求二选一）。
        let (out_dir, in_place) = if dry {
            (None, false)
        } else if self.out_to_subdir {
            (Some(PathBuf::from(self.output.trim())), false)
        } else {
            (None, true)
        };

        Options {
            recursive: self.recursive,
            exclude: self
                .exclude
                .split([';', '；', ',', '，'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            chain: self.chain,
            // 与命令行一致：链式下不存在重叠，裁决方式无从谈起
            longest_first: self.longest_first && !self.chain,
            out_dir,
            in_place,
            // 备份只对就地替换有意义；预演不落盘，也就无所谓备份
            backup: self.backup && !dry && in_place,
            dry_run: dry,
            // 预演一律不落盘，改名与验证都无从谈起
            rename_files: self.rename_files && !dry,
            // 执行后自动验证是**执行本身的一部分**，不再是界面选项：
            // 两个关卡都在内存里比字节，不额外花时间，没有关掉的理由。
            verify_after: !dry,
            // 界面固定用自动并行度：本机有几个核就用几个。
            // 不在界面上做成选项——"几个线程"是机器的事，不是规则的事，
            // 摆出来只会让人多一个不知道该怎么选的开关。
            threads: 0,
            // 完整镜像只对写副本有意义（就地替换本就在原地）；预演不落盘，同样不生效。
            // 注意这里判 `in_place` 而不是 `out_dir`——`out_dir` 在同一条结构体字面量里
            // 已经先被移进字段，再读它就是 use-after-move。
            mirror: self.mirror && !dry && !in_place,
            // 界面**每次执行完都会写报告**（`write_report`），报告的「文件清单」
            // 要填处理前后的整文件 SHA256 —— 所以这里必须恒开，否则报告那两列会是空的。
            file_sha: true,
        }
    }

    // ─────────────────────── 动作 ───────────────────────

    fn precheck(&mut self) -> bool {
        let paths = self.input_paths();
        if paths.is_empty() {
            self.error = Some("请先指定输入（目录，或一个 / 多个 .docx 文件）".into());
            return false;
        }
        // 逐个报。多选十几个文件时，"输入路径不存在"等于没说——得指出是哪个。
        let missing: Vec<String> = paths
            .iter()
            .filter(|p| !p.exists())
            .map(|p| p.display().to_string())
            .collect();
        if !missing.is_empty() {
            self.error = Some(format!(
                "{} 个输入路径不存在：{}",
                missing.len(),
                missing.join("　；　")
            ));
            return false;
        }
        true
    }

    // ─────────────────────── 文件名改名预览（已下线）───────────────────────
    //
    // 这里原本有一套「改名预览」：勾了「同步替换文件名」后，把**会被改名的**文件
    // 逐条列成 `旧名 → 新名`。2026-10-11 反馈要求**整块不再体现**，理由是它和
    // 下面的「执行结果」重复 —— 那栏本来就有「改名 N」的计数，而已经看得见的
    // 规则表 + 输入路径足够判断会改成什么；预览多占一屏，还会随目录变大而刷屏。
    //
    // ★ 只是**界面上不再显示**，能力一个都没丢：
    //   · `--rename` / `--preset … --rename` 照旧；
    //   · 界面上「同步替换文件名」勾选框照旧（`self.rename_files`，它才决定改不改名）；
    //   · 执行完，「执行结果」里的「改名」计数与产物目录的报告照旧。
    //
    // 真要放回来：把 `rename_preview_key` / `compute_rename_preview` 从 git 历史里
    // 取回，在 `section_rules` 末尾照原样渲染即可（当时是 8 条 + 一行汇总）。

    // ─────────────────────── 执行 ───────────────────────

    /// 起一批。
    ///
    /// 界面线程只做三件事：校验参数、装好规则、开线程——然后**立刻返回**。
    /// 之后每帧由 [`App::pump_job`] 收进度。窗口全程能重绘、能拖动、能最小化。
    fn start_run(&mut self, dry: bool, ctx: &egui::Context) {
        self.toast = None;
        self.error = None;
        if self.job.is_some() {
            return; // 上一批还在跑（按钮此时是灰的，这里只是兜底）
        }
        // ★ 就地覆盖不可逆：确认**统一兜在这个入口**，不散在各按钮里——
        //   以后不管从哪儿触发（按钮、快捷键、将来的菜单）都拦得住。
        //   预演不落盘、写副本不动源文件，两者都不需要确认。
        //   走一趟确认框，人点「确认替换」→ 置 `inplace_confirmed` → 再进来放行一次。
        if !dry && !self.out_to_subdir && !self.inplace_confirmed {
            self.confirm_inplace = true;
            return;
        }
        self.inplace_confirmed = false; // 放行票一次有效，用完立刻作废
        // 新一轮开始：把上一轮的「执行结果」面板先撤掉。留着它的话，跑的过程中
        // 人会以为那是这一轮的结论（数字都还在，只是过时了）。
        self.result = None;
        if !self.precheck() {
            return;
        }
        if self.out_to_subdir && self.output.trim().is_empty() {
            // 勾了写副本却没填目录：按约定用 <输入目录>/out，不必打断人去填
            self.output = self.input_dir().join("out").display().to_string();
        }
        let rule_list = match self.build_rules() {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };

        // 落盘的那份日志只写本次执行这一段，不把开窗提示和上一次执行也捎进去
        self.run_log_from = self.log.len();
        self.push_log(format!(
            "──── {} ──── 规则 {} 条",
            if dry { "预演" } else { "执行" },
            rule_list.len()
        ));
        self.push_log(if dry {
            "落盘位置：预演，不写任何文件".to_string()
        } else if self.out_to_subdir {
            // 勾了镜像就说一句。它改变的是"输出目录里有什么"，
            // 而这正是执行完之后最容易被误解的一件事（少了一堆没改动的文件）。
            if self.mirror {
                format!(
                    "落盘位置：副本 → {}（完整镜像：未改动的文件也原样复制过去）",
                    self.output.trim()
                )
            } else {
                format!("落盘位置：副本 → {}", self.output.trim())
            }
        } else if self.backup {
            "落盘位置：就地替换源文件（保留 .bak 备份）".to_string()
        } else {
            "落盘位置：就地替换源文件（不留备份）".to_string()
        });

        let opts = self.options_for(dry);
        let paths = self.input_paths();
        // 多输入时说一声：报告里"输入"那一栏会是好几条，别让人以为出了错
        if paths.len() > 1 {
            self.push_log(format!(
                "输入 {} 项（{}）",
                paths.len(),
                paths
                    .iter()
                    .map(|p| short_name(p))
                    .collect::<Vec<_>>()
                    .join("、")
            ));
        }
        // 报告/日志落点提前算好并冻住：执行期间人还可以改输入框，
        // 收尾时再算就可能落到另一个目录去。
        let report_path = self.report_path();
        let log_path = self.product_dir().join("运行日志.txt");

        let (tx, rx) = mpsc::channel::<Msg>();
        // 工作线程有多个，`Sender` 不是 `Sync` —— 套一把锁。一个文件才过一次，忽略不计。
        let tx = Mutex::new(tx);
        let ctx2 = ctx.clone();
        let (paths2, rules2, opts2) = (paths.clone(), rule_list.clone(), opts.clone());

        let handle = std::thread::spawn(move || {
            let out = pipeline::run(&paths2, &rules2, &opts2, &|i, t, o| {
                let mark = match o.status {
                    "OK" | "DRY_RUN" => "·",
                    "CONFLICT" => "!",
                    "ERROR" => "✗",
                    _ => "·",
                };
                let mut lines = Vec::with_capacity(2);
                lines.push(format!(
                    "{mark} [{i}/{t}] {}　{}　命中 {}（替换 {} / 冲突 {}）",
                    short_name(&o.src),
                    o.status,
                    o.hits.len(),
                    o.applied,
                    o.conflicts
                ));
                for h in &o.hits {
                    if !h.applied {
                        lines.push(format!(
                            "      ✗ 规则#{}　「{}」　{}",
                            h.rule_id, h.matched, h.reason
                        ));
                    }
                }
                let g = tx.lock().unwrap_or_else(|e| e.into_inner());
                let _ = g.send(Msg::Progress(i, t, lines));
                drop(g);
                // 叫界面醒一下：有新行要画
                ctx2.request_repaint();
            }, &|stage, i, t| {
                // 阶段 4/5 的进度（同步文件名 / 验证产物）。
                // 这两段在补这条消息之前是**完全静默**的，所以进度条满格之后
                // 还会干等一段——文件越多等得越久（本机 rename 单价随目录里
                // 的文件数涨，180 个文件时能到 190 ms/次）。
                let g = tx.lock().unwrap_or_else(|e| e.into_inner());
                let _ = g.send(Msg::Stage(stage.to_string(), i, t));
                drop(g);
                ctx2.request_repaint();
            });

            let msg = match out {
                Ok(res) => Msg::Done(Box::new(res)),
                Err(e) => Msg::Failed(format!("{e:#}")),
            };
            let g = tx.lock().unwrap_or_else(|e| e.into_inner());
            let _ = g.send(msg);
            drop(g);
            ctx2.request_repaint();
        });

        self.job = Some(Job {
            rx,
            handle: Some(handle),
            total: 0,
            done: 0,
            stage: None,
            started: std::time::Instant::now(),
            rules: rule_list,
            opts,
            paths,
            dry,
            report_path,
            log_path,
        });
    }

    /// 收一次后台进度。每帧调一次；没消息就立刻返回，不阻塞界面。
    fn pump_job(&mut self) {
        let Some(mut job) = self.job.take() else {
            return;
        };
        let mut finished: Option<Box<RunResult>> = None;
        let mut failed: Option<String> = None;

        loop {
            match job.rx.try_recv() {
                Ok(Msg::Progress(_i, t, lines)) => {
                    job.total = t;
                    job.done += 1;
                    self.extend_log(lines);
                }
                Ok(Msg::Stage(name, i, t)) => {
                    job.stage = Some((name, i, t));
                }
                Ok(Msg::Done(res)) => {
                    finished = Some(res);
                    break;
                }
                Ok(Msg::Failed(e)) => {
                    failed = Some(e);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                // `Done` / `Failed` 是线程发的最后一条，队列里没有它俩就是它 panic 了，
                // 不能让界面一直转圈等下去。
                Err(mpsc::TryRecvError::Disconnected) => {
                    failed = Some("执行线程意外退出（没有返回结果）".into());
                    break;
                }
            }
        }

        if let Some(res) = finished {
            if let Some(h) = job.handle.take() {
                let _ = h.join();
            }
            self.finish_run(job, *res);
        } else if let Some(e) = failed {
            if let Some(h) = job.handle.take() {
                let _ = h.join();
            }
            self.error = Some(e);
        } else {
            self.job = Some(job); // 还没跑完，放回去
        }
    }

    /// 阻塞地把刚起的一批跑到底。只给无头自检用（界面上没有这个入口）。
    fn run_blocking(&mut self, dry: bool) {
        // 自检没有真窗口，给个哑 ctx —— `request_repaint` 落在它身上是无害的
        let ctx = egui::Context::default();
        self.start_run(dry, &ctx);
        while self.job.is_some() {
            self.pump_job();
            if self.job.is_some() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    /// 线程报完工后的收尾：回填命中数、写报告与运行日志、给出结论。
    ///
    /// 这些必须在界面线程做（要改 App 状态），所以整批算完后只把 `RunResult`
    /// 送回来，重活留在原线程里。
    fn finish_run(&mut self, job: Job, res: RunResult) {
        // 逐规则命中数回填到规则表
        self.last_rule_hits.clear();
        for o in &res.outcomes {
            for h in &o.hits {
                *self.last_rule_hits.entry(h.rule_id).or_insert(0) += 1;
            }
        }
        for (i, r) in self.rows.iter_mut().enumerate() {
            r.hits = self.last_rule_hits.get(&((i + 1) as u32)).copied();
        }

        // 线程数与时耗都报出来：文件一多，"到底有没有在并行"不该靠猜。
        self.push_log(format!(
            "并行：{} 线程｜本次用时 {} ms",
            res.threads,
            job.started.elapsed().as_millis()
        ));
        self.push_log(format!(
            "合计：文件 {}（含跳过 {}）｜命中 {}｜已替换 {}｜冲突 {}",
            res.outcomes.len(),
            res.skipped.len(),
            res.total_hits(),
            res.total_applied(),
            res.total_conflicts()
        ));

        if !job.dry {
            let renamed = res.name_rows.iter().filter(|n| n.changed).count();
            if self.rename_files && renamed > 0 {
                self.push_log(format!("文件名同步改名：{renamed} 个"));
            }
            // 目标名已被占用时是加序号而不是覆盖（保护已有文件），要说一声
            let occupied = res
                .name_rows
                .iter()
                .filter(|n| n.note.contains("已被占用"))
                .count();
            if occupied > 0 {
                self.push_log(format!(
                    "注意：{occupied} 个目标文件名在输出目录里已存在，已自动加序号（不覆盖已有文件）"
                ));
            }
            let bad = res.verify_bad();
            if !res.verify_rows.is_empty() {
                self.push_log(format!(
                    "自动验证：{} 个文件　通过 {} / 不通过 {}",
                    res.verify_rows.len(),
                    res.verify_rows.len() - bad,
                    bad
                ));
            }
            for v in res.verify_rows.iter().filter(|v| !v.ok()) {
                self.push_log(format!("   × {}：{}", v.file, v.note));
            }
            // 报告
            let rp_s = job.report_path.display().to_string();
            match write_report(&rp_s, &res, &job.rules, &job.paths, &job.opts) {
                Ok(_) => self.push_log(format!("报告已写出：{rp_s}")),
                Err(e) => self.error = Some(format!("报告写出失败：{e}")),
            }

            // 运行日志落盘：跟报告放在一起（写副本＝输出目录，就地替换＝源目录）
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // 开头的 BOM：Windows 记事本靠它认出 UTF-8，否则中文是乱码
            let mut text = format!(
                "\u{feff}wrepl  Word 批量替换　运行日志\r\n生成时间：{}\r\n{}\r\n\r\n",
                pipeline::format_epoch(now),
                "-".repeat(60)
            );
            // 起点可能因为日志超上限被挤走 → 退回 0，宁可多带几行也不要越界
            let tail = self
                .log
                .get(self.run_log_from.min(self.log.len())..)
                .unwrap_or(&[]);
            text.push_str(&tail.join("\r\n"));
            text.push_str("\r\n");
            match pipeline::write_run_log(&job.log_path, &text) {
                Ok(p) => self.push_log(format!("运行日志已写出：{}", p.display())),
                Err(e) => self.error = Some(format!("运行日志写出失败：{e:#}")),
            }
        }

        let msg = verdict_text(&res, job.dry);
        let ok = res.failed() == 0 && res.verify_bad() == 0;
        self.toast = Some((msg, ok));
        if res.outcomes.is_empty() {
            self.push_log("（没有找到可处理的 .docx）");
        }
        self.dry_run = job.dry;
        self.result = Some(res);

        // 执行期间日志上限是放宽的（本次执行那一段要整段落盘），收尾后收回常态
        let cap = 400;
        self.trim_log(cap);
    }

    /// 日志上限。执行期间放宽——这一整段要写进「运行日志.txt」，
    /// 被 400 行的**显示**上限挤掉就说不过去了。
    fn log_cap(&self) -> usize {
        if self.job.is_some() { 20_000 } else { 400 }
    }

    /// 超上限就从最老的开始丢；同时把「本次执行起点」一起往前挪，避免切片越界。
    fn trim_log(&mut self, cap: usize) {
        if self.log.len() > cap {
            let d = self.log.len() - cap;
            self.log.drain(0..d);
            self.run_log_from = self.run_log_from.saturating_sub(d);
        }
    }

    /// 追加若干行（后台进度用）。
    fn extend_log(&mut self, lines: Vec<String>) {
        self.log.extend(lines);
        let cap = self.log_cap();
        self.trim_log(cap);
    }

    /// 用系统默认程序打开一个**文件**（.xlsx 报告 / .docx / 图片…）。
    ///
    /// 与自绘面板的分工：**面板负责"看"，这里负责"把看到的那一个打开"**。
    /// 原先这里叫 `open_path`、对文件取父目录再拉资源管理器 —— 现在
    /// "看"这件事交给 [`PickFor::BrowseDir`] 那套自绘面板了（不新建 OS 窗口），
    /// 留下来的只有"打开"这一个动作。
    ///
    /// 走 `explorer`：它拿到**文件**路径时等价于 `ShellExecute`（拿目录路径才是
    /// "打开资源管理器"）。这是全程序唯一还会新建 OS 窗口的地方，而且只在
    /// 用户主动双击 / 点「打开」时发生 —— 不是点一下就弹。
    fn open_file(&mut self, p: &str) {
        let p = p.trim();
        if p.is_empty() || !Path::new(p).is_file() {
            return;
        }
        if let Err(e) = std::process::Command::new("explorer").arg(p).spawn() {
            self.error = Some(format!("打不开：{e}"));
        }
    }

    // ─────────────────────── 界面 ───────────────────────

    /// 画一帧。
    ///
    /// 入参是 eframe 给的 **root `Ui`**（整窗、无内边距、无底色）。
    ///
    /// ★ egui 0.34 起面板只有 `Panel::*::show_inside(ui, …)` 这一条路，旧的
    ///   `show(ctx, …)` 被标成 deprecated —— `wrepl-gui` 因此长期挂着 7 条告警
    ///   （3 条 `TopBottomPanel`/`SidePanel` 别名 + 4 条 `show`）。搬到这里之后
    ///   那 7 条归零，`CONTRIBUTING.md` 里「自家代码零告警」才名副其实。
    ///
    /// `Context` 从 `ui.ctx()` 取一份**克隆**（`Context` 内是 `Arc`，克隆很便宜）：
    /// 主题、重绘请求与两个浮层（确认框 / 选择器）仍按 `&Context` 写，不必为了
    /// 搬布局把它们全改一遍。
    pub fn render(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        // ① 先把后台线程攒下的进度收进来（不阻塞），这一帧就能画出来
        self.pump_job();
        // 新两页的任务状态在 `wt` 里，也每帧收一次
        self.wt.poll();
        // ② 有活干的时候定时重绘：串行阶段（预分配输出路径、改名）没有进度消息，
        //    不主动叫醒的话进度条会停住不动。200ms 一次，人眼看是连续的。
        if self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // 主题只在变化时应用一次（每帧都刷会把用户的临时样式冲掉）。
        //
        // 第 2 步起走设计系统：`ds::apply` 内部是
        // `set_theme + apply_theme_with(palette, Comfortable)`，浅/深两套 palette
        // 各自对应迁移前 `Visuals::light()/dark()` 的角色。
        if self.applied_light != Some(self.light_theme) {
            ds::apply(&ctx, self.light_theme);
            self.applied_light = Some(self.light_theme);
        }

        // ── TopBar：品牌 + 全局开关（GUIDE §1：「TopBar · brand · user menu」）──
        egui::Panel::top("title").show_inside(ui, |ui| {
            let pal = ds::palette_of(ui.ctx());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // 品牌图标用设计系统的品牌绿（`Icon::Leaf` 是 GUIDE 里给
                // "tout est calme / 品牌" 场景的推荐图标）。
                let (ir, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                ds::Icon::Leaf.paint(ui.painter(), ir, pal.brand_default);
                ui.label(
                    RichText::new("wrepl")
                        .size(20.0)
                        .strong()
                        .color(pal.text_primary),
                );
                ui.label(
                    RichText::new("Word 工具集")
                        .size(13.0)
                        .color(pal.text_secondary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(ds::Checkbox::with_label(&mut self.light_theme, "浅色"));
                });
            });
            ui.add_space(6.0);
        });

        // ── Sidebar：三页导航 ────────────────────────────────────────────────
        //
        // GUIDE §2：3–12 个常驻区块一律用 SidePanel + NavItem（≤5 且是"同一实体的
        // 子视图"才用 Tabs）。三页各有自己的动作与状态、随时可切，属于前者。
        // （GUIDE 写的是 0.34 之前的名字；同一件东西现在叫 `Panel::left`。）
        egui::Panel::left("nav")
            .exact_size(184.0)
            .resizable(false)
            .show_inside(ui, |ui| {
                // ★ 让内容**铺满整个面板宽度**（2026-10-11 那条黑条的根治）。
                //
                // `egui::Frame` 铺底色时用的是 `内容 min_rect + 内边距`，不是面板矩形 ——
                // 侧栏内容（三个 NavItem）比可用宽度窄，于是底色只铺到 `exact_size`
                // 就停了；而面板本身还多预留了一圈内边距，`CentralPanel` 又从更右边
                // 开始画，中间就空出一条（本机实测 11px）谁都没刷到的缝，露出窗口
                // 清屏色 = 黑条。
                //
                // 把内容的最小宽度顶到可用宽度，Frame 就必然铺满，缝从根上没有了。
                // （`ds::window_bg` 那条只是兜底。）
                ui.set_min_width(ui.available_width());
                ui.add_space(10.0);
                for (page, label, icon) in [
                    (Page::Replace, "批量替换", ds::Icon::FileText),
                    (Page::Header, "页眉替换", ds::Icon::Edit),
                    (Page::Print, "打印与导出", ds::Icon::Package),
                ] {
                    if ui
                        .add(
                            ds::NavItem::new(label)
                                .icon(icon)
                                .selected(self.page == page),
                        )
                        .clicked()
                    {
                        self.page = page;
                    }
                }
            });

        egui::Panel::bottom("actions").show_inside(ui, |ui| {
            // 这一栏是「批量替换」专用的（执行按钮 / 打开目录 / 报告 / 清日志）。
            // 另两页各自把动作放在页面卡片里，所以这里直接不画。
            if self.page != Page::Replace {
                return;
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let running = self.job.is_some();
                let can_run = !self.input.trim().is_empty() && !running;
                let label = if running { "执行中…" } else { "执行替换" };
                // 就地覆盖的确认兜在 `start_run` 入口（见那里的注释），这里直接调。
                //
                // GUIDE §4：每屏**只有一个** primary，破坏性动作用 danger。
                // 就地替换会不可撤销地覆盖源文件 —— 正是 danger 的用例；
                // 写副本不动源文件，保持 primary 即可。
                let run_btn = if self.out_to_subdir {
                    ds::Button::primary(label)
                } else {
                    ds::Button::danger(label)
                }
                .size(ds::ButtonSize::Lg)
                .leading(if running {
                    ds::Icon::Hourglass
                } else {
                    ds::Icon::Lightning
                })
                .disabled(!can_run);
                if ui
                    .add(run_btn)
                    .on_hover_text(if self.out_to_subdir {
                        "把产物写到输出目录，源文件不动\n快捷键：Ctrl+Enter（或 F5 先扫描一遍）"
                    } else {
                        "就地覆盖源文件（不可撤销）——点击后会先弹一次确认\n快捷键：Ctrl+Enter（或 F5 先扫描一遍）"
                    })
                    .clicked()
                {
                    self.start_run(false, &ctx);
                }
                ui.add_space(ds::SPACING.s5);
                let open_lbl = if self.out_to_subdir {
                    "打开输出目录"
                } else {
                    "打开源目录"
                };
                if ui
                    .add(ds::Button::secondary(open_lbl).leading(ds::Icon::FolderOpen))
                    .clicked()
                {
                    // 走自绘面板浏览，不拉资源管理器 —— 后者是新的 OS 窗口，
                    // 本机每建一个要交 0.8~2 秒「窗口税」（见 `picker` 模块头）。
                    self.open_picker(PickFor::BrowseDir);
                }
                ui.add_space(ds::SPACING.s2);
                if ui
                    .add(ds::Button::secondary("打开报告").leading(ds::Icon::FileText))
                    .clicked()
                {
                    self.open_picker(PickFor::BrowseReport);
                }
                ui.add_space(ds::SPACING.s5);
                // 执行期间不给清：那一整段日志等一下要落盘成「运行日志.txt」
                if ui
                    .add(
                        ds::Button::ghost("清空日志")
                            .leading(ds::Icon::Trash)
                            .disabled(running),
                    )
                    .clicked()
                {
                    self.log.clear();
                    self.run_log_from = 0;
                }
            });

            // 进度条：只在跑的时候出现。文件多的时候，这一条就是"它没死"的凭据。
            //
            // 一整批其实有五段：收集 → 预分配路径 → 逐文件改写 → 同步文件名 → 验证。
            // 进度条原先只覆盖第三段，所以它满格之后还会干等一段 —— 用户报障的正是这个。
            // 现在阶段 4/5 各自报 `Msg::Stage`，文案跟着切：
            //   改写中… 12 / 180 → 同步文件名… 40 / 180 → 验证产物… 90 / 180
            if let Some(job) = &self.job {
                ui.add_space(4.0);
                let (frac, text) = match &job.stage {
                    Some((name, i, t)) if *t > 0 => (
                        (*i as f32 / *t as f32).clamp(0.0, 1.0),
                        format!("{name}… {i} / {t}"),
                    ),
                    _ if job.total == 0 => (0.0, "正在收集待处理文件…".to_string()),
                    // 阶段 3 报完了、但阶段 4/5 的第一条消息还没到（或这两段本来就不跑）
                    _ if job.done >= job.total => (1.0, "正在收尾…".to_string()),
                    _ => (
                        (job.done as f32 / job.total as f32).clamp(0.0, 1.0),
                        format!("改写中… {} / {} 个文件", job.done, job.total),
                    ),
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(RichText::new(text).small())
                        .desired_width(320.0),
                );
            }
            if let Some((msg, ok)) = &self.toast {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(msg)
                        .strong()
                        .color(if *ok { OK_C } else { WARN_C }),
                );
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("× {e}")).strong().color(ERR_C));
            }
            ui.add_space(6.0);
        });

        // 运行日志**排在页面里**（中央滚动区的最后一块），与 0.2.2 一致。
        //
        // 0.2.3 中间把它改成过「贴底的独立面板」，想消掉"日志下面那片空白"。
        // 结果是撞上 egui 0.29 的一个坑：`TopBottomPanel`（现在叫 `Panel::bottom`）
        // 存进 state 的是**内容矩形**
        // 的高度（不是面板矩形），下一帧又把它当面板高度读回来；而日志里那个
        // `auto_shrink([false, false])` 的 ScrollArea 会把"可用高度"整块吃掉 ——
        // 于是内容矩形每帧比面板高一截，形成正反馈。实测**强制重绘 5 帧，面板就从
        // 168 涨到填满整个窗口**，「运行日志」的标题直接顶到标题栏下面。
        // 教训：底部面板里不要放"吃满可用高度"的东西；固定高度也压不住（会变成
        // 一条死横条），不如回到原位置 —— 日志跟着页面滚，最简单也最稳。
        egui::CentralPanel::default().show_inside(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                match self.page {
                    Page::Replace => {
                        self.section_io(ui);
                        ui.add_space(10.0);
                        self.section_rules(ui);
                        ui.add_space(10.0);
                        self.section_result(ui);
                        ui.add_space(10.0);
                        self.section_log(ui);
                    }
                    Page::Header => self.wt.ui_header(ui),
                    Page::Print => self.wt.ui_print(ui),
                }
            });
        });

        // 就地覆盖确认框最后画：它要浮在整页之上（含底部那排动作按钮）。
        self.confirm_inplace_dialog(&ctx);

        // 「页眉替换 / 打印与导出」两页的「选择文档」按钮**不自己弹对话框**：
        // 选择器是常驻的、全程序只有一台，所以由这里统一收口 —— 界面上那些
        // 按钮只把意图抛出来（`WordTool::take_pick`），开还是这儿开。
        // 收在这里还有个好处：此刻已出了 `CentralPanel` 的借用期，够得着 `self.picker`。
        if let Some(who) = self.wt.take_pick() {
            self.open_picker(match who {
                crate::wordtool::PickDoc::Header => PickFor::HeaderDocs,
                crate::wordtool::PickDoc::Print => PickFor::PrintDocs,
                crate::wordtool::PickDoc::HeaderImage => PickFor::HeaderImage,
            });
        }

        // 自绘文件选择器比确认框还要靠前（`Foreground` 面板 + 全屏遮罩），
        // 所以放在确认框之后。它自带遮罩，两者不会同时被点到。
        self.handle_picker(&ctx);

        // 拖放**必须排在 `handle_picker` 之后** —— 选择器自己带一层全屏遮罩
        // （`Order::Foreground`），而拖放提示那层画在它下面会被整片盖掉
        // （实测：坐标、可见性都对，但屏幕上什么都看不见）。
        // 选路按当前页决定，此刻 `self.page` 已是本帧最终值。
        self.handle_drop(&ctx);

        // 快捷键。排在最后：它要能改状态（起一批 / 开选择器），
        // 此刻已出全部面板的借用期。
        self.handle_shortcuts(&ctx);
    }

    /// 「就地覆盖」确认框。
    ///
    /// 只做一件事：在真的动源文件之前停一下。就地替换把源文件**直接覆盖、不可撤销**，
    /// 而触发它只是界面上的一个单击 —— 所以这里**只说清"改源文件、不可撤销"**，
    /// 问一句就走（文字从简）。
    ///
    /// 迁移第 2 步：0.29 那套"两层 `Area` 手搓 Modal"换成设计系统的
    /// [`ds::ConfirmDialog`]（GUIDE §3「Modal = bloquant」／§4「破坏性动作走
    /// `ConfirmDialog::danger()`」）。**只负责确认**：要不要留 `.bak` 由上面
    /// 「选项」那行的复选框决定，框里不再重复给一次。
    ///
    /// 就地替换**不加自动备份**：用户明确选了"覆盖源文件"，那就确认后直接覆盖，
    /// 不额外留备份文件（这是 xin 2026-10-11 明确的口径）。
    fn confirm_inplace_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_inplace {
            return;
        }
        match ds::ConfirmDialog::new(
            "在源文件上直接替换？",
            "此操作不可撤销：原文件会被直接覆盖，且不会额外留备份。\
             若不放心，可先取消、勾上「保留 .bak 备份」再执行。",
        )
        .danger()
        .confirm_label("确认替换")
        .cancel_label("取消")
        .show(ctx)
        {
            // 先关框、再置"放行票"，否则下一帧还会画这个确认框。
            Some(true) => {
                self.confirm_inplace = false;
                self.inplace_confirmed = true;
                self.start_run(false, ctx);
            }
            Some(false) => self.confirm_inplace = false,
            None => {}
        }
    }

    fn section_io(&mut self, ui: &mut egui::Ui) {
        section(ui, "文件与输出", |ui| {
            /// 路径输入框宽。
            const F: f32 = 560.0;

            let mut pick_in_dir = false;
            let mut pick_in_file = false;
            let mut pick_out_dir = false;
            let mut use_sibling_out = false;

            // GUIDE §7：标签在字段**正上方**（不是左侧——IT 类界面这样扫得更快）。
            // 这里手写这一行小标签，因为下面那个框是多行的：
            // ⚠ **没得换**：设计系统的 `InputField` 只有 `singleline`，换成它会丢掉
            //   「一行一个路径」这个用法（多选文件 / 手输若干条都靠它）。
            ui.label(
                RichText::new("输入")
                    .size(12.0)
                    .color(ds::palette_of(ui.ctx()).text_secondary),
            );
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                // 多行：一行一个路径。只给一个目录 / 一个文件时就是一行高。
                // 高度封顶 6 行——一次选了几十个文件时不再往长里撑，框内自己滚。
                let rows = self.input.lines().count().clamp(1, 6) as f32;
                ui.add_sized(
                    [F, ui_scale::ROW_H * rows],
                    egui::TextEdit::multiline(&mut self.input)
                        .hint_text("目录，或若干 .docx（一行一个）"),
                );
                ui.add_space(ds::SPACING.s2);
                ui.vertical(|ui| {
                    if ui
                        .add(ds::Button::secondary("选目录").leading(ds::Icon::Folder))
                        .on_hover_text("选输入目录（或目录里的一批 .docx）\n快捷键：Ctrl+O")
                        .clicked()
                    {
                        pick_in_dir = true;
                    }
                    if ui
                        .add(ds::Button::secondary("选文件").leading(ds::Icon::FileText))
                        .on_hover_text(
                            "按住 Ctrl / Shift 一次选多个 .docx；选中几个就处理几个。\n\
                             也可以直接把多个路径粘进左边的框里（一行一个，或用分号隔开）。",
                        )
                        .clicked()
                    {
                        pick_in_file = true;
                    }
                });
            });
            ui.add_space(ds::SPACING.s3);

            ui.horizontal(|ui| {
                ui.add(ds::Checkbox::with_label(
                    &mut self.out_to_subdir,
                    "输出到子文件夹",
                ));
                ui.add_space(ds::SPACING.s4);
                // 镜像只对写副本有意义：就地替换本就在原地，没有"要不要复制"这回事。
                // 置灰而不是隐藏——让人看到这个开关存在，切换落盘方式后它就在那儿。
                // 注：`Checkbox` 自己管 `disabled`，不能靠外层 `add_enabled_ui`。
                let writes_copy = self.out_to_subdir;
                ui.add(
                    ds::Checkbox::with_label(&mut self.mirror, "完整镜像").disabled(!writes_copy),
                )
                .on_hover_text(
                    "默认不勾：输出目录里只有本次真正改过的文件。\n\
                     勾上则未改动的文件也原样复制过去（逐字节相同，文件名一并归一），\n\
                     输出目录成为输入目录的**完整镜像**，可直接当交付包拿走。\n\
                     只对「输出到子文件夹」有效。",
                );
            });
            if self.out_to_subdir {
                ui.add_space(ds::SPACING.s3);
                let _ = ds::InputField::new(&mut self.output)
                    .label("输出目录")
                    .placeholder("留空 = <输入目录>/out")
                    .leading(ds::Icon::FolderOpen)
                    .desired_width(F)
                    .show(ui);
                ui.add_space(ds::SPACING.s2);
                ui.horizontal(|ui| {
                    if ui
                        .add(ds::Button::secondary("选目录").leading(ds::Icon::Folder))
                        .clicked()
                    {
                        pick_out_dir = true;
                    }
                    if ui
                        .add(ds::Button::ghost("源文件目录").leading(ds::Icon::Refresh))
                        .on_hover_text(
                            "把输出目录一键填成「源文件目录旁的 out 子文件夹」。\n\
                             （源文件目录＝上面「输入」那行指向的目录）",
                        )
                        .clicked()
                    {
                        use_sibling_out = true;
                    }
                });
            }
            ui.add_space(ds::SPACING.s3);

            ui.horizontal(|ui| {
                ui.add(ds::Checkbox::with_label(&mut self.recursive, "递归子目录"));
                ui.add_space(ds::SPACING.s4);
                // 备份勾选只对「就地替换」有效，写副本时置灰——不给一个不影响结果的选择
                ui.add(
                    ds::Checkbox::with_label(&mut self.backup, "保留 .bak 备份")
                        .disabled(self.out_to_subdir),
                )
                .on_hover_text(
                    "默认不留：就地替换后源目录里不会多出任何文件。\n\
                     勾上则每个文件先留一份 .docx.bak 再改写（只首次生成，\n\
                     重复跑不会覆盖最初那版）。写副本时源文件本来就不动，无需备份。",
                );
            });
            // 「排除」输入框**不再显示**（2026-10-11 反馈）。
            //
            // 它是"按文件名 glob 再多排掉一批"用的（例如 `*_bak*`），日常几乎用不到，
            // 摆在「文件与输出」里反而让人怀疑"不填会漏掉什么"。
            //
            // ★ 隐掉这个入口**不影响**任何默认行为：
            //   · Word 临时锁文件（`~$*.docx`）是 `pipeline::exclude_reason` 里的
            //     **内置**规则，跟这个框无关 —— 执行结果里"跳过"那批照旧是它；
            //   · 真要再加排除通配符，命令行版仍有 `--exclude`，`--preset` 也还认。
            // 字段本身留在 `App` 上（`options_for` 仍会读它），随时可以把这个入口放回来。

            // 选路径的动作放在布局之后执行：避免在借用 self 的闭包里嵌套借用。
            // 四个入口都走**同一个自绘面板**，靠 `PickFor` 区分结果落到哪个字段。
            if pick_in_dir {
                self.open_picker(PickFor::InputDir);
            }
            if pick_in_file {
                self.open_picker(PickFor::InputFiles);
            }
            if pick_out_dir {
                self.open_picker(PickFor::OutputDir);
            }
            if use_sibling_out {
                // 与「输出到子文件夹」留空时的默认落点同一条路（见 input_dir）
                let base = self.input_dir();
                if !base.as_os_str().is_empty() {
                    self.output = base.join("out").display().to_string();
                }
            }
        });
    }

    fn section_rules(&mut self, ui: &mut egui::Ui) {
        let mut import_txt = false;
        let mut import_xlsx = false;
        let mut export_xlsx = false;
        section(ui, "替换规则", |ui| {
            // 列宽显式给定。
            //
            // 之前用 `egui::Grid` + `TextEdit::desired_width`，实测输入框被压到 ~50px：
            // Grid 按「上一帧测得的列宽」排布，而测量本身又受当时单元格可用宽度钳制，
            // 于是 desired_width 被吞掉。改成 `add_sized` 逐格给定宽高，所见即所得。
            // 列宽跟着窗口走：拖宽窗口时表格一起变宽，右侧不留一大块空白。
            // 命中 / × 固定窄，剩下的宽度给两个输入列（约 52:48）。
            const W_HIT: f32 = 64.0;
            const W_DEL: f32 = 30.0;
            let gap = ui.spacing().item_spacing.x;
            let free = (ui.available_width() - W_HIT - W_DEL - gap * 3.0).max(400.0);
            let w_find = (free * 0.52).floor();
            let w_repl = (free - w_find).floor();

            // 工具栏分两行，一行只管一类事：
            // 上行是"动规则表"的四个动作（加一条 / 从文件读 / 导出），
            // 下行是两个开关（怎么跑），跟动作分开。
            ui.horizontal(|ui| {
                if ui
                    .add(ds::Button::primary("添加规则").leading(ds::Icon::Plus))
                    .on_hover_text("手工增加一条规则")
                    .clicked()
                {
                    self.rows.push(Row::new("", "", "全部"));
                }
                ui.add_space(ds::SPACING.s3);
                if ui
                    .add(ds::Button::secondary("从文本导入").leading(ds::Icon::Upload))
                    .on_hover_text("读 .txt 规则文件，整表替换当前规则")
                    .clicked()
                {
                    import_txt = true;
                }
                ui.add_space(ds::SPACING.s2);
                if ui
                    .add(ds::Button::secondary("从 Excel 导入").leading(ds::Icon::Upload))
                    .on_hover_text("读 .xlsx 的「规则」工作表，整表替换当前规则")
                    .clicked()
                {
                    import_xlsx = true;
                }
                ui.add_space(ds::SPACING.s2);
                if ui
                    .add(ds::Button::secondary("导出为 Excel").leading(ds::Icon::Download))
                    .on_hover_text("把下表导出成 .xlsx，便于存档或改完再读回来")
                    .clicked()
                {
                    export_xlsx = true;
                }
                // 清空整表。靠右摆、离"添加 / 导入"那三个常用动作远一点，
                // 而且走两步——清掉的手工规则界面里没有"撤销"，一次误点代价太大。
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.clear_armed {
                        // right_to_left：先放的在最右边，所以顺序是
                        // 「清空 N 条规则？ … [取消] [确认清空]」
                        if ui
                            .add(ds::Button::danger("确认清空").leading(ds::Icon::Trash))
                            .clicked()
                        {
                            let n = self.rows.len();
                            self.rows.clear();
                            self.last_rule_hits.clear();
                            self.clear_armed = false;
                            self.push_log(format!("已清空 {n} 条规则"));
                            self.toast = Some((format!("已清空 {n} 条规则"), true));
                        }
                        ui.add_space(ds::SPACING.s2);
                        if ui.add(ds::Button::secondary("取消")).clicked() {
                            self.clear_armed = false;
                        }
                        ui.add_space(ds::SPACING.s2);
                        ui.label(
                            RichText::new(format!("清空 {} 条规则？", self.rows.len()))
                                .color(WARN_C),
                        );
                    } else if ui
                        .add(
                            ds::Button::ghost("清空规则")
                                .leading(ds::Icon::Trash)
                                .disabled(self.rows.is_empty()),
                        )
                        .on_hover_text("一次清掉整张规则表（导入新表、换一批任务时用）。\n点一下只是待命，会再问一次。")
                        .clicked()
                    {
                        self.clear_armed = true;
                    }
                });
            });
            ui.add_space(12.0);

            // 选项与按钮拉开距离：它们是"怎么跑"，不是"点什么"
            ui.horizontal(|ui| {
                let chain_resp =
                    ui.add(ds::Checkbox::with_label(&mut self.chain, "链式替换"));
                if chain_resp.changed() && self.chain {
                    // 链式下规则先后作用，不存在区间重叠，最长匹配优先无事可做。
                    // 命令行遇到这个组合会直接报错，界面上就别让它出现。
                    self.longest_first = false;
                }
                ui.add_space(ds::SPACING.s5);
                ui.add(
                    ds::Checkbox::with_label(&mut self.longest_first, "最长匹配优先")
                        .disabled(self.chain),
                )
                .on_hover_text(
                    "规则命中区间重叠时，只让「查找内容更长」的那条生效，\n\
                     被挤掉的记进报告（谁让给谁、多少处）。\n\
                     不勾则两条都不改，只报冲突——原来的行为。",
                );
                ui.add_space(ds::SPACING.s5);
                ui.add(ds::Checkbox::with_label(&mut self.rename_files, "同步替换文件名"));
            });
            ui.add_space(4.0);

            // Excel 规则表的列：勾哪些就认哪些（读和写都按它）。
            // 「启用」不在这里——界面只放会执行的规则，导出也就不写那一列。
            // 收进折叠面板：只在读写 Excel 时才要看，摊开占一整行、小字又密。
            egui::CollapsingHeader::new(RichText::new("Excel 列").small().color(DIM_C))
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        // ─── 局部补丁（第 4b 步）：这一排开关的标签要 `Small`(12px) ───
                        //
                        // 依据 GUIDE §Typography：`Méta / labels` 档就是 `Small`(12px)，
                        // 且"金律"禁止同一处混两种字号。折叠头本身已是 `.small()`，
                        // 底下这排标签也得同档，否则面板内部两档字号打架。
                        //
                        // 但设计系统的 `Checkbox` 把标签字号**写死 13px**
                        // （`egui_sauge/src/components/checkbox.rs` 第 69/159 行）、
                        // 且 `label` 只收 `&str`，装不下 `RichText::small()`。
                        //
                        // 按第 1 步定的 (c) 约定**不改上游本地副本**、也**不动 `ds.rs`
                        // 的公共 API**，就地拆成两件套：
                        //   · `ds::Checkbox::new(&mut on)`  —— 只要方框，仍是 sage 绿；
                        //   · `egui::Label` + `RichText::small()` —— 只要 12px 标签。
                        // 两者之间不需要手动补间距：sage 的 `item_spacing.x` 就是
                        // `SPACING.s2`(8px)，与 DS 复选框自己的 box↔label 间距同值。
                        // 标签挂 `Sense::click()`，点文字照样能切换（与原生复选框一致）。
                        for h in rules::XLSX_UI_HEADERS {
                            let mut on = self.xlsx_cols.is_on(h);
                            let hit_box = ui.add(ds::Checkbox::new(&mut on)).clicked();
                            let hit_label = ui
                                .add(
                                    egui::Label::new(RichText::new(h).small())
                                        .sense(egui::Sense::click()),
                                )
                                .clicked();
                            // 方框被点时 `on` 已被它自己翻过；标签被点则手动翻一次。
                            if hit_box {
                                self.xlsx_cols.set(h, on);
                            } else if hit_label {
                                self.xlsx_cols.set(h, !on);
                            }
                        }
                    });
                });
            ui.add_space(10.0);

            // 工具区和规则表之间拉一条横线：上面是操作，下面是数据
            ui.separator();
            ui.add_space(10.0);

            // 表头做成一条带底色的横条，跟下面的输入行分开。
            // 左右不留内边距，好让表头文字与下面输入框左边缘对齐。
            egui::Frame::NONE
                .fill(stripe_bg(ui))
                .inner_margin(egui::Margin {
                    left: 0,
                    right: 0,
                    top: 5,
                    bottom: 5,
                })
                .corner_radius(egui::CornerRadius::same(3))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // 撑满整行：否则底色横条只包住三列文字，看着不像表头
                        ui.set_min_width(ui.available_width());
                        for (w, h) in
                            [(w_find, "查找内容"), (w_repl, "替换为"), (W_HIT, "命中")]
                        {
                            fixed_cell(ui, w, 20.0, RichText::new(h).strong());
                        }
                    });
                });
            ui.add_space(4.0);

            let mut delete: Option<usize> = None;
            // ★ 规则表**虚拟化**：只渲染滚动窗口内的那几行。
            //
            // 以前这里是 `for (i, r) in self.rows.iter_mut().enumerate()`，把整张表
            // **每一行**每帧都铺一遍。egui 不做虚拟化，规则一上百条（从 Excel 导一张
            // 大表很容易到几百条），每帧就要建几百个输入框，界面立刻发卡。
            //
            // `show_rows` 按固定行高算出"此刻可见的是第几行到第几行"，只把这段 range
            // 交给闭包渲染。行高一致（下面每行都是固定高），所以滚动条长度与位置仍准确。
            //
            // 每行再用 `push_id(i)` 圈一个独立 id 空间：show_rows 会按"每行一个 id"
            // 去跳过前面的行（`skip_ahead_auto_ids`），而我们有输入框+按钮好几个控件，
            // 靠它推不准；显式按行号定 id，滚动时输入框的光标/焦点才不会跳到别的行上。
            if !self.rows.is_empty() {
                // 一行规则的真实占位：
                //   单行输入框 `ui_scale::ROW_H`(28) + 隔行底色 Frame 的上下内边距(3+3) = 34。
                //
                // ★ 这个值必须**同时**喂给 `show_rows` 和视口高度计算，并与下面那个
                //   `inner_margin` 同源。egui 的实现（scroll_area.rs:963-966）里，
                //   `show_rows` 是假设"每行实际就渲染 row_height 这么高"的：
                //       内容总高          = (row_height + item_spacing.y) * n - item_spacing.y
                //       可见行范围 = 视口高 / (row_height + item_spacing.y)
                //   以前这里写死 32（比真实帧高少 2），行数一多，滚到底时最后一行会差一点。
                const ROW_PAD: f32 = 3.0; // 行的上下内边距，与下面 Frame 的 inner_margin 同源
                let row_h = ui_scale::ROW_H + ROW_PAD * 2.0;

                // 表格默认露出 7 行；超出的部分给一条纵向滚动条。
                // 高度算法（含"上限 + 下限"那段关键处理）与日志区、跳过清单共用
                // `fixed_rows_scroll`；行间距从当前 Style 现读，不写死 6.0。
                const VISIBLE_ROWS: usize = 7;
                let row_gap = ui.spacing().item_spacing.y;

                let n_rows = self.rows.len();
                let rows = &mut self.rows;
                fixed_rows_scroll("rules-table", VISIBLE_ROWS, row_h, row_gap)
                    .show_rows(ui, row_h, n_rows, |ui, range| {
                        for i in range {
                            ui.push_id(i, |ui| {
                                let r = &mut rows[i];
                                // 隔行淡底：规则一多，行与行会糊成一片
                                let row_bg = if i % 2 == 1 {
                                    stripe_bg(ui)
                                } else {
                                    Color32::TRANSPARENT
                                };
                                egui::Frame::NONE
                                    .fill(row_bg)
                                    .inner_margin(egui::Margin {
                                        left: 0,
                                        right: 0,
                                        // 与上面的 `row_h` 同源：这两处必须一致，
                                        // 否则 show_rows 推算的行范围与实际绘制对不上。
                                        top: ROW_PAD as i8,
                                        bottom: ROW_PAD as i8,
                                    })
                                    .corner_radius(egui::CornerRadius::same(3))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            // 隔行底色同样撑满整行
                                            ui.set_min_width(ui.available_width());
                                            ui.add_sized(
                                                [w_find, ui_scale::ROW_H],
                                                egui::TextEdit::singleline(&mut r.find)
                                                    .hint_text("查找内容"),
                                            );
                                            ui.add_sized(
                                                [w_repl, ui_scale::ROW_H],
                                                egui::TextEdit::singleline(&mut r.replace)
                                                    .hint_text("替换为"),
                                            );
                                            let resp = fixed_cell(ui, W_HIT, ui_scale::ROW_H, match r.hits {
                                                Some(n) if n > 0 => {
                                                    RichText::new(n.to_string()).color(OK_C).strong()
                                                }
                                                Some(_) => RichText::new("0").color(DIM_C),
                                                None => RichText::new("—").color(DIM_C),
                                            });
                                            // 从规则表读进来的行可能带备注，鼠标停在命中数上能看到
                                            if !r.note.is_empty() {
                                                resp.on_hover_text(&r.note);
                                            }
                                            // 用 U+00D7（乘号）而不是 U+2715：后者在装入的
                                            // 中文字体里没有字形，实测渲染成豆腐块。
                                            if ui
                                                .add_sized(
                                                    [W_DEL, ui_scale::ROW_H],
                                                    egui::Button::new("×").small(),
                                                )
                                                .clicked()
                                            {
                                                delete = Some(i);
                                            }
                                        });
                                    });
                            });
                        }
                    });
            }
            if let Some(i) = delete {
                self.rows.remove(i);
            }
            if self.rows.is_empty() {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("还没有规则：用上面的「从 Excel 导入…」载入，或点「＋ 添加规则」手填")
                            .color(DIM_C),
                    );
                });
                ui.add_space(16.0);
            }

            // 「文件名改名预览」整块**不再体现**（2026-10-11 反馈）。
            // 原因与保留的能力见上方「文件名改名预览（已下线）」那段注释。
        });

        // 选文件要借 &mut self，只能放在上面的闭包之外执行。
        // 三个入口同样走自绘面板，导入/导出的**实际处理**在下面的三个 `do_*` 里，
        // 面板与「... 系统对话框」两条路共用同一份逻辑。
        if import_txt {
            self.open_picker(PickFor::ImportTxt);
        }
        if import_xlsx {
            self.open_picker(PickFor::ImportXlsx);
        }
        if export_xlsx {
            self.open_picker(PickFor::ExportXlsx);
        }
    }

    // ─────────────────── 选路径：自绘面板 + 系统对话框兜底 ───────────────────

    /// 打开自绘面板，替 `what` 这个入口选路径。
    fn open_picker(&mut self, what: PickFor) {
        // 起点目录：能猜一个合理的就猜，猜不出就交给面板用它自己记住的上次目录
        // （`PathBuf::new()` = 空 → 面板回退到 `last_dir`）。
        let in_first = first_path(&self.input);
        let out_dir = {
            let t = self.output.trim();
            if t.is_empty() {
                PathBuf::new()
            } else {
                PathBuf::from(t)
            }
        };
        // 两个「选文档」入口的起点：已选文档里第一个的路径
        // （`Picker::open` 见到文件会自己退到它所在目录）。这样"再挑几个"
        // 会停在上次挑的那处，而不是回到默认目录。
        let hdr_first = doc_start(self.wt.docs_of(crate::wordtool::PickDoc::Header));
        let print_first = doc_start(self.wt.docs_of(crate::wordtool::PickDoc::Print));
        // 选图片同理：停在上次那张图旁边。
        let img_first = doc_start(self.wt.docs_of(crate::wordtool::PickDoc::HeaderImage));
        // 两个「浏览」入口的起点 —— 就是底部那两个按钮想让人看的地方：
        // **产物落点**（就地替换 = 源目录；写副本 = 输出目录）。
        // 直接问 `product_dir()`，不自己拼：它已经把"输出目录留空"那种情况
        // 算进去了（那时产物落在 `<输入>/out` 旁边的规则由它一处说了算）。
        let browse_dir = self.product_dir();

        let (mode, title, start, exts, default_name): (Mode, &str, PathBuf, Vec<&str>, String) =
            match what {
                PickFor::InputDir => (Mode::Folder, "选择输入目录", in_first, vec![], String::new()),
                PickFor::InputFiles => (
                    Mode::Files,
                    "选择输入文件（可多选）",
                    in_first,
                    vec!["docx"],
                    String::new(),
                ),
                PickFor::OutputDir => {
                    (Mode::Folder, "选择输出目录", out_dir, vec![], String::new())
                }
                PickFor::ImportTxt => (
                    Mode::Files,
                    "导入文本规则文件",
                    PathBuf::new(),
                    vec!["txt"],
                    String::new(),
                ),
                PickFor::ImportXlsx => (
                    Mode::Files,
                    "导入 Excel 规则表",
                    PathBuf::new(),
                    vec!["xlsx"],
                    String::new(),
                ),
                PickFor::ExportXlsx => (
                    Mode::Save,
                    "导出规则表到 Excel",
                    PathBuf::new(),
                    vec!["xlsx"],
                    "规则.xlsx".to_string(),
                ),
                // 页眉替换 / 打印与导出共用同一个面板，只是标题不同
                // （标题在面板左上角显示，用户一眼能看到这一趟是在替哪一页选）。
                PickFor::HeaderDocs => (
                    Mode::Files,
                    "选择文档 —— 页眉替换",
                    hdr_first,
                    // `.doc` 也放进来：老交付包里有 .doc，Word COM 一样打得开。
                    vec!["doc", "docx"],
                    String::new(),
                ),
                PickFor::PrintDocs => (
                    Mode::Files,
                    "选择文档 —— 打印与导出",
                    print_first,
                    vec!["doc", "docx"],
                    String::new(),
                ),
                // 图片：只选一张，筛选按各引擎实际认得的格式给
                // （`hdr::detect` 认 PNG/GIF/BMP/JPEG，这里再放 tif/tiff 也无妨：
                //  多选的格式最终仍由引擎那边判，选错了它会明确报错）。
                PickFor::HeaderImage => (
                    Mode::Files,
                    "选择图片 —— 页眉替换",
                    img_first,
                    vec!["png", "jpg", "jpeg", "bmp", "gif", "tif", "tiff"],
                    String::new(),
                ),
                // 两个「浏览」入口：`Mode::Browse` 不选东西，只让人看 ——
                // 面板里双击文件即用系统默认程序打开它（见 `apply_picked`）。
                // **不传筛选**：浏览时"这一格是什么、旁边还有什么"都该看得见。
                PickFor::BrowseDir => (
                    Mode::Browse,
                    "浏览 —— 产物落点",
                    browse_dir,
                    vec![],
                    String::new(),
                ),
                PickFor::BrowseReport => (
                    Mode::Browse,
                    "浏览 —— 报告落点",
                    self.report_path(),
                    vec![],
                    String::new(),
                ),
            };

        self.picker_for = Some(what);
        self.picker
            .open(mode, title, start, &exts, &default_name);
    }

    /// 每帧收一次面板结果。面板没开、或还没选完，什么都不做。
    fn handle_picker(&mut self, ctx: &egui::Context) {
        let Some(outcome) = self.picker.show(ctx) else {
            return;
        };
        // 结果一定是刚才那个入口引起的；拿走 `picker_for` 就是"这一趟结了"。
        let Some(what) = self.picker_for.take() else {
            crate::diag::log("选择器有结果但没有对应的入口（picker_for 为空），忽略");
            return;
        };
        crate::diag::log(format!("选择器结果：{outcome:?}  入口={what:?}"));
        match outcome {
            Outcome::Cancelled => {}
            // 「...」：自绘面板搞不定（比如要连一个没映射的网络位置），退回系统对话框。
            Outcome::Fallback => self.fallback_dialog(what),
            Outcome::Picked(paths) => self.apply_picked(what, paths),
        }
    }

    /// 面板选好了 → 填到对应字段。
    fn apply_picked(&mut self, what: PickFor, paths: Vec<PathBuf>) {
        crate::diag::log(format!("应用选择结果：入口={what:?} 路径={paths:?}"));
        match what {
            PickFor::InputDir | PickFor::OutputDir => {
                let Some(p) = paths.into_iter().next() else {
                    return;
                };
                let s = p.display().to_string();
                if what == PickFor::InputDir {
                    self.input = s;
                } else {
                    self.output = s;
                }
            }
            PickFor::InputFiles => {
                // 选中几个就是几个，一行写一个（和从前一样）
                self.input = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            PickFor::ImportTxt => {
                if let Some(p) = paths.into_iter().next() {
                    self.do_import_txt(p.display().to_string());
                }
            }
            PickFor::ImportXlsx => {
                if let Some(p) = paths.into_iter().next() {
                    self.do_import_xlsx(p.display().to_string());
                }
            }
            PickFor::ExportXlsx => {
                if let Some(p) = paths.into_iter().next() {
                    self.do_export_xlsx(p.display().to_string());
                }
            }
            // 两个「选文档」入口 —— 路径交给 `WordTool` 自己存，
            // 它那边才是「已选文档」这份状态的主人。
            PickFor::HeaderDocs => {
                self.wt.set_docs(
                    crate::wordtool::PickDoc::Header,
                    paths.iter().map(|p| p.display().to_string()).collect(),
                );
            }
            PickFor::PrintDocs => {
                self.wt.set_docs(
                    crate::wordtool::PickDoc::Print,
                    paths.iter().map(|p| p.display().to_string()).collect(),
                );
            }
            PickFor::HeaderImage => {
                // 界面上只有一个图片槽位，只收第一张（`Files` 模式下也只选了一张）。
                if let Some(p) = paths.into_iter().next() {
                    self.wt.set_image(p.display().to_string());
                }
            }
            // 浏览模式返回的是"用户想打开的东西"：面板里双击文件，或选中后点「打开」。
            // 目录不会出现在这里（`Mode::Browse` 只把文件算作结果），
            // 所以这里只管打开；用户若只是看了看就点「关闭」，一个结果都不会有。
            PickFor::BrowseDir | PickFor::BrowseReport => {
                for p in &paths {
                    self.open_file(&p.display().to_string());
                }
            }
        }
    }

    // ── 快捷键 ──────────────────────────────────────────────────────────

    /// 每帧收一次键盘输入，绑三个"起手动作"。
    ///
    /// | 键 | 动作 | 落到哪 |
    /// |---|---|---|
    /// | `Ctrl+O` | 选目录 | 批量替换页 → `InputDir`；页眉 / 打印页 → 各自的「选文档」 |
    /// | `F5` | 扫描（预演） | `start_run(true, …)` |
    /// | `Ctrl+Enter` | 执行 | `start_run(false, …)` |
    ///
    /// 三条都调**既有的入口**（`open_picker` / `start_run`），不新开路径 ——
    /// 所以「就地覆盖要确认」那道关（兜在 `start_run` 里，见那里的注释）
    /// 快捷键也躲不过去。
    ///
    /// ★ 为什么用 `ctx.input` 轮询而不是 `ui.shortcut` / `Menu`：
    ///   键位要在**任何页面、任何焦点位置**都成立（包括焦点在多行输入框里 ——
    ///   那时用户正要打字，`Ctrl+Enter` 不该被吃掉）。挂在某个 widget 上会随
    ///   焦点漂移，轮询全局输入则不会。
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // 有任务在跑就不接 —— 与底栏「执行替换」按钮的置灰条件一致。
        let running = self.job.is_some();
        let has_input = !self.input.trim().is_empty();
        let (ctrl, f5, ctrl_enter) = ctx.input(|i| {
            let m = i.modifiers;
            let c = m.ctrl || m.command;
            (c, i.key_pressed(egui::Key::F5), c && i.key_pressed(egui::Key::Enter))
        });

        if ctrl && ctx.input(|i| i.key_pressed(egui::Key::O)) {
            // 「选目录」的落点跟着当前页走：批量替换页选的是**输入目录**，
            // 另两页的起手动作是选文档（它们没有"目录"这个概念）。
            match self.page {
                Page::Replace => self.open_picker(PickFor::InputDir),
                Page::Header => self.open_picker(PickFor::HeaderDocs),
                Page::Print => self.open_picker(PickFor::PrintDocs),
            }
            return;
        }
        if f5 && !running && has_input {
            self.start_run(true, ctx);
            return;
        }
        if ctrl_enter && !running && has_input {
            self.start_run(false, ctx);
        }
    }

    // ── 拖放 ────────────────────────────────────────────────────────────

    /// 每帧收一次拖放输入，**落点按当前页决定**。
    ///
    /// 与自绘面板、系统对话框并列 —— 拖放只是"又一种把路径塞进来的壳"，
    /// 落点仍走 [`App::apply_picked`] 那条唯一漏斗，所以不会出现"拖进来的和
    /// 选进来的行为不一样"。
    ///
    /// ★ `dropped_files` / `hovered_files` 是 [`egui::RawInput`] 里**每帧**的
    ///   输入，落到哪一帧就在那一帧取走，**不能缓存**（缓存下来会重复消费同一批）。
    fn handle_drop(&mut self, ctx: &egui::Context) {
        // 拖拽中的提示：只在真的悬着东西时画，且不进页面布局。
        // 插进页面里的话，`hovered_files` 每帧都变，内容会被顶得来回跳。
        self.drop_overlay(ctx);

        // `DroppedFile.path` 是 `Option` —— 浏览器后端拖进来的是文件内容、
        // 没有路径。本程序只认本地路径，`None` 的直接丢掉（不能给它一个空串，
        // 那会变成"往清单里塞一条空路径"，后面才炸）。
        //
        // 正常情况下这里是空的；只有 `WREPL_DEBUG_DROP=hover|drop` 才会拿到东西
        // —— 本机合成鼠标进不来窗口（见 `diag` 模块开头），拖放是"按住不放"
        // 才有的状态，真实操作构造不出来，只能让程序自己做。
        let injected = crate::diag::debug_drop_mode();
        let dropped: Vec<PathBuf> = if let Some((hover, paths)) = &injected {
            if *hover {
                return; // 只画遮罩，不落地
            }
            paths.clone()
        } else {
            std::mem::take(&mut ctx.input(|i| i.raw.dropped_files.clone()))
                .into_iter()
                .filter_map(|f| f.path)
                .collect()
        };
        if dropped.is_empty() {
            return;
        }
        crate::diag::log(format!("拖放：收到 {} 项", dropped.len()));

        // 按住 Ctrl = 追加到现有内容，否则替换 —— 与资源管理器里 Ctrl 多选
        // 的习惯一致，也和 `picker.rs` 里那枚框选橡皮筋一致。
        let append = ctx.input(|i| i.modifiers.ctrl || i.modifiers.command);

        match self.page {
            // 「输入」栏本来就吃目录与文件的混合列表（`input_paths` 会遍历目录），
            // 所以这里**不过滤**，原样收下。
            Page::Replace => self.drop_into_input(dropped, append),
            // 两个「已选文档」入口的白名单是 `.doc` / `.docx`，与那两个按钮
            // 的选择器一致；目录**展开一层**（不递归）—— 递归展开会在拖进一个
            // 交付包目录时静默吞进几百个文件，用户没要求。
            Page::Header => self.drop_into_docs(dropped, append, crate::wordtool::PickDoc::Header),
            Page::Print => self.drop_into_docs(dropped, append, crate::wordtool::PickDoc::Print),
        }
    }

    /// 拖到「批量替换」页：写进 `input` 栏。
    ///
    /// 目录与文件**混着收** —— 工具自己会遍历目录，这里不替它做判断。
    fn drop_into_input(&mut self, paths: Vec<PathBuf>, append: bool) {
        let mut text = String::new();
        for p in &paths {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&p.display().to_string());
        }
        if append && !self.input.trim().is_empty() {
            let mut merged = self.input.trim_end().to_string();
            merged.push('\n');
            merged.push_str(&text);
            self.input = merged;
        } else {
            self.input = text;
        }
        self.push_log(format!(
            "拖入 {} 项到「输入」{}",
            paths.len(),
            if append { "（已追加）" } else { "" }
        ));
    }

    /// 拖到「页眉替换 / 打印与导出」两页：只收 Word 文档，目录展开一层。
    ///
    /// 白名单 `.doc` / `.docx` 与那两个按钮的选择器一致 —— 拖进来的和选进来
    /// 的必须是同一批东西，否则两条路会给出不同的候选集。
    fn drop_into_docs(
        &mut self,
        paths: Vec<PathBuf>,
        append: bool,
        which: crate::wordtool::PickDoc,
    ) {
        let mut kept: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        for p in &paths {
            if p.is_dir() {
                // 目录只展开一层，不递归。
                let Ok(rd) = std::fs::read_dir(p) else {
                    skipped += 1;
                    continue;
                };
                for e in rd.flatten() {
                    let ep = e.path();
                    if ep.is_file() && is_word_doc(&ep) {
                        kept.push(ep.display().to_string());
                    }
                }
            } else if is_word_doc(p) {
                kept.push(p.display().to_string());
            } else {
                skipped += 1;
            }
        }
        if kept.is_empty() {
            self.push_log(format!(
                "拖入的 {} 项里没有 Word 文档（只收 .doc / .docx）{}",
                paths.len(),
                if skipped > 0 {
                    format!("，已跳过 {skipped} 项")
                } else {
                    String::new()
                }
            ));
            return;
        }
        if append {
            // 追加：先取回现有列表再拼上，与 `apply_picked` 的"整表替换"区分开。
            let mut all: Vec<String> = self.wt.docs_of(which).to_vec();
            all.extend(kept.iter().cloned());
            self.wt.set_docs(which, all);
        } else {
            self.wt.set_docs(which, kept.clone());
        }
        self.push_log(format!(
            "拖入 {} 个 Word 文档到「{}」{}",
            kept.len(),
            match which {
                crate::wordtool::PickDoc::Print => "打印与导出",
                _ => "页眉替换",
            },
            if append { "（已追加）" } else { "" }
        ));
    }

    /// 拖拽进行中的整窗遮罩提示。
    ///
    /// "这个窗口能接拖放"在界面上没有任何静态线索，拖过来的当口得说一句。
    /// 用 `egui::Area`（`Order::Foreground` + `interactable(false)`）而不是往
    /// 页面里插一行 —— 后者会被上面那个"每帧都变"的问题顶得跳。
    fn drop_overlay(&self, ctx: &egui::Context) {
        // 真实悬停，或者 `WREPL_DEBUG_DROP=hover` —— 后者是为了截图/复现
        // 那个"按住不放"的状态，见 `diag::debug_drop_mode`。
        let forced = crate::diag::debug_drop_mode().is_some_and(|(h, _)| h);
        if !forced && ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let pal = ds::palette_of(ctx);
        // ★ 用 `egui::Area`（`Order::Foreground`）而不是往页面里插一行：
        //   插进布局的话 `hovered_files` 每帧都变，内容会被顶得来回跳。
        //   `interactable(false)` = 纯提示，不吃掉底下任何点击。
        //
        //   面积取 `content_rect`（面板区，不含顶栏 / 底栏那两条）——
        //   这也正是拖放真正能落下的范围；已弃用的 `screen_rect` 不要再用。
        egui::Area::new(egui::Id::new("wrepl-drop-overlay"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .fixed_pos(egui::pos2(0.0, 0.0))
            .show(ctx, |ui| {
                let rect = ui.ctx().content_rect();
                let (r, _) = ui.allocate_exact_size(rect.size(), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(
                    r,
                    6.0,
                    egui::Color32::from_black_alpha(if pal.dark_mode { 150 } else { 90 }),
                );
                painter.rect_stroke(
                    r.shrink(3.0),
                    6.0,
                    egui::Stroke::new(2.0_f32, pal.brand_default),
                    egui::StrokeKind::Middle,
                );
                painter.text(
                    r.center(),
                    egui::Align2::CENTER_CENTER,
                    "松手即可加入清单",
                    egui::FontId::proportional(20.0),
                    pal.text_primary,
                );
            });
    }

    /// 兜底：用系统原生对话框重来一遍（面板右下角「...」）。
    ///
    /// 逻辑与自绘面板**完全一致**，只是换了个选路径的壳 —— 结果都汇到
    /// [`App::apply_picked`] 同一处，不会因为走了哪条路而产生行为差异。
    fn fallback_dialog(&mut self, what: PickFor) {
        let picked: Option<Vec<PathBuf>> = match what {
            PickFor::InputDir | PickFor::OutputDir => {
                rfd::FileDialog::new().pick_folder().map(|p| vec![p])
            }
            PickFor::InputFiles => rfd::FileDialog::new()
                .add_filter("Word 文档", &["docx"])
                .pick_files(),
            PickFor::ImportTxt => rfd::FileDialog::new()
                .add_filter("文本规则文件", &["txt"])
                .pick_file()
                .map(|p| vec![p]),
            PickFor::ImportXlsx => rfd::FileDialog::new()
                .add_filter("Excel 规则表", &["xlsx"])
                .pick_file()
                .map(|p| vec![p]),
            PickFor::ExportXlsx => rfd::FileDialog::new()
                .add_filter("Excel 规则表", &["xlsx"])
                .set_file_name("规则.xlsx")
                .save_file()
                .map(|p| vec![p]),
            PickFor::HeaderDocs | PickFor::PrintDocs => rfd::FileDialog::new()
                .set_title(match what {
                    PickFor::HeaderDocs => "选择文档 —— 页眉替换",
                    _ => "选择文档 —— 打印与导出",
                })
                .add_filter("Word 文档", &["doc", "docx"])
                .pick_files(),
            PickFor::HeaderImage => rfd::FileDialog::new()
                .set_title("选择图片 —— 页眉替换")
                .add_filter("图片文件", &["png", "jpg", "jpeg", "bmp", "gif", "tif", "tiff"])
                .pick_file()
                .map(|p| vec![p]),
            // 两个「浏览」入口**没有**兜底：`Mode::Browse` 的面板本来就不摆「...」
            // 那个逃生口（浏览模式不"选"东西，原生对话框没有对应动作可做）。
            // 这一支留在 match 里只为穷尽性 —— 走不到。
            PickFor::BrowseDir | PickFor::BrowseReport => None,
        };
        if let Some(paths) = picked {
            self.apply_picked(what, paths);
        }
    }

    /// 从文本规则文件导入（**整表替换**语义）。
    ///
    /// `load_rules_file` 是**追加**语义（`--preset` 要它这样）；按钮这里要的是
    /// "整表替换"，所以先取走原表，解析失败再还回去——一次读文件失败不该把
    /// 界面上已经填好的规则清空。
    fn do_import_txt(&mut self, s: String) {
        let kept = std::mem::take(&mut self.rows);
        match self.load_rules_file(&s) {
            Ok(n) => {
                if !kept.is_empty() {
                    self.push_log(format!(
                        "原有 {} 条规则已清空，整表来自文本文件",
                        kept.len()
                    ));
                }
                self.push_log(format!("已从文本规则文件载入 {n} 条：{s}"));
                self.toast = Some((format!("已从文本文件导入 {n} 条规则"), true));
            }
            Err(e) => {
                self.rows = kept;
                self.error = Some(e);
            }
        }
    }

    fn do_import_xlsx(&mut self, s: String) {
        match self.load_rules_xlsx(&s) {
            Ok((n, sheet)) => {
                self.push_log(format!(
                    "已从 Excel 载入规则表（{n} 条，工作表「{sheet}」）：{s}"
                ));
                self.toast = Some((format!("已从 Excel 导入 {n} 条规则"), true));
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn do_export_xlsx(&mut self, s: String) {
        match self.export_rules_xlsx(&s) {
            Ok(n) => {
                self.push_log(format!("已导出 {n} 条规则到：{s}"));
                self.toast = Some((format!("已导出 {n} 条规则"), true));
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn section_result(&mut self, ui: &mut egui::Ui) {
        let Some(res) = &self.result else {
            return;
        };
        let title = if self.dry_run {
            "执行结果（未落盘）"
        } else {
            "执行结果"
        };
        section(ui, title, |ui| {
            ui.horizontal_wrapped(|ui| {
                chip(ui, "文件", res.outcomes.len().to_string(), DIM_C);
                chip(ui, "跳过", res.skipped.len().to_string(), DIM_C);
                chip(ui, "命中", res.total_hits().to_string(), DIM_C);
                chip(ui, "已替换", res.total_applied().to_string(), OK_C);
                chip(
                    ui,
                    "冲突",
                    res.total_conflicts().to_string(),
                    if res.total_conflicts() > 0 { WARN_C } else { DIM_C },
                );
                if !res.verify_rows.is_empty() {
                    let bad = res.verify_bad();
                    chip(
                        ui,
                        "格式保全",
                        format!("{}/{}", res.verify_rows.len() - bad, res.verify_rows.len()),
                        if bad > 0 { ERR_C } else { OK_C },
                    );
                }
                if !res.name_rows.is_empty() {
                    let n = res.name_rows.iter().filter(|x| x.changed).count();
                    chip(ui, "改名", n.to_string(), OK_C);
                }
            });

            if !res.skipped.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("跳过的文件").strong());
                // 固定露出几行，再多就出滚动条 —— 与规则表、日志共用同一个
                // `fixed_rows_scroll`。这几行的字号是 `.small()`，行高直接问当前
                // Style 要（`TextStyle::Small` 解析出来的 FontId），不写死数字。
                const VISIBLE_ROWS: usize = 4;
                let row_h = ui.text_style_height(&egui::TextStyle::Small);
                let row_gap = ui.spacing().item_spacing.y;
                fixed_rows_scroll("skipped-files", VISIBLE_ROWS, row_h, row_gap).show(ui, |ui| {
                    for s in &res.skipped {
                        ui.label(
                            RichText::new(format!(
                                "· {}　{}",
                                s.path.file_name().and_then(|x| x.to_str()).unwrap_or("?"),
                                s.reason
                            ))
                            .small()
                            .color(DIM_C),
                        );
                    }
                });
            }
        });
    }

    fn section_log(&mut self, ui: &mut egui::Ui) {
        // 各分区一律不带序号：「执行结果」没跑过之前不出现，
        // 一旦编号就会断号；而且序号本身对使用毫无帮助。
        section(ui, "运行日志", |ui| {
            // 只读：渲染成可选中的标签，不给文本框——可编辑的日志框会让人
            // 误以为改动有意义，而下一帧就会被真实轨迹覆盖掉。
            // 完整的一份在每次执行后落到产物目录的「运行日志.txt」里。
            //
            // 固定露出 `VISIBLE_ROWS` 行，更早的部分交给内层滚动条
            //（与「替换规则」表共用同一个 `fixed_rows_scroll`）。
            if self.log.is_empty() {
                // 空态：写清"日志是什么、会去哪"，比一个「空」字有用。
                ui.label(
                    RichText::new(
                        "尚未运行 —— 点「执行替换」后这里逐行显示过程；\
                         结束后完整的日志会写到产物目录的「运行日志.txt」",
                    )
                    .small()
                    .color(DIM_C),
                );
                return;
            }

            const VISIBLE_ROWS: usize = 5;

            // 一行日志的真实高度。日志是**中英混排**的等宽文本：含汉字的那一行由汉字
            // 字体决定行高（Proportional 家族里排第一的就是 CJK），纯 ASCII 行才由
            // 等宽字体决定。取两者较大值 —— 宁可盒子略高，也不把字裁掉。
            let row_h = ui.ctx().fonts_mut(|f| {
                f.row_height(&egui::FontId::monospace(ui_scale::LOG))
                    .max(f.row_height(&egui::FontId::proportional(ui_scale::LOG)))
            });
            let row_gap = ui.spacing().item_spacing.y;

            // 渲染上限：跑起来时 `log` 能涨到两万行（见 `log_cap`），全量逐帧铺一遍会卡死。
            // 只画最后 `SHOW` 行；更早的交待一句 —— 完整的一份在产物目录的日志文件里。
            const SHOW: usize = 300;
            let total = self.log.len();
            let start = total.saturating_sub(SHOW);
            if start > 0 {
                ui.label(
                    RichText::new(format!(
                        "……（前面还有 {start} 行未在此显示；完整日志见产物目录的「运行日志.txt」）"
                    ))
                    .small()
                    .color(DIM_C),
                );
            }

            // `stick_to_bottom`：日志天然跟着尾巴走 —— 跑到一半时人要看的是**最新**几行，
            // 钉在底部就不用每来一行都手动往下拖。人一旦自己往上滚，这个粘性会自动让位。
            // 这里**不能**用 `show_rows`：日志行会被折行（长路径），固定行高模型一旦被打破，
            // 滚动位置就对不上了。
            fixed_rows_scroll("run-log", VISIBLE_ROWS, row_h, row_gap)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log[start..] {
                        ui.label(RichText::new(line).monospace().size(ui_scale::LOG));
                    }
                });
        });
    }
}

/// eframe 的入口：每帧调一次 `ui`，其余全交给 `render`。
/// 这样布局代码也能在无窗口环境里被 `selftest` 直接驱动（见 `selftest`）。
///
/// ★ 2026-10-12：把布局从 `update(ctx)` 搬进 `ui(ui)`，并**删掉 `update`**。
///
///   起因是 egui 0.34 的面板迁移：`Panel::*::show_inside(ui, …)` 需要一枚 `Ui`，
///   而 `update` 只给 `ctx`。既然要动，就一步到位 —— `ui` 本来也是 0.34 起
///   推荐、且**必需**的那个入口（`update` 已 deprecated，eframe 内部
///   `epi_integration.rs:294/299` 先调 `update` 再调 `ui`，两条都在就都会跑）。
///   留着 `update` 只会多一条谁都不该再用的通路，还白白吃一个 `#[allow(deprecated)]`。
///
///   ⚠ 两条通路**不能同时**非空 —— 那会一帧画两遍。这里只留 `ui`。
impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render(ui);
    }

    /// 窗口清屏色。
    ///
    /// ★ 不能省（2026-10-11 踩过）：eframe 默认返回**全透明**，落到窗口上就是纯黑。
    /// 侧栏与内容区之间一旦有一道谁都没刷到的缝（侧栏 `Frame` 只铺 `exact_size`
    /// 宽、面板预留宽度还多一圈内边距，就会留缝），缝里露出来的正是这个颜色——
    /// 之前界面中间那条扎眼的黑条就是它。设成主题背景色，缝即便还在也隐身。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        ds::window_bg(self.light_theme)
    }
}

// ─────────────────────── 小工具 ───────────────────────

/// 页内分区：设计系统的 `Card`（标题走 h3、卡面带描边与阴影）。
///
/// 迁移前这里是 `Frame::group + 自定义蓝标题`；换成 `Card` 之后
/// 「文件与输出 / 替换规则 / 执行结果 / 运行日志」四块一次性都变成卡片，
/// **不用逐块改**——这也是 GUIDE §1「每个主题区块一张 `Card`」的推荐形态。
fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ds::Card::new().title(title).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

/// 固定露出 `rows` 行的纵向滚动区 —— **规则表 / 运行日志 / 跳过清单共用同一套高度算法**。
///
/// 参数：`row_h` = 一行内容的真实高度（**不含**行间距），`gap` = `item_spacing.y`。
/// 总高与 egui `show_rows` 内部所用的公式同源：
/// `rows * (row_h + gap) - gap`（最后一行后面不再加间距，所以减掉一个）。
///
/// ★★ 为什么必须**同时**给上限和下限（这是踩过的坑，别再改回去）
///
/// `max_height` 只是**上限**。egui 里视口高 = `min(父容器可用高度, 上限)`
/// （`scroll_area.rs` 的 `begin()`：`available_outer.size().at_most(max_size)`），
/// 而这几块的父容器正是 `CentralPanel` 那个**外层** `ScrollArea` ——
/// 1180×820 的窗口里它只给到一百出头的像素，表格于是被压成 2 行高
///（0.4.1 之前"规则表只露两行"就是这么来的）。
///
/// 解法是再给一条**下限** `min_scrolled_height`，它的文档语义正好合用：
/// *"只有当内容本身就比它矮时才会更小"* —— 内容够高就锁死在这 `rows` 行
///（超出部分交给这条内层滚动条承担），内容更矮则自然收缩、不留空白
///（配合 `auto_shrink([false, true])`，两种情形都对）。
///
/// 返回的是**配好的** `ScrollArea`，由调用方决定怎么消费：
/// · 大量**定高**行（规则表）→ 接 `.show_rows(ui, row_h, n, ..)`，只渲染可见区间；
/// · 内容会**折行**（日志、跳过清单）→ 接 `.show(ui, ..)`，**不能**用 `show_rows`
///   （固定行高模型一旦被折行打破，滚动位置就对不上）。
fn fixed_rows_scroll(id: &str, rows: usize, row_h: f32, gap: f32) -> egui::ScrollArea {
    let h = rows as f32 * (row_h + gap) - gap;
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(h)
        .min_scrolled_height(h)
        .auto_shrink([false, true])
}

/// 表头 / 隔行的淡底色。
///
/// 必须跟随主题：暗色主题下再用浅灰，会变成一块刺眼的白斑。
fn stripe_bg(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(0x33, 0x38, 0x41)
    } else {
        Color32::from_rgb(0xE2, 0xE8, 0xF0)
    }
}

/// 表格里占固定宽度、**内容左对齐**的一格。
///
/// 两个现成写法都不能直接用，都是踩过才知道的：
/// · `ui.add_sized([w, h], Label::new(..))` —— `add_sized` 内部是
///   `centered_and_justified`，而 Label 只占**文本自己**的宽度，被居中摆进格子；
///   于是文字居中，表头跟下面的输入框错开半个列宽。`Label::halign` 也救不了：
///   它调的是 galley 在 Label 自身 rect 里的位置，那个 rect 就等于文本宽度。
/// · `ui.allocate_ui_with_layout(.., left_to_right, ..)` —— 没有 justify，
///   格子被压成内容宽度，表格直接散架。
///
/// 所以：先用 `allocate_exact_size` 占死整格，再建一个贴着格子左上角排的子 Ui 画字。
fn fixed_cell(ui: &mut egui::Ui, w: f32, h: f32, text: RichText) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let mut cell = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let _ = cell.add(egui::Label::new(text).halign(egui::Align::LEFT));
    resp
}

fn chip(ui: &mut egui::Ui, label: &str, value: String, color: Color32) {
    ui.label(RichText::new(format!("{label} ")).small().color(DIM_C));
    ui.label(RichText::new(value).strong().color(color));
    ui.add_space(10.0);
}

fn short_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

/// 是不是 Word 文档 —— `.doc` / `.docx`，**不看大小写**。
///
/// 拖放的白名单与「页眉替换 / 打印与导出」那两页的选择器白名单是同一个来源
/// （`fallback_dialog` 里的 `add_filter("Word 文档", &["doc", "docx"])`），
/// 所以拖进来的必须是同一批东西，否则两条路给出的候选集会不一样。
///
/// 之所以自己判而不在 `is_dir()` 之后交给别处：拖进来的路径是**任意**用户
/// 塞进来的，什么都可能有，扩展名这一关不能省。
fn is_word_doc(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            e == "doc" || e == "docx"
        })
        .unwrap_or(false)
}

/// 「输入」那一栏文本里的**第一个**路径。
///
/// 那一栏可能是一行一个的多个路径、也可能用 `;` / `；` 隔开（见 [`App::input_paths`]）。
/// 打开选择器时拿它当起点，人就不用从 `C:\` 一层层点回去。取不出就返回空，
/// 面板会回退到自己记住的上次目录。
fn first_path(s: &str) -> PathBuf {
    s.split(['\n', '\r', ';', '；'])
        .map(str::trim)
        .find(|x| !x.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// 「已选文档」列表里第一条的路径 —— 给选择器当起点用。
///
/// 交给第一条**文件**而不是它所在的目录：`Picker::open` 见到文件会自己退到
/// 它所在目录，这样起点逻辑留在选择器一处，这里不用重复判断。
fn doc_start(docs: &[String]) -> PathBuf {
    docs.first().map(PathBuf::from).unwrap_or_default()
}

pub fn verdict_text(res: &RunResult, dry: bool) -> String {
    if res.failed() > 0 {
        format!("不合格：{} 个文件处理失败，见日志", res.failed())
    } else if res.verify_bad() > 0 {
        format!("不合格：{} 个文件未通过执行后自动验证", res.verify_bad())
    } else if res.total_conflicts() > 0 {
        format!("有条件通过：{} 处区间冲突已跳过，须人工复核", res.total_conflicts())
    } else if dry {
        format!(
            "预演完成：命中 {} 处、替换 {} 处，未写入任何文件",
            res.total_hits(),
            res.total_applied()
        )
    } else {
        format!(
            "合格：命中 {} 处、替换 {} 处，无冲突；格式保全 {}/{} 通过",
            res.total_hits(),
            res.total_applied(),
            res.verify_rows.iter().filter(|v| v.ok()).count(),
            res.verify_rows.len()
        )
    }
}

fn write_report(
    rp: &str,
    res: &RunResult,
    rules: &[Rule],
    inputs: &[PathBuf],
    opts: &Options,
) -> Result<PathBuf, String> {
    let path = PathBuf::from(rp);
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败 {dir:?}：{e}"))?;
        }
    }

    let file_rows: Vec<report::FileRow> = res
        .outcomes
        .iter()
        .map(|o| report::FileRow {
            file: o
                .src
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
            path: o.src.display().to_string(),
            size_kb: o.size_kb,
            mtime: o.mtime.clone(),
            rule_count: o.rule_count,
            total_hits: o.hits.len(),
            applied: o.applied,
            conflicts: o.conflicts,
            status: o.status.to_string(),
            sha_before: short_sha(&o.sha_before),
            sha_after: short_sha(&o.sha_after),
            parts_changed: o.parts.join(" "),
            note: o.note.clone(),
        })
        .collect();

    let mut file_rows = file_rows;
    for s in &res.skipped {
        file_rows.push(report::FileRow {
            file: s
                .path
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_string(),
            path: s.path.display().to_string(),
            size_kb: std::fs::metadata(&s.path)
                .map(|m| m.len() as f64 / 1024.0)
                .unwrap_or(0.0),
            mtime: String::new(),
            rule_count: rules.len(),
            total_hits: 0,
            applied: 0,
            conflicts: 0,
            status: "SKIP".to_string(),
            sha_before: String::new(),
            sha_after: String::new(),
            parts_changed: String::new(),
            note: s.reason.clone(),
        });
    }

    let mut hit_rows: Vec<report::HitRow> = Vec::new();
    for o in &res.outcomes {
        let fname = o
            .src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        for h in &o.hits {
            let find = rules
                .iter()
                .find(|r| r.id == h.rule_id)
                .map(|r| r.find.clone())
                .unwrap_or_default();
            hit_rows.push(report::HitRow {
                file: fname.clone(),
                rule_id: h.rule_id,
                find,
                replace: h.replaced_by.clone(),
                slot: h.slot.label().to_string(),
                para: h.para,
                matched: h.matched.clone(),
                applied: h.applied,
                reason: h.reason.clone(),
                strategy: h.strategy.clone(),
            });
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let operator = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "(未知)".to_string());

    let meta = report::ReportMeta {
        tool: "wrepl-gui".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        generated_at: pipeline::format_epoch(now),
        operator,
        input: inputs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("　；　"),
        output: if opts.in_place {
            "（就地替换源文件）".to_string()
        } else {
            opts.out_dir
                .as_ref()
                .map(|d| d.display().to_string())
                .unwrap_or_default()
        },
        rule_source: "图形界面规则表（见「规则清单」表）".to_string(),
        mode: {
            let mut m = if opts.in_place {
                if opts.backup {
                    "就地替换源文件（保留 .bak 备份）".to_string()
                } else {
                    "就地替换源文件（原文件上直接覆盖，不留备份）".to_string()
                }
            } else {
                "写出到新目录（原文件不动）".to_string()
            };
            if opts.mirror {
                m.push_str("　＋　完整镜像（未改动的文件也复制到输出目录）");
            }
            if opts.rename_files {
                m.push_str("　＋　文件名同步改名");
            }
            if opts.verify_after {
                m.push_str("　＋　执行后自动验证");
            }
            m
        },
        verdict: verdict_text(res, false),
    };

    let rule_rows: Vec<report::RuleRow> = rules
        .iter()
        .map(|r| report::RuleRow {
            id: r.id,
            enabled: r.enabled,
            find: r.find.clone(),
            replace: r.replace.clone(),
            scope: r.scope.display().to_string(),
            case_sensitive: r.case_sensitive,
            whole_word: r.whole_word,
            kana_sensitive: r.kana_sensitive,
            wildcard: r.use_wildcard,
            note: r.note.clone(),
        })
        .collect();

    report::write_xlsx(
        &path,
        &meta,
        &file_rows,
        &hit_rows,
        &rule_rows,
        &res.name_rows,
        &res.verify_rows,
    )
    .map_err(|e| format!("{e:#}"))?;

    Ok(path)
}

fn short_sha(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        s.chars().take(16).collect()
    }
}

// ─────────────────────── 字号与字体 ───────────────────────

/// 界面度量：**所有字号、以及"必须跟着字号走"的行高，只在这里定义**。
///
/// 别处一律引用这些常量。想整体调大/调小界面，改这一个模块就够 —— 不必再去
/// `app.rs` / `picker.rs` 里翻散落的数字。**那正是改字号最常见的事故来源**：
/// 字放大了、装它的格子没放大，文字就被裁掉（本文件里有一堆 `add_sized`
/// 是显式给宽高的，不跟着改就会溢出）。
///
/// 取值理由（2026-10-09，本机 1920×1080、缩放 100%）：egui 的默认值偏小 ——
/// `Body` 12.5、`Button` 12.5、**`Small` 只有 9.0**，而 `Small` 正是表头、计数、
/// 路径提示和 `.small()` 按钮在用的字号（本仓库 11 处），9 点几乎要凑近看。
/// 这里整体比默认放大 15%~30%。**只动数值，不动布局结构。**
pub mod ui_scale {
    // 注：原 `SECTION`(17) / `BODY`(16) / `BUTTON`(16) 三个常量随第 2、3 步迁移
    // 到设计系统后已无引用 —— 分区标题、正文、按钮字号都改由 egui_sauge 的
    // 九档字号层级（h3 / body / button）与 `ds::Button` 自己管，删掉以免留死代码。
    /// 日志与等宽文本。
    pub const LOG: f32 = 14.0;
    /// 辅助小字：表头、计数、提示、`.small()` 按钮。**egui 默认 9.0，太小了。**
    pub const SMALL: f32 = 11.5;
    /// 文件选择器：文件名 / 次要信息。
    pub const PICK_NAME: f32 = 14.5;
    pub const PICK_META: f32 = 13.0;
    /// 文件选择器左侧导航栏的宽度（分区标题 + 「图标 + 文字」行）。
    pub const PICK_SIDE_W: f32 = 172.0;

    /// 规则表格一行的高度（里面装的是 `BODY` 号单行输入框）。
    pub const ROW_H: f32 = 28.0;
    /// 单个紧凑输入框/标签的高度（路径、排除、下拉）。
    pub const EDIT_H: f32 = 26.0;
    /// 文件选择器列表的一行。**28 而不是 24**：对着 Files 的观感调过 ——
    /// 24 点在放大后的字号下显得挤，行与行的呼吸感不够。
    pub const PICK_ROW_H: f32 = 28.0;
    /// 文件选择器里自绘导航图标（后退 / 前进 / 上一级 / 刷新）的边长。
    pub const NAV_BTN: f32 = 26.0;
}


// ─────────────────────── 无窗口自检 ───────────────────────

/// `wrepl-gui --selftest <输入> <输出目录> [规则文件]`
///
/// 用来回归 **GUI 的接线**（不是替换引擎——那是 CLI 回归的职责）：
/// 真加载字体、真跑几帧布局、再真跑一次完整批量。返回进程退出码。
pub fn selftest(args: &[String]) -> i32 {
    println!("wrepl-gui 自检");
    let mut it = args.iter().skip_while(|a| *a != "--selftest").skip(1);
    let preset = Preset {
        input: it.next().cloned().unwrap_or_default(),
        output: it.next().cloned().unwrap_or_default(),
        rules_file: it.next().filter(|a| !a.starts_with("--")).cloned(),
        // 自检要把「文件名同步改名」这条路也走一遍，所以显式打开；
        // 正常的 --preset 开窗仍然默认不勾（见 Preset::from_args）。
        rename: true,
        // 自检**必须**写副本：绝不允许就地替换（下面还有一道硬保护）。
        in_place: false,
    };

    // 1) 字体
    let ctx = egui::Context::default();
    // 走**和开窗同一条路**：设计系统字体表 + 主题。少了 `ds::apply`，
    // 无头这几帧就是在用 egui 默认样式画，等于没在验真实外观。
    ds::install(&ctx);
    ds::apply(&ctx, true);
    println!("  设计系统 + 字体加载完成");

    // 2) 构造界面状态——与 `--preset` 开窗**同一条路**，避免自检测的是另一套代码
    let mut app = App::with_preset(&preset);
    // 硬保护：自检绝不允许走「就地替换」。给漏了输出目录就直接退出，
    // 免得某天有人改参数把测试语料本身改掉。
    if !app.out_to_subdir {
        println!("  ✗ 自检必须提供输出目录（自检不做就地替换）");
        return 2;
    }
    if app.rows.is_empty() {
        app.rows.push(Row::new("示例查找", "示例替换", "全部"));
    }
    println!("  规则 {} 条", app.rows.len());
    // 装载规则时说过的话（跳过了哪几条、为什么）要一并吐出来。
    // 只写进界面日志的话，无头自检就什么也发现不了——而这些恰恰是最该被
    // 自动检查的部分：静默少跑几条规则，在界面上根本看不出来。
    for line in &app.log {
        println!("  · {line}");
    }
    if let Some(e) = &app.error {
        println!("  ✗ {e}");
        return 1;
    }

    // 3) 真跑几帧布局（不开窗口）——排版代码里的 panic 会在这里暴露
    let mut raw = egui::RawInput::default();
    raw.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(1280.0, 800.0),
    ));
    for _ in 0..3 {
        let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
    }
    println!("  布局 3 帧通过（无 panic）");

    // 3.5) 规则的 Excel 往返：导出 → 读回。走的是界面上那两个按钮的同一条路
    //      （只是绕开了文件对话框），用来验证 xlsx 读写的接线。
    let xlsx = Path::new(&preset.output)
        .join("规则-自检往返.xlsx")
        .display()
        .to_string();
    match app.export_rules_xlsx(&xlsx) {
        Ok(n) => match app.load_rules_xlsx(&xlsx) {
            Ok((m, sheet)) if m == n => {
                println!("  规则 Excel 往返通过（导出 {n} 条 / 读回 {m} 条，工作表「{sheet}」）");
            }
            Ok((m, _)) => {
                println!("  ✗ 规则 Excel 往返条数不一致：导出 {n} / 读回 {m}");
                return 2;
            }
            Err(e) => {
                println!("  ✗ 规则 Excel 读回失败：{e}");
                return 2;
            }
        },
        Err(e) => {
            println!("  ✗ 规则 Excel 导出失败：{e}");
            return 2;
        }
    }

    // 3.6) 新增的两处控件只在特定交互下才画得出来，普通一趟渲染照不到：
    //      · 输入框的多行形态（选了几个文件之后）
    //      · 「清空规则」的待命态（点过第一下之后）
    //      这两处一旦排版出问题（宽度溢出、高度算负），只能在真窗口里当场发现，
    //      所以在这里各画两帧钉住。
    let keep_input = app.input.clone();
    app.input = (1..=7)
        .map(|i| format!("D:/wrepl-selftest/第{i}个.docx"))
        .collect::<Vec<_>>()
        .join("\n");
    app.clear_armed = true;
    for _ in 0..2 {
        let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
    }
    app.clear_armed = false;
    app.input = keep_input;
    println!("  多行输入框 + 「清空规则」待命态：布局 2 帧通过");

    // 3.7) 「完整镜像」复选框的两态：可用（写副本）与置灰（就地替换）。
    //      它是跟着「输出到子文件夹」启停的，普通一趟渲染只能照到其中一态。
    let keep_out = app.out_to_subdir;
    app.out_to_subdir = false;
    let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
    app.out_to_subdir = true;
    app.mirror = true;
    let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
    app.mirror = false;
    app.out_to_subdir = keep_out;
    println!("  「完整镜像」可用 / 置灰两态：布局通过");

    // 3.8) 拖放：遮罩（拖拽中）与落点（三个页面各一种）都要画得出来。
    //
    // 遮罩那条路平时照不到 —— `hovered_files` 只在真的悬着东西时非空，
    // 而它一帧一变，普通渲染跑不到。落点三页则各自有独立的过滤规则
    // （批量替换不过滤、另两页只收 .doc/.docx 且目录展开一层），
    // 走错一条就是"拖进来的和选进来的候选集不一样"。
    {
        use crate::wordtool::PickDoc;

        // 拖放这一段会改 `input` / `page` / 两个文档列表，后面 3.9 的预演要用
        // `input` —— 先存一份再动手。
        let keep_input = app.input.clone();
        let keep_page = app.page;

        fn dropped(p: &[&str]) -> Vec<egui::DroppedFile> {
            p.iter()
                .map(|s| egui::DroppedFile {
                    path: Some(PathBuf::from(s)),
                    ..Default::default()
                })
                .collect()
        }

        // 遮罩：悬停时画一帧（不 drop，只看布局不炸）。
        // `HoveredFile` 只有 `path` + `mime`（没有 `DroppedFile` 那些字节/时间戳）。
        let mut hover = raw.clone();
        hover.hovered_files = ["D:/wrepl-selftest/x.docx"]
            .iter()
            .map(|s| egui::HoveredFile {
                path: Some(PathBuf::from(s)),
                ..Default::default()
            })
            .collect();
        let _ = ctx.run_ui(hover, |ui| app.render(ui));
        println!("  拖拽悬停遮罩：布局通过");

        // 落点 ①：批量替换页 —— 目录与文件混着收，不过滤。
        app.page = Page::Replace;
        let mut d1 = raw.clone();
        d1.dropped_files = dropped(&["D:/wrepl-selftest/目录", "D:/wrepl-selftest/甲.docx"]);
        let _ = ctx.run_ui(d1, |ui| app.render(ui));
        let got = app.input.clone();
        let want = "D:/wrepl-selftest/目录\nD:/wrepl-selftest/甲.docx";
        if got.trim() != want {
            println!("  ✗ 拖放到「输入」不对：\n    期望 {want}\n    实际 {got}");
            return 3;
        }
        println!("  拖放到「输入」（目录+文件混收）：通过");

        // 不按 Ctrl 再拖一次 = **替换**（不是追加）。这条要钉住：
        // 「第二次拖放没按 Ctrl 就把上一次的清掉了」是设计如此，不是 bug ——
        // 钉它是为了让"没按 Ctrl 会清空"这件事变成有意为之，而不是碰巧。
        let mut d2 = raw.clone();
        d2.dropped_files = dropped(&["D:/wrepl-selftest/乙.docx"]);
        let _ = ctx.run_ui(d2, |ui| app.render(ui));
        let got = app.input.clone();
        if got.trim() != "D:/wrepl-selftest/乙.docx" {
            println!("  ✗ 不按 Ctrl 拖放应是替换语义，实际：{got}");
            return 3;
        }
        println!("  第二次拖放（无 Ctrl）替换而非追加：通过");

        // 按住 Ctrl 再拖 = **追加**，上一次的还在。
        let mut d3 = raw.clone();
        d3.dropped_files = dropped(&["D:/wrepl-selftest/丙.docx"]);
        d3.modifiers.ctrl = true;
        let _ = ctx.run_ui(d3, |ui| app.render(ui));
        let got = app.input.clone();
        if !got.contains("乙.docx") || !got.contains("丙.docx") {
            println!("  ✗ 按住 Ctrl 拖放应追加，实际：{got}");
            return 3;
        }
        println!("  按住 Ctrl 拖放追加：通过");
        app.input.clear();

        // 落点 ②：页眉替换页 —— 只收 Word 文档，别的静默跳过。
        app.page = Page::Header;
        let mut d4 = raw.clone();
        d4.dropped_files = dropped(&[
            "D:/wrepl-selftest/甲.docx",
            "D:/wrepl-selftest/笔记.txt",
            "D:/wrepl-selftest/报表.xlsx",
        ]);
        let _ = ctx.run_ui(d4, |ui| app.render(ui));
        let docs = app.wt.docs_of(PickDoc::Header);
        if docs.len() != 1 || !docs[0].ends_with("甲.docx") {
            println!("  ✗ 页眉页拖放白名单不对：{docs:?}");
            return 3;
        }
        println!("  拖放到「页眉替换」（只收 .doc/.docx）：通过");

        // 落点 ③：打印与导出页 —— 走的是另一个列表，别串到页眉那份。
        app.page = Page::Print;
        let mut d5 = raw.clone();
        d5.dropped_files = dropped(&["D:/wrepl-selftest/丙.doc"]);
        let _ = ctx.run_ui(d5, |ui| app.render(ui));
        // 两个断言合起来钉住"落点独立"：打印页自己收到，且**没有**串到页眉那份。
        if app.wt.docs_of(PickDoc::Print).len() != 1
            || !app.wt.docs_of(PickDoc::Print)[0].ends_with("丙.doc")
        {
            println!(
                "  ✗ 打印与导出页没收到拖进来的文档：{:?}",
                app.wt.docs_of(PickDoc::Print)
            );
            return 3;
        }
        if app.wt.docs_of(PickDoc::Header).iter().any(|s| s.ends_with("丙.doc")) {
            println!("  ✗ 打印页的拖放串到了页眉那份列表里");
            return 3;
        }
        println!("  拖放到「打印与导出」（.doc 也收、落点独立）：通过");

        // 复原：拖放这一段动了 `input`、`page` 与两个文档列表，
        // 后面 3.9 的预演还要用 `input` —— 留着改过的值会连累它。
        app.page = keep_page;
        app.input = keep_input;
        app.wt.set_docs(PickDoc::Header, Vec::new());
        app.wt.set_docs(PickDoc::Print, Vec::new());
    }

    // 3.7b) 三个快捷键：`Ctrl+O` / `F5` / `Ctrl+Enter`。
    //
    // 这三条都是**静默退化型** —— 绑错了不报错，只是按了没反应或做错事。
    // 关键是要钉住它们"落到既有入口"这件事：`F5` 走预演（不落盘）、
    // `Ctrl+Enter` 走实跑，而实跑在就地覆盖时**必须先弹确认** ——
    // 快捷键不能绕过那道关（它兜在 `start_run` 入口里）。
    {
        use crate::wordtool::PickDoc;

        let keep_input = app.input.clone();
        let keep_page = app.page;
        // ★ 这两样**必须原样还回去**。本块为了验「Ctrl+Enter 就地覆盖仍弹确认」，
        //   把 `out_to_subdir` 压成 `false`；若忘了恢复，后面第 4 步那次「完整批」
        //   就会被 `start_run` 入口的「就地确认」关拦住 —— 静默 return、什么也不跑，
        //   产物目录零文件、运行日志与报告都不落地，而自检照样打印"执行完成"。
        //   2026-10-12 实测踩过一次（回归第 9 组 4 项转红），故原值一律留存还原。
        let keep_out = app.out_to_subdir;
        let keep_confirm = app.confirm_inplace;

        /// 造一帧"按下了某个键"的输入。
        fn key(raw: &egui::RawInput, k: egui::Key, ctrl: bool) -> egui::RawInput {
            let mut r = raw.clone();
            r.modifiers.ctrl = ctrl;
            r.events.push(egui::Event::Key {
                key: k,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: r.modifiers,
            });
            r
        }

        // ── `Ctrl+O` 落点跟着当前页走 ──
        for (page, page_name, want) in [
            (Page::Replace, "批量替换", PickFor::InputDir),
            (Page::Header, "页眉替换", PickFor::HeaderDocs),
            (Page::Print, "打印与导出", PickFor::PrintDocs),
        ] {
            app.page = page;
            let _ = ctx.run_ui(key(&raw, egui::Key::O, true), |ui| app.render(ui));
            if app.picker_for != Some(want) {
                println!(
                    "  ✗ Ctrl+O 在「{page_name}」页应开 {want:?}，实际 {:?}",
                    app.picker_for
                );
                return 3;
            }
            app.picker.close();
            app.picker_for = None;
        }
        println!("  Ctrl+O 三页各自落到对应选择器：通过");

        // ── `F5` = 预演（dry），不落盘 ──
        app.page = Page::Replace;
        app.input = "D:/wrepl-selftest/不存在也没关系.docx".to_string();
        let _ = ctx.run_ui(key(&raw, egui::Key::F5, false), |ui| app.render(ui));
        // 预演不该要求"就地覆盖确认" —— 它不写任何东西。
        if app.confirm_inplace {
            println!("  ✗ F5 预演不该弹就地覆盖确认框");
            return 3;
        }
        println!("  F5 预演不弹确认框（不落盘）：通过");

        // ── 没填输入时两个执行键都不该起跑 ──
        app.input.clear();
        let _ = ctx.run_ui(key(&raw, egui::Key::F5, false), |ui| app.render(ui));
        let _ = ctx.run_ui(key(&raw, egui::Key::Enter, true), |ui| app.render(ui));
        if app.confirm_inplace {
            println!("  ✗ 没填输入时按 Ctrl+Enter 不该弹确认框");
            return 3;
        }
        println!("  没填输入时 F5 / Ctrl+Enter 都不起跑：通过");

        // ── `Ctrl+Enter` 就地覆盖 → 必须弹确认（快捷键不绕过那道关）──
        app.input = "D:/wrepl-selftest/甲.docx".to_string();
        app.out_to_subdir = false;
        let _ = ctx.run_ui(key(&raw, egui::Key::Enter, true), |ui| app.render(ui));
        if !app.confirm_inplace {
            println!("  ✗ Ctrl+Enter 就地覆盖时必须弹确认框（它绕过了 start_run 的把关）");
            return 3;
        }
        app.confirm_inplace = false;
        println!("  Ctrl+Enter 就地覆盖仍走确认框：通过");

        app.page = keep_page;
        app.input = keep_input;
        app.out_to_subdir = keep_out; // ← 关键：不还原会让第 4 步的完整批被就地确认关拦空跑
        app.confirm_inplace = keep_confirm;
        app.wt.set_docs(PickDoc::Header, Vec::new());
    }

    // 3.8) 新增的两页（页眉替换 / 打印与导出）骨架，外加页眉替换的**两种落盘状态**
    //      （写副本 / 就地覆盖），以及就地覆盖那张确认框。**只是画，不触发任何动作**，
    //      所以无头环境、没装 Word 的机器上也能跑。
    //      为什么两种都要摆出来：就地覆盖那一档会多画一个确认框，而本机的合成鼠标
    //      事件进不到窗口（见 `diag` 模块头）—— 不在这里主动摆出每种组合、各画一帧，
    //      那些分支就永远没被渲染过，排版里的 panic 也就一路躲到用户手上才爆。
    //      （引擎单选 2026-10-19 已随 COM 退役删除，这里不再遍历两套引擎。）
    //      塞几行假路径是为了让「已选文档」列表与计数这两种状态也画出来。
    let keep_page = app.page;
    let keep_hdr_mode = app.wt.hdr_mode;
    let keep_in_place = app.wt.hdr_in_place;
    let keep_confirm = app.wt.hdr_confirm;
    for (page, mode) in [
        (Page::Header, HeaderMode::Text),
        (Page::Header, HeaderMode::Image),
        (Page::Print, HeaderMode::Text),
    ] {
        app.page = page;
        app.wt.hdr_mode = mode;
        app.wt.hdr_docs = (1..=3)
            .map(|i| format!("D:/wrepl-selftest/H{i}.docx"))
            .collect();
        app.wt.print_docs = (1..=3)
            .map(|i| format!("D:/wrepl-selftest/P{i}.docx"))
            .collect();
        for in_place in [false, true] {
            app.wt.hdr_in_place = in_place;
            // 确认框只在"就地覆盖"这条路上出现
            app.wt.hdr_confirm = in_place;
            for _ in 0..2 {
                let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
            }
        }
    }
    // 3.8c) 「打印与导出」页的两种下游：送打印机 / 落 PDF。PDF 那档会多出
    //      「输出目录」「用源文件目录」「PDF/A」三件，页码写错时还要走一条
    //      `InputField::error` 的报错分支 —— 四种状态各画两帧。
    //      理由与上一条相同：**只在某个选择之后才画得出来的控件，不主动摆出来
    //      就永远没被渲染过**，排版里的 panic 会一路躲到用户手上。
    let keep_sink = app.wt.sink;
    let keep_range = app.wt.page_range.clone();
    let keep_pdf_a = app.wt.pdf_a;
    app.page = Page::Print;
    app.wt.print_docs = (1..=2)
        .map(|i| format!("D:/wrepl-selftest/Q{i}.docx"))
        .collect();
    for (sink, range, pdf_a) in [
        (Sink::Printer, "", false),
        (Sink::Pdf, "", false),
        (Sink::Pdf, "3-7", true),
        // 多段页码：导出这边解析必然失败 -> 输入框红字 +「导出 PDF」置灰
        (Sink::Pdf, "1-3,5", false),
    ] {
        app.wt.sink = sink;
        app.wt.page_range = range.to_string();
        app.wt.pdf_a = pdf_a;
        for _ in 0..2 {
            let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
        }
    }
    app.wt.sink = keep_sink;
    app.wt.page_range = keep_range;
    app.wt.pdf_a = keep_pdf_a;
    println!("  打印与导出（打印机 / PDF / PDF-A / 页码写错）：布局通过");

    // 3.8d) 引擎不是真 Word 的那两种状态：① 只有 WPS —— 页面顶部多一条常驻
    //       黄色横幅、执行卡片里多一行拒绝理由（打印那档）、点导出会弹确认框；
    //       ② 两样都没装 —— 红色横幅 + 两条下游都灰。
    //       `wps_confirm` 是 `pub` 就是为了这里：合成鼠标事件进不到窗口
    //       （见 `diag` 模块头的说明），不主动置上，弹窗那一帧永远渲染不到。
    let keep_probe = app.wt.engine;
    let keep_wps_confirm = app.wt.wps_confirm;
    for probe in [
        crate::wordtool::engine::EngineProbe::WpsOnly,
        crate::wordtool::engine::EngineProbe::None,
    ] {
        app.wt.engine = Some(probe);
        for sink in [Sink::Printer, Sink::Pdf] {
            app.wt.sink = sink;
            app.wt.page_range = String::new();
            for _ in 0..2 {
                let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
            }
        }
        // 只有 WPS 时还会弹确认框 —— 单独照一帧
        app.wt.sink = Sink::Pdf;
        app.wt.wps_confirm = probe == crate::wordtool::engine::EngineProbe::WpsOnly;
        for _ in 0..2 {
            let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
        }
        app.wt.wps_confirm = false;
    }
    app.wt.engine = keep_probe;
    app.wt.wps_confirm = keep_wps_confirm;
    app.wt.sink = keep_sink;
    println!("  打印与导出（只有 WPS / 什么都没装 / WPS 确认框）：布局通过");

    app.page = keep_page;
    app.wt.hdr_mode = keep_hdr_mode;
    app.wt.hdr_in_place = keep_in_place;
    app.wt.hdr_confirm = keep_confirm;
    app.wt.hdr_docs.clear();
    app.wt.print_docs.clear();
    println!("  页眉替换（文本 / 图片 × 两种落盘 + 确认框）+ 打印与导出：布局通过");

    // 3.8b) 自绘选择器里**只有浏览模式才有的**那套底部按钮：确定键是「打开」
    //       （选中文件才可按）、取消键是「关闭」（不是「取消」），而且**不摆**
    //       「...」那个系统对话框逃生口。它与另外三种模式的底部排布不同 ——
    //       正是"新分支没被画过"的典型（本机合成鼠标进不到窗口），所以单独照两帧。
    //       顺带照一帧「选择图片」面板：页眉替换页 2026-10-11 并入自绘后才有的入口。
    let keep_page2 = app.page;
    app.page = Page::Replace; // 面板浮在任意页之上，底页画哪张都一样
    for (what, cn) in [
        (PickFor::BrowseDir, "浏览"),
        (PickFor::HeaderImage, "选图片"),
    ] {
        app.open_picker(what);
        for _ in 0..2 {
            let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
        }
        // 照完就收：面板是**常驻**的（跨次打开记着上次停在哪），不收的话
        // 后面几帧会一直浮着那块全屏遮罩，把后面的自检项全挡住。
        app.picker.close();
        app.picker_for = None;
        println!("  自绘选择器 —— {cn}模式：布局通过");
    }

    // 3.8c) 选择器窗口的**定位**：标题栏那条空白拖得动。
    //
    //   2026-10-12 修掉的 bug：原先用 `Window::anchor(CENTER_CENTER)`，而 egui 的
    //   anchor 是**每帧**按"本帧窗口尺寸"重算左上角的（`area.rs`：
    //   `if let Some((anchor, offset)) = anchor { state.set_left_top_pos(
    //    anchor.align_size_within_rect(size, constrain_rect).left_top() + offset) }`，
    //   其中 `size` 就是本帧尺寸）。于是两件事一起发生：
    //     · 切换「桌面 / 文档 / 下载 / 盘符」这些快捷位时，列表内容不同 → 窗口
    //       尺寸不同 → 左上角被重算 —— 屏幕上就是左右跳（用户报的「左右抖动」）；
    //     · 同一套 API 的文档写着 "Anchoring also makes the window immovable."，
    //       顺手把拖动也禁掉了（用户想拖拖不动）。
    //   改 `default_pos`（源码原话 "Set initial position of the window"）后：
    //   位置只在没有记忆时取一次，之后归 egui 记忆，拖走就记住。
    //
    //   ★ 这里用**合成指针事件**验，而不是去截图上量像素：本机真实鼠标事件
    //     进不了窗口（见 `diag` 模块头），而"拖不拖得动"在静态截图上根本看不出来。
    //     拖动区域是**整个窗口**（`area.rs` 的 `move_response` 覆盖 `state.rect()`，
    //     后建的控件压在它上面优先拿事件），所以点标题栏那条空白最保险。
    {
        app.page = Page::Replace;
        app.open_picker(PickFor::BrowseDir);
        for _ in 0..3 {
            let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui));
        }
        let Some(before) = app.picker.last_window_rect() else {
            println!("  ✗ 选择器窗口没画出来（拿不到矩形）");
            return 3;
        };
        // 起点取窗口顶边往下 20 点 —— 标题栏那条空白（避开右上角的 ×）。
        let grab = egui::pos2(before.center().x, before.top() + 20.0);
        let to = grab + egui::vec2(-80.0, -35.0);
        let ev = |e: egui::Event| {
            let mut r = raw.clone();
            r.events.push(e);
            r
        };
        let mv = |p: egui::Pos2| egui::Event::PointerMoved(p);
        let btn = |p: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let _ = ctx.run_ui(ev(mv(grab)), |ui| app.render(ui)); // 先 hover
        let _ = ctx.run_ui(ev(btn(grab, true)), |ui| app.render(ui)); // 按下
        let _ = ctx.run_ui(ev(mv(to)), |ui| app.render(ui)); // 拖
        let _ = ctx.run_ui(ev(mv(to)), |ui| app.render(ui)); // 再拖一帧
        let _ = ctx.run_ui(ev(btn(to, false)), |ui| app.render(ui)); // 松开
        let _ = ctx.run_ui(raw.clone(), |ui| app.render(ui)); // 收尾帧
        let after = app.picker.last_window_rect().unwrap_or(before);
        let (dx, dy) = (after.left() - before.left(), after.top() - before.top());
        if dx.abs() < 20.0 && dy.abs() < 20.0 {
            println!(
                "  ✗ 选择器标题栏拖不动（位移 {dx:.1},{dy:.1}）—— 是不是又用回 \
                 anchor / current_pos 了？那两个都会让窗口 immovable"
            );
            return 3;
        }
        println!("  选择器标题栏可拖动：位移 ({dx:.1}, {dy:.1})");
        app.picker.close();
        app.picker_for = None;
    }

    app.page = keep_page2;

    // 3.9) 打印机枚举。打印与导出页的「打印机」下拉全靠它，而它**不碰 COM**
    //      （纯 Win32 `EnumPrintersW`），所以无头环境照样能验。
    //      这里只是"报出来"：一台都没装打印机是合法情形，不该判失败。
    app.wt.reload_printers();
    match &app.wt.printers_err {
        Some(e) => println!("  · 打印机枚举失败：{e}（打印与导出页会显示这条，仍可用默认打印机）"),
        None => {
            println!("  · 打印机枚举：{} 台", app.wt.printers.len());
            for p in app.wt.printers.iter().take(3) {
                println!(
                    "      {}{}（端口 {}）",
                    p.name,
                    if p.is_default { " ←系统默认" } else { "" },
                    if p.port.is_empty() { "—" } else { &p.port }
                );
            }
        }
    }

    // 4) 真跑一次批量。
    //    硬保护（**第二道**）：开头 2) 查过一次 `out_to_subdir`，但上面 3.7b 等
    //    用例会临时把它压成 false 去试分支；万一哪个用例忘了还原，必须在**跑之前**
    //    响亮拦下 —— 否则完整批会被 `start_run` 入口的就地确认关静默挡空
    //    （产物零文件、无运行日志无报告，却照样打印"执行完成"，回归极难定位）。
    //    宁可当红灯，也不许静默跑空。
    if !app.out_to_subdir {
        println!("  ✗ 完整批硬保护触发：out_to_subdir 被前面用例改脏（应为 true），拒绝就地跑");
        return 2;
    }
    let dry = app.do_run_selftest(true);
    if !dry {
        println!("  ✗ 预演阶段有错误：{}", app.error.clone().unwrap_or_default());
        return 1;
    }
    println!("  预演完成");
    if !app.do_run_selftest(false) {
        println!("  ✗ 执行阶段有错误：{}", app.error.clone().unwrap_or_default());
        return 1;
    }
    println!("  执行完成");

    // 5) 汇总
    match &app.result {
        Some(res) => {
            println!(
                "  结果：文件 {}（跳过 {}）｜命中 {}｜已替换 {}｜冲突 {}｜改名 {}｜验证 {}/{}",
                res.outcomes.len(),
                res.skipped.len(),
                res.total_hits(),
                res.total_applied(),
                res.total_conflicts(),
                res.name_rows.iter().filter(|n| n.changed).count(),
                res.verify_rows.iter().filter(|v| v.ok()).count(),
                res.verify_rows.len()
            );
            for v in res.verify_rows.iter().filter(|v| !v.ok()) {
                println!("    ✗ {}：{}", v.file, v.note);
            }
            if res.failed() > 0 || res.verify_bad() > 0 {
                println!("  ✗ 自检不通过");
                return 2;
            }
            println!("  ✓ 自检通过");
            0
        }
        None => {
            println!("  ✗ 没有结果");
            1
        }
    }
}

impl App {
    /// 自检用的薄封装：跑一次并把错误转成 bool。
    /// 走的是和界面按钮**同一条路**（`start_run` + 每帧 `pump_job`），
    /// 只是这里阻塞等它跑完——无头环境没有帧循环。
    fn do_run_selftest(&mut self, dry: bool) -> bool {
        self.run_blocking(dry);
        self.error.is_none()
    }
}

// ─────────────────────────── 单元测试 ───────────────────────────

/// `app.rs` 的单元测试 —— 补这块长期存在的测试真空。
///
/// ## 为什么挑的是这几处
///
/// 在此之前，全仓 83 个 `#[test]` 全在 `docx` / `hdr` / `naming` / `pipeline` /
/// `probe` / `rules` / `verify` / `picker` 八个模块里，而这个**改动最频繁**的文件
/// 一条都没有。2026-10-13 那轮出的两个 bug（自检把 `out_to_subdir` 压成 false 后
/// 忘还原、选择器位置每帧被重算）**恰恰都落在这片真空里** —— 是 `regress.sh` 兜住的，
/// 而 `regress.sh` 要真实客户语料、**CI 跑不了**。也就是说：改这里以前只有
/// "在本机手动跑一遍"这一道防线。
///
/// 所以下面补的是**最容易被改坏、坏起来后果最重**的纯逻辑：
///
/// - `options_for` —— 决定"到底动没动源文件"，出过 bug 的就是它；
/// - `drop_into_input` / `drop_into_docs` / `is_word_doc` —— 0.4.2 新写的拖放落点；
/// - `input_dir` / `product_dir` / `report_path` —— 报告与运行日志落在哪；
/// - `trim_log` —— `run_log_from` 挪错就会切片越界 panic；
/// - `build_rules` / `scope_of` —— 界面行变成内核规则的唯一出口。
///
/// 这些都是**纯状态机**，不需要窗口、不需要显卡。真正需要把图形栈跑起来的部分
/// （布局、绘制、Windows 消息循环）仍然归 `wrepl-gui.exe --selftest` 管 ——
/// 两者互补：这里跑得快、能定位到函数；自检那条路能覆盖真实的 egui 帧循环。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::wordtool::PickDoc;

    /// 造一个**唯一名字**的临时目录。
    ///
    /// 刻意不删：本机的删除拦截层（safe-delete）会让 `remove_dir_all` 要么挂起、
    /// 要么静默失败，为测试留一堆要清理的目录不值当。名字里带进程号 + 纳秒时间戳，
    /// 从根上撞不上上一轮的残留 —— 与 `regress.sh` 用 `$$` 是同一个思路。
    fn 临时目录(标签: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join(format!("wrepl-app-{标签}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 一个**保证不存在**的路径，用来测"路径在盘上不存在时会怎么解析"。
    fn 不存在的路径() -> PathBuf {
        let p = std::env::temp_dir()
            .join("wrepl-app-这里肯定没有这个目录-7c1e")
            .join("甲.docx");
        assert!(!p.exists(), "这条前提不成立就没法测（真撞上了就换个名字）");
        p
    }

    // ── 拖放：白名单 ────────────────────────────────────────────────

    /// `.doc` / `.docx` 才收，且**不看大小写**。
    ///
    /// 它与「页眉替换 / 打印与导出」那两页的选择器白名单是同一个来源，
    /// 两条路必须给出同一批候选 —— 对不上就会出现"拖进来的和选进来的不一样"。
    #[test]
    fn 白名单_只认_doc_和_docx_且不看大小写() {
        assert!(is_word_doc(Path::new("a.docx")));
        assert!(is_word_doc(Path::new("a.doc")));
        assert!(is_word_doc(Path::new("A.DOCX")));
        assert!(is_word_doc(Path::new("混合.DoCx")));
        assert!(is_word_doc(Path::new(r"E:\交付包\页眉 替换.docx")));
    }

    /// 反例：不是 Word 的一律挡掉。
    #[test]
    fn 白名单_挡掉非_word_与冒充的扩展名() {
        assert!(!is_word_doc(Path::new("a.xlsx")));
        // `.docx.bak` 的扩展名是 `bak` —— 就地替换留下的备份不该被当成文档收进来
        assert!(!is_word_doc(Path::new("a.docx.bak")));
        // 名字里带 .docx、扩展名却是别的（清单文件最容易这样）
        assert!(!is_word_doc(Path::new("清单（列出 all.docx）.txt")));
        // 老二进制格式与 WPS 一律明确拒绝，不做降级尝试
        assert!(!is_word_doc(Path::new("a.wps")));
        assert!(!is_word_doc(Path::new("没有扩展名")));
        assert!(!is_word_doc(Path::new("")));
        // 以点开头 = 隐藏文件，按 Rust 的规则**没有扩展名**
        assert!(!is_word_doc(Path::new(".docx")));
    }

    // ── 拖放：落在「输入」栏（批量替换页） ──────────────────────────

    /// 不按 Ctrl：整栏换掉（与资源管理器里"点选"的直觉一致）。
    #[test]
    fn 拖入输入栏_默认是整栏替换() {
        let mut app = App::new();
        app.input = "E:/旧的".into();
        app.drop_into_input(vec![PathBuf::from("E:/新的一"), PathBuf::from("E:/新的二")], false);
        assert_eq!(
            app.input, "E:/新的一\nE:/新的二",
            "不按 Ctrl 时原内容整个被换掉，多项之间一行一个"
        );
    }

    /// 按住 Ctrl：追加到原内容后面，中间只留一个换行。
    #[test]
    fn 拖入输入栏_按住_ctrl_是追加() {
        let mut app = App::new();
        app.input = "E:/旧的\n".into(); // 故意带一个尾随换行
        app.drop_into_input(vec![PathBuf::from("E:/新的")], true);
        assert_eq!(
            app.input, "E:/旧的\nE:/新的",
            "追加前要 trim_end，否则中间会多出一个空行"
        );
    }

    /// 栏里本来是空的 → 追加等于填进去，**前面不该多一个空行**。
    ///
    /// 这一条单拎出来是因为"空内容 + 追加"最容易写出前导换行，
    /// 而前导换行在 `input_paths` 里会被当空片段跳过，肉眼很难发现。
    #[test]
    fn 拖入输入栏_栏里是空的时候_追加等同于替换() {
        let mut app = App::new();
        assert!(app.input.is_empty(), "新开的 App 输入栏应当是空的");
        app.drop_into_input(vec![PathBuf::from("E:/新的")], true);
        assert_eq!(app.input, "E:/新的");
    }

    // ── 拖放：落在「已选文档」（另两页） ────────────────────────────

    /// 只收 Word 文档，其余跳过 —— 并说明跳过。
    #[test]
    fn 拖入文档栏_只收_word_其余跳过() {
        let mut app = App::new();
        let dir = 临时目录("docs-filter");
        let 甲 = dir.join("甲.docx");
        let 乙 = dir.join("乙.doc");
        let 丙 = dir.join("丙.txt");
        for p in [&甲, &乙, &丙] {
            std::fs::write(p, b"x").expect("造测试文件");
        }

        app.drop_into_docs(vec![甲.clone(), 乙.clone(), 丙.clone()], false, PickDoc::Header);

        let got = app.wt.docs_of(PickDoc::Header).to_vec();
        assert_eq!(got.len(), 2, "只有 .doc / .docx 该留下");
        assert!(got.contains(&甲.display().to_string()));
        assert!(got.contains(&乙.display().to_string()));
        assert!(
            !got.iter().any(|s| s.ends_with("丙.txt")),
            "txt 不该被收进「已选文档」"
        );
    }

    /// 目录**只展开一层**，不递归。
    ///
    /// 递归展开会在拖进一个交付包目录时静默吞进几百个文件 —— 人没要求那个。
    #[test]
    fn 拖入文档栏_目录只展开一层不递归() {
        let mut app = App::new();
        let root = 临时目录("docs-one-level");
        let sub = root.join("子目录");
        std::fs::create_dir_all(&sub).expect("建子目录");
        std::fs::write(root.join("顶层.docx"), b"x").expect("造顶层文件");
        std::fs::write(sub.join("深层.docx"), b"x").expect("造深层文件");

        app.drop_into_docs(vec![root.clone()], false, PickDoc::Header);

        let got = app.wt.docs_of(PickDoc::Header).to_vec();
        assert_eq!(got.len(), 1, "只展开一层：子目录里的不该被吞进来");
        assert!(got[0].ends_with("顶层.docx"), "收进来的该是顶层那个：{got:?}");
    }

    /// 一项都没收下时**不能动原有清单**。
    ///
    /// 这是"拖错了"最常见的场景（拖了个 pdf / 图片进来）：如果这里走了
    /// "先清空再填"的写法，人手一抖就把前面精挑细选的清单全清了。
    #[test]
    fn 拖入文档栏_一项都没收下时不动原有清单() {
        let mut app = App::new();
        app.wt
            .set_docs(PickDoc::Print, vec!["E:/原先挑好的.docx".to_string()]);
        let dir = 临时目录("docs-nothing-kept");
        let 图 = dir.join("图.png");
        std::fs::write(&图, b"x").expect("造测试文件");

        app.drop_into_docs(vec![图], false, PickDoc::Print);

        assert_eq!(
            app.wt.docs_of(PickDoc::Print).to_vec(),
            vec!["E:/原先挑好的.docx".to_string()],
            "拖进来的不是 Word 文档时，原有清单要原封不动"
        );
    }

    /// 按住 Ctrl：追加到清单末尾，顺序是"原有的在前"。
    #[test]
    fn 拖入文档栏_按住_ctrl_是追加到末尾() {
        let mut app = App::new();
        app.wt
            .set_docs(PickDoc::Header, vec!["E:/先选的.docx".to_string()]);
        let dir = 临时目录("docs-append");
        let 再来 = dir.join("再来一个.docx");
        std::fs::write(&再来, b"x").expect("造测试文件");

        app.drop_into_docs(vec![再来.clone()], true, PickDoc::Header);

        let got = app.wt.docs_of(PickDoc::Header).to_vec();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], "E:/先选的.docx", "原有的排在前面");
        assert_eq!(got[1], 再来.display().to_string(), "新拖进来的接在后面");
    }

    /// 落点按页隔离：拖到「页眉替换」不该影响「打印与导出」的清单。
    #[test]
    fn 拖入文档栏_两个页的清单互不串() {
        let mut app = App::new();
        let dir = 临时目录("docs-isolate");
        let d = dir.join("甲.docx");
        std::fs::write(&d, b"x").expect("造测试文件");

        app.drop_into_docs(vec![d], false, PickDoc::Header);

        assert_eq!(app.wt.docs_of(PickDoc::Header).len(), 1, "页眉页应当收到它");
        assert!(
            app.wt.docs_of(PickDoc::Print).is_empty(),
            "打印页的清单不该被牵连"
        );
    }

    // ── 输出模式：决定"到底动没动源文件" ────────────────────────────

    /// 默认落盘方式就是**就地替换源文件** —— 这是全程序后果最重的默认值。
    ///
    /// 界面上没有任何东西提示"这一下会覆盖原文件"，所以这个默认值一旦被改，
    /// 影响的是所有人第一次点「执行替换」的结果。钉死它。
    #[test]
    fn 输出模式_默认就是就地替换源文件() {
        let app = App::new();
        assert!(!app.out_to_subdir, "默认不勾「输出到子文件夹」");

        let o = app.options_for(false);
        assert!(o.in_place, "不勾就是就地覆盖源文件");
        assert!(o.out_dir.is_none(), "就地替换没有输出目录");
        assert!(!o.dry_run);
    }

    /// 勾了「输出到子文件夹」→ 写副本，源文件不动。
    #[test]
    fn 输出模式_勾了子文件夹就写副本() {
        let mut app = App::new();
        app.out_to_subdir = true;
        app.output = "E:/产物".into();
        app.mirror = true;
        app.backup = true;

        let o = app.options_for(false);
        assert!(!o.in_place, "写副本绝不动源文件");
        assert_eq!(o.out_dir, Some(PathBuf::from("E:/产物")));
        assert!(o.mirror, "镜像只在写副本时才有意义");
        assert!(!o.backup, "备份只跟就地替换有关，写副本时不该生效");
    }

    /// 预演：什么都不许落盘 —— 备份 / 镜像 / 改名 / 执行后验证一并失效。
    ///
    /// 这一条是 `F5`（扫描预演）的地基。任何一项漏掉，按 F5 就不再是"看一眼"。
    #[test]
    fn 输出模式_预演一律不落盘() {
        let mut app = App::new();
        app.out_to_subdir = true;
        app.output = "E:/产物".into();
        app.mirror = true;
        app.backup = true;
        app.rename_files = true;

        let o = app.options_for(true);
        assert!(o.dry_run);
        assert!(!o.in_place, "预演不是就地——它压根不落盘");
        assert!(o.out_dir.is_none(), "预演不许指定输出目录");
        assert!(!o.backup, "预演不落盘，没有备份可言");
        assert!(!o.mirror, "预演不落盘，谈不上镜像");
        assert!(!o.rename_files, "预演不改名");
        assert!(!o.verify_after, "预演没写文件，没什么可验证的");
    }

    /// 备份的三个条件缺一不可：勾了 + 非预演 + 就地。
    #[test]
    fn 输出模式_备份只在就地且非预演时生效() {
        let mut app = App::new();
        app.backup = true;
        assert!(app.options_for(false).backup, "就地替换 + 执行 → 该留 .bak");

        app.out_to_subdir = true;
        app.output = "E:/产物".into();
        assert!(
            !app.options_for(false).backup,
            "写副本本来就不动源文件，没有备份的概念"
        );

        app.out_to_subdir = false;
        assert!(!app.options_for(true).backup, "预演不落盘");
    }

    /// 链式替换下不存在区间重叠，所以"最长匹配优先"自动失效。
    ///
    /// 这两个开关在界面上是互斥的（勾了链式会把另一个清掉），但内核侧
    /// 必须**自己再判一次** —— 否则规则表来回倒腾时会出现"两个都开着"的状态。
    #[test]
    fn 输出模式_链式替换压制最长匹配() {
        let mut app = App::new();
        app.longest_first = true;
        app.chain = false;
        assert!(app.options_for(false).longest_first, "不链式时最长匹配生效");

        app.chain = true;
        assert!(
            !app.options_for(false).longest_first,
            "链式下规则先后作用、不存在重叠，裁决方式无从谈起"
        );
    }

    /// 排除串认**四种**分隔符，且两头的空白要吃掉、空片段要丢掉。
    #[test]
    fn 输出模式_排除串认四种分隔符并去掉空白() {
        let mut app = App::new();
        app.exclude = " ~$*.docx ; 临时 ，备份,  ".into();

        let o = app.options_for(false);
        assert_eq!(
            o.exclude,
            vec![
                "~$*.docx".to_string(),
                "临时".to_string(),
                "备份".to_string()
            ],
            "分号 / 中文分号 / 逗号 / 顿号都要认，两头的空白要 trim，空的不要"
        );
    }

    /// 界面**不做**并行度选项：内核永远拿到 `0`（=按本机核数自动决定）。
    #[test]
    fn 输出模式_线程数交给内核自动决定() {
        let app = App::new();
        assert_eq!(
            app.options_for(false).threads,
            0,
            "0 = 自动；界面上不该出现「开几个线程」这种要人猜的开关"
        );
    }

    // ── 输入栏解析与落点计算 ───────────────────────────────────────

    /// 输入栏的文本要能按多种分隔符拆开、去重、保序、trim。
    #[test]
    fn 输入栏_多种分隔符都会拆并去重保序() {
        let mut app = App::new();
        app.input = "E:/甲\nE:/乙;E:/丙；E:/甲\r\n  E:/丁  ".into();

        // 保留粘贴顺序（报告里"输入"那一栏要保持原样），但重复的只留第一次。
        let got: Vec<String> = app
            .input_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(got, vec!["E:/甲", "E:/乙", "E:/丙", "E:/丁"]);
    }

    /// 空输入 → 空列表（不是"一个空路径"）。
    #[test]
    fn 输入栏_空白输入不产生路径() {
        let mut app = App::new();
        app.input = "   \n  \n".into();
        assert!(app.input_paths().is_empty(), "全是空白的输入不该被当成一个路径");
    }

    /// `input_dir` 取**第一个**输入的家：是文件就取它的父目录。
    #[test]
    fn 输入目录_文件取父目录() {
        let mut app = App::new();
        let ghost = 不存在的路径();
        app.input = ghost.display().to_string();

        let want = ghost.parent().expect("这个路径有父目录").to_path_buf();
        assert_eq!(app.input_dir(), want);
    }

    /// 是目录就取它本身 —— 用系统临时目录当真目录，避免依赖本机别处的路径。
    #[test]
    fn 输入目录_目录取它本身() {
        let mut app = App::new();
        let real = std::env::temp_dir();
        assert!(real.is_dir(), "系统临时目录应当是存在的");

        app.input = real.display().to_string();
        assert_eq!(app.input_dir(), PathBuf::from(real.display().to_string()));
    }

    /// 输入为空 → 空路径（后续据此回退，而不是拼出一个 `\替换报告.xlsx`）。
    #[test]
    fn 输入目录_空输入返回空路径() {
        let app = App::new();
        assert_eq!(app.input_dir(), PathBuf::new());
    }

    /// 产物与报告的落点跟着输出模式走，三种情形都要对。
    #[test]
    fn 落点_产物与报告跟着输出模式走() {
        let mut app = App::new();
        let ghost = 不存在的路径();
        let 它家 = ghost.parent().expect("有父目录").to_path_buf();
        app.input = ghost.display().to_string();

        // ① 就地替换 → 落在输入目录（跟产物挨着，回头好找）
        assert_eq!(app.product_dir(), 它家);
        assert_eq!(app.report_path(), 它家.join("替换报告.xlsx"));

        // ② 写副本 → 落在输出目录
        app.out_to_subdir = true;
        app.output = "E:/产物".into();
        assert_eq!(app.product_dir(), PathBuf::from("E:/产物"));
        assert_eq!(app.report_path(), PathBuf::from("E:/产物").join("替换报告.xlsx"));

        // ③ 勾了写副本但输出栏是空白 → 退回输入目录，
        //    **不能**把产物落到一个空路径上（那样报告就丢在当前工作目录了）
        app.output = "   ".into();
        assert_eq!(app.product_dir(), 它家, "输出栏空白时要退回输入目录");
    }

    /// 连输入都没有时，报告退回**裸文件名**（落在当前目录），不拼出 `\替换报告.xlsx`。
    #[test]
    fn 落点_什么都没有时报告退回裸文件名() {
        let app = App::new();
        assert_eq!(app.report_path(), PathBuf::from("替换报告.xlsx"));
    }

    // ── 日志容量 ───────────────────────────────────────────────────

    /// 超上限丢最老的，同时把"本次执行起点"一起往前挪。
    ///
    /// 这一条要是漏了，`log[run_log_from..]` 就会切片越界 **panic** ——
    /// 而它只在"日志攒满 400 行"这种长跑里才触发，手工点几下永远碰不到。
    #[test]
    fn 日志裁剪_超上限丢最老的行并同步前移起点() {
        let mut app = App::new();
        app.log = (0..10).map(|i| format!("第{i}行")).collect();
        app.run_log_from = 8;

        app.trim_log(5);

        assert_eq!(app.log.len(), 5, "超上限就裁到只剩上限那么多");
        assert_eq!(app.log[0], "第5行", "丢的是最老的那几行");
        assert_eq!(app.run_log_from, 3, "本次执行的起点要同步往前挪 5 行");
        assert!(
            app.run_log_from <= app.log.len(),
            "起点绝不能跑到日志长度之外，否则切片会 panic"
        );
    }

    /// 起点比裁掉的还靠前 → 饱和到 0，不许下溢。
    #[test]
    fn 日志裁剪_起点比裁掉的还靠前时饱和到零() {
        let mut app = App::new();
        app.log = (0..10).map(|i| format!("第{i}行")).collect();
        app.run_log_from = 1;

        app.trim_log(2);

        assert_eq!(app.run_log_from, 0, "减不动就停在 0，不能下溢成天文数字");
        assert_eq!(app.log.len(), 2);
    }

    /// 没超上限时一动不动。
    #[test]
    fn 日志裁剪_没超上限时不动() {
        let mut app = App::new();
        app.log = vec!["只有一行".to_string()];
        app.run_log_from = 0;

        app.trim_log(400);

        assert_eq!(app.log, vec!["只有一行".to_string()]);
        assert_eq!(app.run_log_from, 0);
    }

    /// 追加日志时顺手裁剪，且空闲上限是 400 行。
    #[test]
    fn 追加日志_按空闲上限裁剪() {
        let mut app = App::new();
        app.log.clear();

        app.extend_log((0..500).map(|i| format!("行{i}")).collect());

        assert_eq!(app.log.len(), 400, "空闲时只留显示用的那 400 行");
        assert_eq!(app.log[0], "行100", "丢的是最老的 100 行");
    }

    // ── 规则构建：界面行 → 内核规则 ────────────────────────────────

    /// 没填「查找内容」的行不装进规则表。
    #[test]
    fn 规则_跳过没填查找内容的行() {
        let mut app = App::new();
        app.rows = vec![
            Row::new("甲", "乙", "全部"),
            Row::new("", "丙", "全部"), // 空查找内容：半截规则，不装
            Row::new("丁", "戊", "全部"),
        ];

        let rules = app.build_rules().expect("应当构得出规则");
        assert_eq!(rules.len(), 2, "没填查找内容的那行要跳过");
        assert_eq!(rules[0].find, "甲");
        assert_eq!(rules[1].find, "丁");
        assert_eq!(rules[0].id, 1);
        assert_eq!(
            rules[1].id, 3,
            "编号沿用界面行号（报告要能回溯到原处），跳过的行不重新编号"
        );
    }

    /// 「同步替换文件名」没勾时，作用域只写了「文件名」的规则**没有可用作用域**。
    ///
    /// 必须报错而不是静默跳过：人写了这条规则就以为它会生效，
    /// 悄悄不跑等于埋雷（这条错误信息还欠一次实际发生的验证）。
    #[test]
    fn 规则_没勾同步改名时纯文件名作用域要报错() {
        let mut app = App::new();
        app.rename_files = false;
        app.rows = vec![Row::new("甲", "乙", "文件名")];

        let err = app
            .build_rules()
            .expect_err("纯文件名作用域 + 没勾改名 = 没有可用作用域");
        assert!(err.contains("没有可用作用域"), "错误要说清原因：{err}");
        assert!(err.contains("第 1 条"), "错误要指出是第几条：{err}");

        // 勾上改名之后同一个作用域就成立了
        app.rename_files = true;
        assert!(app.build_rules().is_ok(), "勾了「同步替换文件名」就该通过");
    }

    /// 作用域串写错时要报错，并指出是哪一条、错在哪。
    #[test]
    fn 规则_作用域写错要报错并指出行号() {
        let mut app = App::new();
        app.rows = vec![Row::new("甲", "乙", "正文,不存在的维度")];

        let err = app.build_rules().expect_err("未知作用域应当报错");
        assert!(err.contains("第 1 条"), "要指出是哪一条：{err}");
        assert!(err.contains("不存在的维度"), "要把写错的那个词带出来：{err}");
    }

    /// 一条规则都没有 → 报错，不允许空跑。
    #[test]
    fn 规则_一条都没有时要报错() {
        let mut app = App::new();
        assert!(app.build_rules().is_err(), "空白规则表不该能开跑");

        app.rows = vec![Row::new("", "", "全部")];
        assert!(app.build_rules().is_err(), "全是空行也一样");
    }

    /// 「全部」要展开成所有维度，并且**文件名那一维由开关说了算**。
    #[test]
    fn 规则_全部作用域展开且文件名维度跟开关走() {
        let mut app = App::new();
        app.rows = vec![Row::new("甲", "乙", "全部")];
        app.rename_files = true;

        let rules = app.build_rules().expect("「全部」应当能解析");
        let sc = rules[0].scope;
        assert!(sc.body && sc.header_footer && sc.textbox && sc.footnote && sc.comment);
        assert!(sc.filename, "勾了同步改名，文件名那一维应当打开");

        app.rename_files = false;
        let rules = app.build_rules().expect("关掉改名后「全部」仍然有效");
        assert!(
            !rules[0].scope.filename,
            "没勾同步改名时，规则文件里写没写「文件名」都不该动文件名"
        );
        assert!(rules[0].scope.body, "其余维度不受影响");
    }

    // ── 预置模式（`--preset` / `--selftest` 共用） ──────────────────

    /// 给了输出目录 → 自动走"写副本"。
    ///
    /// 这是 `--selftest` **永远不可能动到源语料**的根本原因：预置路径一律
    /// 落在写副本上，跟"就地替换"那个默认值无关。
    #[test]
    fn 预置_给了输出目录就走写副本() {
        let p = Preset {
            input: "E:/输入".into(),
            output: "E:/产物".into(),
            ..Default::default()
        };

        let app = App::with_preset(&p);

        assert!(app.out_to_subdir, "给了输出目录就该走写副本");
        assert!(
            !app.rename_files,
            "预置模式默认**不改**文件名（与界面默认相反）——拿现成语料直接跑，改名会连语料名一起改掉"
        );
        assert_eq!(
            app.options_for(false).out_dir,
            Some(PathBuf::from("E:/产物"))
        );
    }

    /// 不给输出目录 → 就地替换。
    #[test]
    fn 预置_没有输出目录时走就地替换() {
        let p = Preset {
            input: "E:/输入".into(),
            ..Default::default()
        };

        let app = App::with_preset(&p);

        assert!(!app.out_to_subdir);
        assert!(app.options_for(false).in_place);
    }

    /// 显式 `--in-place` 优先于输出目录。
    #[test]
    fn 预置_显式就地优先于输出目录() {
        let p = Preset {
            input: "E:/输入".into(),
            output: "E:/产物".into(),
            in_place: true,
            ..Default::default()
        };

        let app = App::with_preset(&p);

        assert!(!app.out_to_subdir, "--in-place 要压过输出目录");
        assert!(app.options_for(false).in_place);
    }

    // ── 对外的结论文案 ─────────────────────────────────────────────

    /// 五档结论里最容易写错的两档：预演 vs 执行。
    ///
    /// 预演必须**明说没写任何文件** —— 否则人看完这句话没法判断磁盘动没动。
    #[test]
    fn 结论文案_预演要说清没写文件() {
        let empty = RunResult::default();

        let dry = verdict_text(&empty, true);
        assert!(dry.starts_with("预演完成"), "预演有自己的一套说法：{dry}");
        assert!(dry.contains("未写入任何文件"), "必须讲清磁盘没动：{dry}");

        let real = verdict_text(&empty, false);
        assert!(real.starts_with("合格"), "没有失败 / 验证不过 / 冲突就算合格：{real}");
        assert!(real.contains("0/0"), "空结果的验证计数是 0/0：{real}");
        assert!(
            !real.contains("未写入"),
            "真跑过的结论不该说「未写入」：{real}"
        );
    }

    // ── 无窗口帧循环（冒烟） ───────────────────────────────────────

    /// 一个**装好设计系统**的 `Context`。
    ///
    /// 渲染测试必须先装：界面代码用的是设计系统那套字号层级
    /// （`TextStyle::Name("h3")` 之类），而 egui 默认样式表里只有
    /// `Small` / `Body` / `Monospace` / `Button` / `Heading` 五档 ——
    /// 不装就当场 panic：
    ///
    /// ```text
    /// Failed to find Name("h3") in Style::text_styles.
    /// ```
    ///
    /// 顺带钉住一条隐性约定：**界面渲染不能早于 `ds::install`**。
    /// `main.rs` 里正是这个顺序，这里照做。
    fn 装了设计系统的_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        ds::install(&ctx);
        ctx
    }

    /// 跑若干帧不 panic —— 这是 `app.rs` 第一次被真的渲染一遍。
    ///
    /// egui 的 `Context` 不需要 GPU（字体光栅 + 布局都是纯 CPU），
    /// 所以这一条能在测试里跑；真正需要显卡与 OS 消息循环的部分仍归 `--selftest`。
    /// 它不替代自检，只是把"改完顺手 `cargo test` 就能发现 panic"这条线补上。
    #[test]
    fn 无窗口跑若干帧不panic() {
        let ctx = 装了设计系统的_ctx();
        let mut app = App::new();
        // 造点内容进去，免得只渲染一个空壳
        app.input = "E:/不存在的输入目录-9f3a".into();
        app.rows = vec![Row::new("设计情况", "已改造情况", "全部")];

        for _ in 0..3 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| app.render(ui));
        }
    }

    /// `Ctrl+O` 的落点跟着当前页走 —— 三种页各开各的选择器。
    ///
    /// 合成按键是**唯一**能自动验证这件事的办法：本机的合成鼠标事件进不了窗口
    /// （见 `diag` 模块头），键盘同理，只能在测试里自己造一帧输入。
    #[test]
    fn 快捷键_ctrl_o_的落点跟着当前页走() {
        // 造一帧"按下了 Ctrl+O"的输入。
        let 按下_ctrl_o = |app: &mut App| {
            let ctx = 装了设计系统的_ctx();
            let mut input = egui::RawInput::default();
            input.modifiers = egui::Modifiers::CTRL;
            input.events.push(egui::Event::Key {
                key: egui::Key::O,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::CTRL,
            });
            let _ = ctx.run_ui(input, |ui| app.render(ui));
        };

        let mut app = App::new();

        app.page = Page::Replace;
        app.picker_for = None;
        按下_ctrl_o(&mut app);
        assert!(
            matches!(app.picker_for, Some(PickFor::InputDir)),
            "批量替换页的起手动作是「选输入目录」"
        );

        app.page = Page::Header;
        app.picker.close();
        app.picker_for = None;
        按下_ctrl_o(&mut app);
        assert!(
            matches!(app.picker_for, Some(PickFor::HeaderDocs)),
            "页眉替换页没有「目录」这个概念，起手动作是「选文档」"
        );

        app.page = Page::Print;
        app.picker.close();
        app.picker_for = None;
        按下_ctrl_o(&mut app);
        assert!(
            matches!(app.picker_for, Some(PickFor::PrintDocs)),
            "打印与导出页同理"
        );
    }
}
