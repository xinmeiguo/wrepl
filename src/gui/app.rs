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
//! ## 关于"同步修改文件名"的默认值
//!
//! **默认不勾**。改名会动交付物文件名，属于容易被忽略的副作用；
//! 需要时由人显式勾上，勾了之后报告里会多出「文件名对照」表。
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

const OK_C: Color32 = Color32::from_rgb(0x1B, 0x7F, 0x3B);
const WARN_C: Color32 = Color32::from_rgb(0xB5, 0x6A, 0x00);
const ERR_C: Color32 = Color32::from_rgb(0xC0, 0x2B, 0x1D);
const DIM_C: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);

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
    /// 是否预先勾上「同步修改文件名」。默认 **false**——与界面默认值一致，
    /// 改名必须由人显式要求，哪怕在预置模式下也不替人做决定。
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
    input: String,
    /// 勾上「输出到子文件夹」时才用：产物目录。默认 `<输入目录>/out`。
    output: String,
    /// 落盘位置开关。**false（默认）＝ 就地替换源文件**；
    /// true ＝ 写到 `output` 目录，源文件不动。
    out_to_subdir: bool,
    recursive: bool,
    exclude: String,

    // 规则
    rows: Vec<Row>,
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
    /// 「同步修改文件名」：勾上后同一套规则也作用到文件名上。
    ///
    /// 这是文件名维度的**唯一**开关——勾了才动文件名，不勾一律不动，
    /// 规则文件里「作用域」写没写「文件名」都不影响这个判断。
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
    /// 本次执行的日志在 `log` 里的起点——落盘的「运行日志.txt」只取这一段，
    /// 不把开窗提示和上一次执行也捎进去。
    run_log_from: usize,
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
            recursive: false,
            exclude: String::new(),
            rows: Vec::new(),
            chain: false,
            // ★ 默认不勾：保持了工具一贯的"不猜"——重叠时两条都不改，把冲突报出来。
            longest_first: false,
            xlsx_cols,
            // ★ 默认不勾：改名会动交付物文件名，必须由人显式决定
            rename_files: false,
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
            run_log_from: 0,
        };
        app.push_log("就绪：选输入 → 填规则（或「从 Excel 导入」）→ 点「执行替换」");
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

    fn input_paths(&self) -> Vec<PathBuf> {
        vec![PathBuf::from(self.input.trim())]
    }

    /// 输入所在的目录（输入是目录就是它本身；是单个 .docx 就是它的父目录）。
    fn input_dir(&self) -> PathBuf {
        let s = self.input.trim();
        if s.is_empty() {
            return PathBuf::new();
        }
        let p = Path::new(s);
        if p.is_dir() {
            p.to_path_buf()
        } else {
            p.parent().map(|x| x.to_path_buf()).unwrap_or_default()
        }
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
    /// **「文件名」那一维由 「规则」区的「同步修改文件名」开关说了算**——
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
                "第 {} 条规则没有可用作用域：它只作用到「文件名」，但「同步修改文件名」没勾上",
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
        }
    }

    // ─────────────────────── 动作 ───────────────────────

    fn precheck(&mut self) -> bool {
        if self.input.trim().is_empty() {
            self.error = Some("请先指定输入（目录或 .docx 文件）".into());
            return false;
        }
        if !Path::new(self.input.trim()).exists() {
            self.error = Some(format!("输入路径不存在：{}", self.input.trim()));
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
            format!("落盘位置：副本 → {}", self.output.trim())
        } else if self.backup {
            "落盘位置：就地替换源文件（保留 .bak 备份）".to_string()
        } else {
            "落盘位置：就地替换源文件（不留备份）".to_string()
        });

        let opts = self.options_for(dry);
        let paths = self.input_paths();
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
                        .size(22.0)
                        .strong()
                        .color(Color32::from_rgb(0x1F, 0x4E, 0x79)),
                );
                ui.label(RichText::new("Word 批量替换").color(DIM_C));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.light_theme, "浅色");
                });
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let running = self.job.is_some();
                let can_run = !self.input.trim().is_empty() && !running;
                let label = if running { "执行中…" } else { "执行替换" };
                if ui
                    .add_enabled(
                        can_run,
                        egui::Button::new(RichText::new(label).size(15.0).strong()),
                    )
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
            if let Some(job) = &self.job {
                ui.add_space(4.0);
                let frac = if job.total == 0 {
                    0.0
                } else {
                    (job.done as f32 / job.total as f32).clamp(0.0, 1.0)
                };
                let text = if job.total == 0 {
                    "正在收集待处理文件…".to_string()
                } else {
                    format!("{} / {} 个文件", job.done, job.total)
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

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.section_io(ui);
                ui.add_space(10.0);
                self.section_rules(ui);
                ui.add_space(10.0);
                self.section_result(ui);
                ui.add_space(10.0);
                self.section_log(ui);
            });
        });
    }

    fn section_io(&mut self, ui: &mut egui::Ui) {
        section(ui, "输入与输出", |ui| {
            const L: f32 = 52.0; // 标签列宽
            const F: f32 = 560.0; // 路径输入框宽

            let mut pick_in_dir = false;
            let mut pick_in_file = false;
            let mut pick_out_dir = false;
            let mut use_sibling_out = false;

            ui.horizontal(|ui| {
                cell_label(ui, L, "输入");
                path_edit(ui, F, &mut self.input, "");
                if ui.button("选目录…").clicked() {
                    pick_in_dir = true;
                }
                if ui.button("选文件…").clicked() {
                    pick_in_file = true;
                }
            });
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                cell_label(ui, L, "输出");
                ui.checkbox(&mut self.out_to_subdir, "输出到子文件夹");
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
                    if ui.button("用输入旁 out").clicked() {
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
                    [180.0, 24.0],
                    egui::TextEdit::singleline(&mut self.exclude).hint_text("*_bak*"),
                );
            });

            // 对话框动作放在布局之后执行：避免在借用 self 的闭包里嵌套借用
            if pick_in_dir {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.input = p.display().to_string();
                }
            }
            if pick_in_file {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("Word 文档", &["docx"])
                    .pick_file()
                {
                    self.input = p.display().to_string();
                }
            }
            if pick_out_dir {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    self.output = p.display().to_string();
                }
            }
            if use_sibling_out {
                let inp = self.input.trim().to_string();
                if !inp.is_empty() {
                    let p = Path::new(&inp);
                    let base = if p.is_dir() {
                        p.to_path_buf()
                    } else {
                        p.parent().map(|x| x.to_path_buf()).unwrap_or_default()
                    };
                    self.output = base.join("out").display().to_string();
                }
            }
        });
    }

    fn section_rules(&mut self, ui: &mut egui::Ui) {
        let mut import_txt = false;
        let mut import_xlsx = false;
        let mut export_xlsx = false;
        section(ui, "规则", |ui| {
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
                ui.checkbox(&mut self.rename_files, "同步修改文件名");
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
                        for (w, h) in [(w_find, "查找内容"), (w_repl, "替换为"), (W_HIT, "命中")] {
                            fixed_cell(ui, w, 20.0, RichText::new(h).strong());
                        }
                    });
                });
            ui.add_space(4.0);

            let mut delete: Option<usize> = None;
            for (i, r) in self.rows.iter_mut().enumerate() {
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
                                [w_find, 26.0],
                                egui::TextEdit::singleline(&mut r.find).hint_text("查找内容"),
                            );
                            ui.add_sized(
                                [w_repl, 26.0],
                                egui::TextEdit::singleline(&mut r.replace).hint_text("替换为"),
                            );
                            let resp = fixed_cell(ui, W_HIT, 26.0, match r.hits {
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
                            // 用 U+00D7（乘号）而不是 U+2715：后者在装入的中文字体里没有字形，
                            // 实测渲染成豆腐块。
                            if ui
                                .add_sized([W_DEL, 26.0], egui::Button::new("×").small())
                                .clicked()
                            {
                                delete = Some(i);
                            }
                        });
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

            // 文件名改名预览：勾了「同步修改文件名」就先把新名字摆出来。
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

        // 文件对话框要借 &mut self，只能放在上面的闭包之外执行
        if import_txt {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("文本规则文件", &["txt"])
                .pick_file()
            {
                let s = p.display().to_string();
                // load_rules_file 是**追加**语义（`--preset` 要它这样）；
                // 按钮这里要的是"整表替换"，所以先取走原表，解析失败再还回去——
                // 一次读文件失败不该把界面上已经填好的规则清空。
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
        }
        if import_xlsx {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("Excel 规则表", &["xlsx"])
                .pick_file()
            {
                let s = p.display().to_string();
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
        }
        if export_xlsx {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("Excel 规则表", &["xlsx"])
                .set_file_name("规则.xlsx")
                .save_file()
            {
                let s = p.display().to_string();
                match self.export_rules_xlsx(&s) {
                    Ok(n) => {
                        self.push_log(format!("已导出 {n} 条规则到：{s}"));
                        self.toast = Some((format!("已导出 {n} 条规则"), true));
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        }
    }

    fn section_result(&mut self, ui: &mut egui::Ui) {
        let Some(res) = &self.result else {
            return;
        };
        let title = if self.dry_run {
            "上次结果（未落盘）"
        } else {
            "上次结果"
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
        // 各分区一律不带序号：「上次结果」没跑过之前不出现，
        // 一旦编号就会断号；而且序号本身对使用毫无帮助。
        section(ui, "运行日志", |ui| {
            // 只读：渲染成可选中的标签，不给文本框——可编辑的日志框会让人
            // 误以为改动有意义，而下一帧就会被真实轨迹覆盖掉。
            // 完整的一份在每次执行后落到产物目录的「运行日志.txt」里。
            const SHOW: usize = 120;
            let total = self.log.len();
            let start = total.saturating_sub(SHOW);
            for line in &self.log[start..] {
                ui.label(RichText::new(line).monospace().size(13.0));
            }
            if total == 0 {
                ui.label(RichText::new("空").color(DIM_C));
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
                    .size(16.0)
                    .strong()
                    .color(Color32::from_rgb(0x1F, 0x4E, 0x79)),
            );
            ui.add_space(6.0);
            add(ui);
        });
}

/// 表格里的标签列（固定宽，保证每行对齐）。内容左对齐，与规则表表头一致。
fn cell_label(ui: &mut egui::Ui, w: f32, s: &str) {
    let _ = fixed_cell(ui, w, 24.0, RichText::new(s));
}

/// 路径输入框：固定宽高，不被父级布局压缩。
fn path_edit(ui: &mut egui::Ui, w: f32, s: &mut String, hint: &str) {
    let _ = ui.add_sized([w, 24.0], egui::TextEdit::singleline(s).hint_text(hint));
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

// ─────────────────────── 字体 ───────────────────────

/// 运行时加载系统中文字体（不打包字体，避免授权与体积问题）。
///
/// 优先微软雅黑 `msyh.ttc`——它是字体集合（`.ttc`），
/// `epaint` 会把 `FontData::index` 透传给 `ab_glyph::FontRef::try_from_slice_and_index`，
/// 所以索引 0 取第一张字面即可。找不到就退到黑体 / 宋体。
pub fn install_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[(&str, u32)] = &[
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
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(15.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(15.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(13.5, egui::FontFamily::Monospace),
    );
    ctx.set_style(style);
    let _ = used;
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
