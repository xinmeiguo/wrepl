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

use eframe::egui::{self, Color32, RichText};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use wrepl::pipeline::{self, Options, RunResult};
use wrepl::rules::{self, Rule, Scope};
use wrepl::{naming, report};

use crate::picker::{Mode, Outcome, Picker};
use crate::wordtool::HeaderMode;

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
    /// 「批量打印」页的「选择文档（可多选）」
    PrintDocs,
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
    /// 改名预览的缓存：`(输入+规则指纹, 预览行)`。逐帧重算会一直扫目录，
    /// 目录一大界面就"拖拉"，所以只在输入或规则真的变了时才重算。
    rename_preview: Option<(String, Vec<(String, Color32)>)>,

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
    /// 当前页：正文替换（原有）/ 页眉替换 / 批量打印。
    page: Page,
    /// 页眉替换与批量打印的状态 + 任务（走 Word COM）。
    wt: crate::wordtool::WordTool,
}

/// 顶部的三个页。
///
/// 「正文替换」是工具原本的能力，与另两页**没有共同依赖**——它纯 Rust 改 OOXML，
/// 不需要 Word；「页眉替换 / 批量打印」走 Word COM，**需要目标机器装 Word**。
#[derive(PartialEq, Clone, Copy)]
enum Page {
    /// 正文批量替换（原有）
    Replace,
    /// 页眉首单元格替换（文本 / 图片）
    Header,
    /// 批量打印
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
            rename_preview: None,
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
        // 诊断开关：`WREPL_DEBUG_PICKER=dir|file|out|save` 时启动就把选择器
        // 打开，再配合 `WREPL_DEBUG_CLICK` 就能无人值守地复现"点一下"。
        // 本机的合成鼠标输入进不来窗口（见 `diag` 里的说明），只能让程序自己做。
        // 不设这个环境变量时，下面整段是空转。
        match crate::diag::debug_picker_mode().as_deref() {
            Some("dir") => app.open_picker(PickFor::InputDir),
            Some("file") => app.open_picker(PickFor::InputFiles),
            Some("out") => app.open_picker(PickFor::OutputDir),
            Some("save") => app.open_picker(PickFor::ExportXlsx),
            // 另两页的「选择文档」——同一个面板、同一个入口分派，只是结果
            // 落到 `WordTool` 那边（见 `apply_picked`）。
            Some("hdrdocs") => app.open_picker(PickFor::HeaderDocs),
            Some("printdocs") => app.open_picker(PickFor::PrintDocs),
            Some(other) => {
                crate::diag::log(format!("[诊断] 未知的 WREPL_DEBUG_PICKER={other:?}，忽略"))
            }
            None => {}
        }
        // 同款诊断开关：`WREPL_DEBUG_PAGE=replace|header|header-image|print` 启动即落在那页。
        // 加了「页眉替换 / 批量打印」两页之后，截图与演示都需要让窗口一开就停在
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

    // ─────────────────────── 文件名改名预览 ───────────────────────

    /// 预览缓存键：输入路径 + 链式开关 + 整张规则表。
    /// 键构造只碰内存，不扫目录，所以真正贵的 I/O 只在必要时发生。
    fn rename_preview_key(&self) -> String {
        let mut s = String::with_capacity(64);
        s.push_str(self.input.trim());
        s.push('\u{1}');
        s.push_str(if self.chain { "1" } else { "0" });
        for r in &self.rows {
            s.push('\u{2}');
            s.push_str(&r.find);
            s.push('\u{3}');
            s.push_str(&r.replace);
        }
        s
    }

    /// 真正干活的那一段：扫一遍目录，对前 8 个文件试算新名。
    fn compute_rename_preview(&self) -> Vec<(String, Color32)> {
        let mut out: Vec<(String, Color32)> = Vec::new();
        let rule_list = self.build_rules().unwrap_or_default();
        if rule_list.is_empty() || self.input.trim().is_empty() {
            return out;
        }
        let opts = self.options_for(true);
        let Ok((files, _)) = pipeline::collect_targets(&self.input_paths(), &opts) else {
            return out;
        };
        for (i, (src, _)) in files.iter().take(8).enumerate() {
            let name = src
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            match naming::rename_file_name(&name, &rule_list, self.chain) {
                Ok(p) if p.changed() => out.push((
                    format!("{}. {} → {}", i + 1, p.old_name, p.new_name),
                    OK_C,
                )),
                Ok(_) => out.push((format!("{}. {}（不改）", i + 1, name), DIM_C)),
                Err(e) => out.push((format!("{}. 改名失败：{e:#}", i + 1), ERR_C)),
            }
        }
        out
    }

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

    fn open_path(&mut self, p: &str) {
        let p = p.trim();
        if p.is_empty() {
            return;
        }
        let target = if Path::new(p).is_dir() {
            p.to_string()
        } else {
            Path::new(p)
                .parent()
                .map(|x| x.display().to_string())
                .unwrap_or_else(|| p.to_string())
        };
        if let Err(e) = std::process::Command::new("explorer").arg(&target).spawn() {
            self.error = Some(format!("打不开：{e}"));
        }
    }

    // ─────────────────────── 界面 ───────────────────────

    pub fn render(&mut self, ctx: &egui::Context) {
        // ① 先把后台线程攒下的进度收进来（不阻塞），这一帧就能画出来
        self.pump_job();
        // 新两页的任务状态在 `wt` 里，也每帧收一次
        self.wt.poll();
        // ② 有活干的时候定时重绘：串行阶段（预分配输出路径、改名）没有进度消息，
        //    不主动叫醒的话进度条会停住不动。200ms 一次，人眼看是连续的。
        if self.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // 主题只在变化时应用一次（每帧都 set_visuals 会把用户的临时样式冲掉）
        if self.applied_light != Some(self.light_theme) {
            ctx.set_visuals(visuals_of(self.light_theme));
            self.applied_light = Some(self.light_theme);
        }

        egui::TopBottomPanel::top("title").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("wrepl")
                        .size(ui_scale::TITLE)
                        .strong()
                        .color(Color32::from_rgb(0x1F, 0x4E, 0x79)),
                );
                ui.label(RichText::new("Word 工具集").color(DIM_C));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.light_theme, "浅色");
                });
            });
            ui.add_space(4.0);
            // 三页切换。任务在跑时也允许切——任务状态挂在 `job` / `wt` 上，
            // 切页不会中断它（`render` 每帧都会 poll）。
            ui.horizontal(|ui| {
                for (m, label) in [
                    (Page::Replace, "正文替换"),
                    (Page::Header, "页眉替换"),
                    (Page::Print, "批量打印"),
                ] {
                    let selected = self.page == m;
                    let text = RichText::new(label)
                        .size(ui_scale::SECTION)
                        .color(if selected {
                            Color32::from_rgb(0x1F, 0x4E, 0x79)
                        } else {
                            DIM_C
                        });
                    let text = if selected { text.strong() } else { text };
                    if ui.selectable_label(selected, text).clicked() {
                        self.page = m;
                    }
                }
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
            // 这一栏是「正文替换」专用的（执行按钮 / 打开目录 / 报告 / 清日志）。
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
                if ui
                    .add_enabled(
                        can_run,
                        egui::Button::new(RichText::new(label).size(ui_scale::BUTTON).strong()),
                    )
                    .on_hover_text(if self.out_to_subdir {
                        "把产物写到输出目录，源文件不动"
                    } else {
                        "就地覆盖源文件（不可撤销）——点击后会先弹一次确认"
                    })
                    .clicked()
                {
                    self.start_run(false, ctx);
                }
                ui.separator();
                let (open_lbl, open_p) = if self.out_to_subdir {
                    ("打开输出目录", self.output.clone())
                } else {
                    ("打开源目录", self.input_dir().display().to_string())
                };
                if ui.button(open_lbl).clicked() {
                    self.open_path(&open_p);
                }
                if ui.button("打开报告").clicked() {
                    let p = self.report_path().display().to_string();
                    self.open_path(&p);
                }
                ui.separator();
                // 执行期间不给清：那一整段日志等一下要落盘成「运行日志.txt」
                if ui
                    .add_enabled(!running, egui::Button::new("清空日志"))
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
        // 结果是撞上 egui 0.29 的一个坑：`TopBottomPanel` 存进 state 的是**内容矩形**
        // 的高度（不是面板矩形），下一帧又把它当面板高度读回来；而日志里那个
        // `auto_shrink([false, false])` 的 ScrollArea 会把"可用高度"整块吃掉 ——
        // 于是内容矩形每帧比面板高一截，形成正反馈。实测**强制重绘 5 帧，面板就从
        // 168 涨到填满整个窗口**，「运行日志」的标题直接顶到标题栏下面。
        // 教训：底部面板里不要放"吃满可用高度"的东西；固定高度也压不住（会变成
        // 一条死横条），不如回到原位置 —— 日志跟着页面滚，最简单也最稳。
        egui::CentralPanel::default().show(ctx, |ui| {
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
        self.confirm_inplace_dialog(ctx);

        // 「页眉替换 / 批量打印」两页的「选择文档」按钮**不自己弹对话框**：
        // 选择器是常驻的、全程序只有一台，所以由这里统一收口 —— 界面上那些
        // 按钮只把意图抛出来（`WordTool::take_pick`），开还是这儿开。
        // 收在这里还有个好处：此刻已出了 `CentralPanel` 的借用期，够得着 `self.picker`。
        if let Some(who) = self.wt.take_pick() {
            self.open_picker(match who {
                crate::wordtool::PickDoc::Header => PickFor::HeaderDocs,
                crate::wordtool::PickDoc::Print => PickFor::PrintDocs,
            });
        }

        // 自绘文件选择器比确认框还要靠前（`Foreground` 面板 + 全屏遮罩），
        // 所以放在最后。它自带遮罩，两者不会同时被点到。
        self.handle_picker(ctx);
    }

    /// 「就地覆盖」确认框。
    ///
    /// 只做一件事：在真的动源文件之前停一下。就地替换把源文件**直接覆盖、不可撤销**，
    /// 而触发它只是界面上的一个单击——所以这里**只说清"改源文件、不可撤销"**，
    /// 再给个当场勾备份的口子，问一句就走（文字从简）。
    ///
    /// egui 0.29 没有 `Modal`，用两层 `Area` 模拟：底层遮罩铺满屏幕并吞掉点击
    /// （不然确认框开着的时候，人还能点到后面的「执行替换」和输入框），
    /// 上层放对话框本体。两层用**不同**的 `Order` 定序，不依赖同层内的绘制顺序。
    fn confirm_inplace_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_inplace {
            return;
        }
        let mut go = false;
        let mut cancel = false;

        // ① 遮罩：铺满整屏、吞掉所有点击。`Order::Middle` 高于页面所在的
        //    `Background`、低于对话框的 `Foreground`，正好夹在中间。
        egui::Area::new(egui::Id::new("wrepl-confirm-veil"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .interactable(true)
            .show(ctx, |ui| {
                let full = ctx.screen_rect();
                ui.allocate_response(full.size(), egui::Sense::click_and_drag());
                ui.painter()
                    .rect_filled(full, 0.0, Color32::from_black_alpha(70));
            });

        // ② 对话框本体：居中，浮在遮罩之上。**文字从简**。
        egui::Area::new(egui::Id::new("wrepl-confirm-dialog"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(340.0);
                    ui.label(
                        RichText::new("将在源文件上直接替换，此操作不可撤销。")
                            .strong()
                            .color(ERR_C),
                    );
                    // 没备份时给一条退路：勾一下就行，不必取消回主界面。
                    if !self.backup {
                        ui.add_space(6.0);
                        ui.checkbox(&mut self.backup, "替换前先保留 .bak 备份").on_hover_text(
                            "每个文件改写前先留一份 .docx.bak（只首次生成，重复跑不会覆盖最初那版）",
                        );
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        // 按钮文字**不用红色**：上面那句警告已经是红的，红色只用来
                        // 说"有风险"，不该同时用来标"按钮"。按钮用正文色
                        // （浅色主题下即黑色）——强调靠加粗，不靠颜色。
                        // 取 `text_color()` 而不是写死黑色：换深色主题时不会变成黑底黑字。
                        let btn_fg = ui.visuals().text_color();
                        if ui
                            .add(egui::Button::new(
                                RichText::new("确认替换").strong().color(btn_fg),
                            ))
                            .clicked()
                        {
                            go = true;
                        }
                        // 「确认替换」是放行破坏性操作的按钮，「取消」紧挨着它右侧。
                        // 用默认的 8px 间距时两个按钮几乎贴在一起，鼠标偏一格就点错——
                        // 而这两个动作的结果正好相反（真改文件 / 什么都不做）。
                        // 这里额外拉开一段，别让相反的动作挤在同一片区域里。
                        ui.add_space(12.0);
                        if ui.button("取消").clicked() {
                            cancel = true;
                        }
                    });
                });
            });

        if go {
            // 先关框、再置"放行票"，否则下一帧还会画这个确认框。
            self.confirm_inplace = false;
            self.inplace_confirmed = true;
            self.start_run(false, ctx);
        } else if cancel {
            self.confirm_inplace = false;
        }
    }

    fn section_io(&mut self, ui: &mut egui::Ui) {
        section(ui, "文件与输出", |ui| {
            const L: f32 = 52.0; // 标签列宽
            const F: f32 = 560.0; // 路径输入框宽

            let mut pick_in_dir = false;
            let mut pick_in_file = false;
            let mut pick_out_dir = false;
            let mut use_sibling_out = false;

            ui.horizontal_top(|ui| {
                cell_label(ui, L, "输入");
                // 多行：一行一个路径。只给一个目录 / 一个文件时就是一行高。
                // 高度封顶 6 行——一次选了几十个文件时不再往长里撑，框内自己滚。
                let rows = self.input.lines().count().clamp(1, 6) as f32;
                ui.add_sized(
                    [F, ui_scale::ROW_H * rows],
                    egui::TextEdit::multiline(&mut self.input)
                        .hint_text("目录，或若干 .docx（一行一个）"),
                );
                ui.vertical(|ui| {
                    if ui.button("选目录…").clicked() {
                        pick_in_dir = true;
                    }
                    if ui
                        .button("选文件…")
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
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                cell_label(ui, L, "输出");
                ui.checkbox(&mut self.out_to_subdir, "输出到子文件夹");
                ui.add_space(8.0);
                // 镜像只对写副本有意义：就地替换本就在原地，没有"要不要复制"这回事。
                // 置灰而不是隐藏——让人看到这个开关存在，切换落盘方式后它就在那儿。
                let writes_copy = self.out_to_subdir;
                ui.add_enabled_ui(writes_copy, |ui| {
                    ui.checkbox(&mut self.mirror, "完整镜像").on_hover_text(
                        "默认不勾：输出目录里只有本次真正改过的文件。\n\
                         勾上则未改动的文件也原样复制过去（逐字节相同，文件名一并归一），\n\
                         输出目录成为输入目录的**完整镜像**，可直接当交付包拿走。\n\
                         只对「输出到子文件夹」有效。",
                    );
                });
            });
            if self.out_to_subdir {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // 缩进对齐上面那个路径框：标签宽 + 一个控件间距
                    ui.add_space(L + ui.spacing().item_spacing.x);
                    path_edit(ui, F - L, &mut self.output, "");
                    if ui.button("选目录…").clicked() {
                        pick_out_dir = true;
                    }
                    if ui
                        .button("源文件目录")
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
            ui.add_space(8.0);

            // 「报告」不再是一行输入框：它跟着产物走（写副本＝输出目录，
            // 就地替换＝源目录），运行日志也落在同一处。
            ui.horizontal(|ui| {
                cell_label(ui, L, "选项");
                ui.checkbox(&mut self.recursive, "递归子目录");
                ui.add_space(8.0);
                // 备份勾选只对「就地替换」有效，写副本时置灰——不给一个不影响结果的选择
                ui.add_enabled_ui(!self.out_to_subdir, |ui| {
                    ui.checkbox(&mut self.backup, "保留 .bak 备份").on_hover_text(
                        "默认不留：就地替换后源目录里不会多出任何文件。\n\
                         勾上则每个文件先留一份 .docx.bak 再改写（只首次生成，\n\
                         重复跑不会覆盖最初那版）。写副本时源文件本来就不动，无需备份。",
                    );
                });
                ui.add_space(8.0);
                ui.label("排除");
                ui.add_sized(
                    [180.0, ui_scale::EDIT_H],
                    egui::TextEdit::singleline(&mut self.exclude).hint_text("*_bak*"),
                );
            });

            // 选路径的动作放在布局之后执行：避免在借用 self 的闭包里嵌套借用。
            // 三个入口都走**同一个自绘面板**，靠 `PickFor` 区分结果落到哪个字段。
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
                    .button("＋ 添加规则")
                    .on_hover_text("手工增加一条规则")
                    .clicked()
                {
                    self.rows.push(Row::new("", "", "全部"));
                }
                ui.add_space(12.0);
                if ui
                    .button("从文本导入…")
                    .on_hover_text("读 .txt 规则文件，整表替换当前规则")
                    .clicked()
                {
                    import_txt = true;
                }
                ui.add_space(8.0);
                if ui
                    .button("从 Excel 导入…")
                    .on_hover_text("读 .xlsx 的「规则」工作表，整表替换当前规则")
                    .clicked()
                {
                    import_xlsx = true;
                }
                ui.add_space(8.0);
                if ui
                    .button("导出为 Excel…")
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
                            .add(egui::Button::new(
                                RichText::new("确认清空").color(ERR_C).strong(),
                            ))
                            .clicked()
                        {
                            let n = self.rows.len();
                            self.rows.clear();
                            self.last_rule_hits.clear();
                            self.rename_preview = None;
                            self.clear_armed = false;
                            self.push_log(format!("已清空 {n} 条规则"));
                            self.toast = Some((format!("已清空 {n} 条规则"), true));
                        }
                        if ui.button("取消").clicked() {
                            self.clear_armed = false;
                        }
                        ui.label(
                            RichText::new(format!("清空 {} 条规则？", self.rows.len()))
                                .color(WARN_C),
                        );
                    } else if ui
                        .add_enabled(!self.rows.is_empty(), egui::Button::new("清空规则"))
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
                if ui.checkbox(&mut self.chain, "链式替换").changed() && self.chain {
                    // 链式下规则先后作用，不存在区间重叠，最长匹配优先无事可做。
                    // 命令行遇到这个组合会直接报错，界面上就别让它出现。
                    self.longest_first = false;
                }
                ui.add_space(20.0);
                ui.add_enabled_ui(!self.chain, |ui| {
                    ui.checkbox(&mut self.longest_first, "最长匹配优先").on_hover_text(
                        "规则命中区间重叠时，只让「查找内容更长」的那条生效，\n\
                         被挤掉的记进报告（谁让给谁、多少处）。\n\
                         不勾则两条都不改，只报冲突——原来的行为。",
                    );
                });
                ui.add_space(20.0);
                ui.checkbox(&mut self.rename_files, "同步替换文件名");
            });
            ui.add_space(4.0);

            // Excel 规则表的列：勾哪些就认哪些（读和写都按它）。
            // 「启用」不在这里——界面只放会执行的规则，导出也就不写那一列。
            // 收进折叠面板：只在读写 Excel 时才要看，摊开占一整行、小字又密。
            egui::CollapsingHeader::new(RichText::new("Excel 列").small().color(DIM_C))
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for h in rules::XLSX_UI_HEADERS {
                            let mut on = self.xlsx_cols.is_on(h);
                            if ui.checkbox(&mut on, RichText::new(h).small()).changed() {
                                self.xlsx_cols.set(h, on);
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
            egui::Frame::none()
                .fill(stripe_bg(ui))
                .inner_margin(egui::Margin {
                    left: 0.0,
                    right: 0.0,
                    top: 5.0,
                    bottom: 5.0,
                })
                .rounding(egui::Rounding::same(3.0))
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
                // 行高 = 内容 26 + 上下内边距 3+3 = 32；**不含行间距**（show_rows 自己加）。
                const ROW_H: f32 = 32.0;
                // 表格最多占这么高，再多就自己滚；12 行够看，也不至于把整页挤没。
                const MAX_H: f32 = 420.0;
                let n_rows = self.rows.len();
                let rows = &mut self.rows;
                egui::ScrollArea::vertical()
                    .id_salt("rules-table")
                    .max_height(MAX_H)
                    .auto_shrink([false, true])
                    .show_rows(ui, ROW_H, n_rows, |ui, range| {
                        for i in range {
                            ui.push_id(i, |ui| {
                                let r = &mut rows[i];
                                // 隔行淡底：规则一多，行与行会糊成一片
                                let row_bg = if i % 2 == 1 {
                                    stripe_bg(ui)
                                } else {
                                    Color32::TRANSPARENT
                                };
                                egui::Frame::none()
                                    .fill(row_bg)
                                    .inner_margin(egui::Margin {
                                        left: 0.0,
                                        right: 0.0,
                                        top: 3.0,
                                        bottom: 3.0,
                                    })
                                    .rounding(egui::Rounding::same(3.0))
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

            // 文件名改名预览：勾了「同步替换文件名」就先把新名字摆出来。
            //
            // 这段以前**每帧**重算一遍：扫目录（walkdir）+ 对每个文件跑规则。
            // 目录一大，界面就会一直有 I/O，看起来"拖拉"。现在按
            // 「输入 + 规则指纹」缓存，只有真正改了东西才重算。
            if self.rename_files {
                let key = self.rename_preview_key();
                if self.rename_preview.as_ref().map(|(k, _)| k != &key) != Some(false) {
                    let lines = self.compute_rename_preview();
                    self.rename_preview = Some((key, lines));
                }
                if let Some((_, lines)) = &self.rename_preview {
                    if !lines.is_empty() {
                        ui.add_space(8.0);
                        ui.label(RichText::new("文件名改名预览").strong());
                        for l in lines {
                            ui.label(RichText::new(l.0.clone()).color(l.1));
                        }
                    }
                }
            }
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
                // 页眉替换 / 批量打印共用同一个面板，只是标题不同
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
                    "选择文档 —— 批量打印",
                    print_first,
                    vec!["doc", "docx"],
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
        }
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
                    _ => "选择文档 —— 批量打印",
                })
                .add_filter("Word 文档", &["doc", "docx"])
                .pick_files(),
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
            // **不再给日志套 ScrollArea**（0.2.3 试过）：日志区跟着页面滚就行，
            // 套一层内滚动条只会让人以为日志被截断了 —— 而且正是它把底部面板
            // 撑爆的（见 `render` 里那段）。
            const SHOW: usize = 240;
            let total = self.log.len();
            let start = total.saturating_sub(SHOW);
            if start > 0 {
                ui.label(
                    RichText::new(format!("……（前面还有 {start} 行，省略）"))
                        .small()
                        .color(DIM_C),
                );
            }
            for line in &self.log[start..] {
                ui.label(RichText::new(line).monospace().size(ui_scale::LOG));
            }
            if total == 0 {
                // 空态：写清"日志是什么、会去哪"，比一个「空」字有用。
                ui.label(
                    RichText::new(
                        "尚未运行 —— 点「执行替换」后这里逐行显示过程；\
                         结束后完整的日志会写到产物目录的「运行日志.txt」",
                    )
                    .small()
                    .color(DIM_C),
                );
            }
        });
    }
}

/// eframe 的入口：每帧调一次 `update`，其余全交给 `render`。
/// 这样布局代码也能在无窗口环境里被 `selftest` 直接驱动（见 `selftest`）。
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

// ─────────────────────── 小工具 ───────────────────────

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(title)
                    .size(ui_scale::SECTION)
                    .strong()
                    .color(Color32::from_rgb(0x1F, 0x4E, 0x79)),
            );
            ui.add_space(6.0);
            add(ui);
        });
}

/// 表格里的标签列（固定宽，保证每行对齐）。内容左对齐，与规则表表头一致。
fn cell_label(ui: &mut egui::Ui, w: f32, s: &str) {
    let _ = fixed_cell(ui, w, ui_scale::EDIT_H, RichText::new(s));
}

/// 路径输入框：固定宽高，不被父级布局压缩。
fn path_edit(ui: &mut egui::Ui, w: f32, s: &mut String, hint: &str) {
    let _ = ui.add_sized([w, ui_scale::EDIT_H], egui::TextEdit::singleline(s).hint_text(hint));
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

/// 明/暗主题。
fn visuals_of(light: bool) -> egui::Visuals {
    if light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    }
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
    /// 窗口标题「wrepl」。
    pub const TITLE: f32 = 24.0;
    /// 分区标题（「替换规则」「输出」…）。
    pub const SECTION: f32 = 17.0;
    /// 正文：输入框、列表、普通标签。
    pub const BODY: f32 = 16.0;
    /// 按钮（含「执行替换」）。
    pub const BUTTON: f32 = 16.0;
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

/// 运行时加载系统中文字体（不打包字体，避免授权与体积问题），并设定界面字号与间距。
///
/// 字号一律取自 [`ui_scale`]；字体只做**插入**、不替换 egui 自带字体 ——
/// egui 的字体表里还留着 Latin/emoji/符号（`✓`、`→`、`×` 之外的箭头等），
/// 把整个 `Proportional` 家族换掉会让这些字形变成豆腐块。
///
/// 优先微软雅黑 `msyh.ttc`——它是字体集合（`.ttc`），
/// `epaint` 会把 `FontData::index` 透传给 `ab_glyph::FontRef::try_from_slice_and_index`，
/// 所以索引 0 取第一张字面即可。找不到就退到黑体 / 宋体。
pub fn install_fonts(ctx: &egui::Context) {
    // 候选字体：**按"更现代"排，前面的优先**。
    //
    // 只读系统已装的字体、**不打包进产物** —— 零体积增长、无字体授权问题，
    // 也解释了为什么产物一直是 6~7 MB（对比：嵌一个中文字体要 +5~10 MB）。
    //
    // 前 8 项是更现代的中文无衬线（小米 MiSans / 华为 HarmonyOS Sans / 思源黑体 /
    // Noto Sans SC / 阿里普惠体 / OPPO Sans）。**本机一个都没装**，所以行为与以前
    // 完全一致；哪天装了，重启程序就自动用上，不用改代码。
    // 后面五项是 Windows 保底，尤其 `msyh.ttc`（微软雅黑）—— 简体中文系统的标配，
    // 字形覆盖最全，作为兜底最稳。
    //
    // 注意 `.ttc` 是字体集合，靠 `FontData::index` 选第几张字面（见下方注释）。
    const CANDIDATES: &[(&str, u32)] = &[
        // ── 更现代的中文黑体（装了才生效，都是免费商用授权）──
        ("C:/Windows/Fonts/MiSans-Regular.ttf", 0),
        ("C:/Windows/Fonts/MiSans-Regular.otf", 0),
        ("C:/Windows/Fonts/HarmonyOS_Sans_SC_Regular.ttf", 0),
        ("C:/Windows/Fonts/SourceHanSansSC-Regular.otf", 0),
        ("C:/Windows/Fonts/SourceHanSansCN-Regular.otf", 0),
        ("C:/Windows/Fonts/NotoSansSC-Regular.otf", 0),
        ("C:/Windows/Fonts/AlibabaPuHuiTi-3-55-Regular.ttf", 0),
        ("C:/Windows/Fonts/OPPOSans-R.ttf", 0),
        // ── Windows 保底 ──
        ("C:/Windows/Fonts/msyh.ttc", 0),
        ("C:/Windows/Fonts/msyhbd.ttc", 0),
        ("C:/Windows/Fonts/simhei.ttf", 0),
        ("C:/Windows/Fonts/simsun.ttc", 0),
        ("C:/Windows/Fonts/Deng.ttf", 0),
    ];

    let mut fonts = egui::FontDefinitions::default();
    let mut used = String::from("(系统默认)");
    for (path, idx) in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fd = egui::FontData::from_owned(bytes);
        fd.index = *idx;
        fonts.font_data.insert("cjk".to_owned(), fd);
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".to_owned());
        used = (*path).to_string();
        break;
    }
    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    // 五个内置字号**全部**显式给值，不留 egui 默认 —— 取值理由见 `ui_scale`。
    //   · 不给 `Small` 会漏掉表头/计数/提示/`.small()` 按钮这 11 处；
    //   · 不给 `Heading` 则将来谁写一句 `.heading()` 又会掉回默认的 18.0。
    for (which, size, family) in [
        (
            egui::TextStyle::Small,
            ui_scale::SMALL,
            egui::FontFamily::Proportional,
        ),
        (
            egui::TextStyle::Body,
            ui_scale::BODY,
            egui::FontFamily::Proportional,
        ),
        (
            egui::TextStyle::Button,
            ui_scale::BUTTON,
            egui::FontFamily::Proportional,
        ),
        (
            egui::TextStyle::Heading,
            ui_scale::SECTION,
            egui::FontFamily::Proportional,
        ),
        (
            egui::TextStyle::Monospace,
            ui_scale::LOG,
            egui::FontFamily::Monospace,
        ),
    ] {
        style.text_styles.insert(which, egui::FontId::new(size, family));
    }
    // 间距跟着字号一起放宽。只放大字号而不动间距，界面会比原来**更挤**：
    // 字与字的留白是按 egui 默认的小字号配的（item_spacing.y 只有 3.0）。
    style.spacing.item_spacing = egui::vec2(9.0, 5.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    // 按钮/复选框等的默认最小高度（egui 默认 18.0），要容纳 `BUTTON` 号字 + 上下内边距。
    style.spacing.interact_size = egui::vec2(44.0, 24.0);
    ctx.set_style(style);
    // 把实际选中的字体写进日志 —— 以后有人问"为什么看着不一样"，
    // 这一行能直接回答（而不是靠猜他机器上装了什么）。
    crate::diag::log(format!("界面字体：{used}"));
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
    install_fonts(&ctx);
    println!("  字体加载完成");

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
        let _ = ctx.run(raw.clone(), |ctx| app.render(ctx));
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
        let _ = ctx.run(raw.clone(), |ctx| app.render(ctx));
    }
    app.clear_armed = false;
    app.input = keep_input;
    println!("  多行输入框 + 「清空规则」待命态：布局 2 帧通过");

    // 3.7) 「完整镜像」复选框的两态：可用（写副本）与置灰（就地替换）。
    //      它是跟着「输出到子文件夹」启停的，普通一趟渲染只能照到其中一态。
    let keep_out = app.out_to_subdir;
    app.out_to_subdir = false;
    let _ = ctx.run(raw.clone(), |ctx| app.render(ctx));
    app.out_to_subdir = true;
    app.mirror = true;
    let _ = ctx.run(raw.clone(), |ctx| app.render(ctx));
    app.mirror = false;
    app.out_to_subdir = keep_out;
    println!("  「完整镜像」可用 / 置灰两态：布局通过");

    // 3.8) 新增的两页（页眉替换 / 批量打印）骨架：各画两帧。
    //      这两页是 v0.3.0 新加的，普通一趟渲染只照到「正文替换」一页，
    //      所以这里把另外两页也各渲染两帧钉住排版——**只是画，不触发任何
    //      Word COM 动作**，所以无头环境、没装 Word 的机器上也能跑。
    //      塞几行假路径是为了让「已选文档」列表与计数这两种状态也画出来。
    let keep_page = app.page;
    let keep_hdr_mode = app.wt.hdr_mode;
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
        for _ in 0..2 {
            let _ = ctx.run(raw.clone(), |ctx| app.render(ctx));
        }
    }
    app.page = keep_page;
    app.wt.hdr_mode = keep_hdr_mode;
    app.wt.hdr_docs.clear();
    app.wt.print_docs.clear();
    println!("  页眉替换（文本 / 图片）+ 批量打印：布局各 2 帧通过");

    // 3.9) 打印机枚举。批量打印页的「打印机」下拉全靠它，而它**不碰 COM**
    //      （纯 Win32 `EnumPrintersW`），所以无头环境照样能验。
    //      这里只是"报出来"：一台都没装打印机是合法情形，不该判失败。
    app.wt.reload_printers();
    match &app.wt.printers_err {
        Some(e) => println!("  · 打印机枚举失败：{e}（批量打印页会显示这条，仍可用默认打印机）"),
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

    // 4) 真跑一次批量
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
