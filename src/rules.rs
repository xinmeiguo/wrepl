//! 规则模型与来源解析。
//!
//! 一条规则 = 一行。三种来源最终都归一成同一个 `Vec<Rule>`：
//!
//! 1. 命令行逐条：`--rule "查找=>替换"`（可重复）
//! 2. 文本规则文件：每行 `查找<TAB>替换[<TAB>作用域[<TAB>选项[<TAB>备注]]]`
//! 3. Excel 规则表（见 `from_xlsx`，里程碑 M4）
//!
//! ## 执行语义（与旧工具反着来，见设计规格 §3.3）
//!
//! - 多条规则**默认各自独立、基于原文扫描**，不做链式覆盖
//! - 命中区间重叠时**报冲突**，两条都不替换，不静默取一个
//! - 链式（前一条的输出当后一条的输入）需 `--chain` 显式开启

use crate::docx::package::PartKind;
use crate::docx::scan;
use anyhow::{Context, Result, bail};
use std::path::Path;

/// 规则作用域（位标志）。文本框、页眉页脚、脚注、批注各自独立，不做隐式包含。
///
/// `filename` 是唯一**不落在文档内部**的维度：它控制这条规则要不要参与
/// 「文件名同步改名」。默认在 `全部` 里，但改名由调用方的总开关
/// （`--rename-files` / 界面复选框）把关，默认关闭，所以不会平白改到文件名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub body: bool,
    pub header_footer: bool,
    pub textbox: bool,
    pub footnote: bool,
    pub comment: bool,
    pub filename: bool,
}

impl Scope {
    pub const ALL: Scope = Scope {
        body: true,
        header_footer: true,
        textbox: true,
        footnote: true,
        comment: true,
        filename: true,
    };

    pub const BODY_ONLY: Scope = Scope {
        body: true,
        header_footer: false,
        textbox: false,
        footnote: false,
        comment: false,
        filename: false,
    };

    /// 解析作用域串：`全部` / `正文,页眉页脚,文本框,脚注,批注`。
    ///
    /// 遇到未知值**直接报错并列出可用值**，不猜、不静默忽略。
    pub fn parse(s: &str) -> Result<Self> {
        let t = s.trim();
        if t.is_empty() || t == "全部" || t.eq_ignore_ascii_case("all") {
            return Ok(Scope::ALL);
        }
        let mut sc = Scope {
            body: false,
            header_footer: false,
            textbox: false,
            footnote: false,
            comment: false,
            filename: false,
        };
        for part in t.split([',', '，', ';', '；', '|']) {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            match p {
                "正文" | "body" => sc.body = true,
                "页眉页脚" | "页眉" | "页脚" | "header_footer" | "header" | "footer" => {
                    sc.header_footer = true
                }
                "文本框" | "textbox" => sc.textbox = true,
                "脚注" | "尾注" | "脚注尾注" | "footnote" | "endnote" => sc.footnote = true,
                "批注" | "comment" => sc.comment = true,
                "文件名" | "文件名称" | "filename" => sc.filename = true,
                _ => bail!(
                    "未知作用域：`{p}`（可用值：正文,页眉页脚,文本框,脚注,批注,文件名；或写 全部）"
                ),
            }
        }
        Ok(sc)
    }

    pub fn display(&self) -> String {
        let mut v = Vec::new();
        if self.body {
            v.push("正文");
        }
        if self.header_footer {
            v.push("页眉页脚");
        }
        if self.textbox {
            v.push("文本框");
        }
        if self.footnote {
            v.push("脚注");
        }
        if self.comment {
            v.push("批注");
        }
        if self.filename {
            v.push("文件名");
        }
        if v.len() == 6 {
            return "全部".to_string();
        }
        v.join(",")
    }

    pub fn allows(&self, slot: Slot) -> bool {
        match slot {
            Slot::Body => self.body,
            Slot::HeaderFooter => self.header_footer,
            Slot::Textbox => self.textbox,
            Slot::Footnote => self.footnote,
            Slot::Comment => self.comment,
        }
    }

    /// 一个维度都没开。界面上写「正文,文件名」这类组合时可能被开关掐成空域，
    /// 空域规则会静默 0 命中——宁可报错让调用方看见。
    pub fn is_empty(&self) -> bool {
        self.content_empty() && !self.filename
    }

    /// 一个**内容**维度都没开（只剩文件名）。
    ///
    /// 这类规则的字面文本只可能出现在文件名里，不会出现在任何 part 的内容中：
    /// 残留自检（[`crate::verify::residue`]）必须跳过它们，否则每个文件都会报
    /// 一条假残留。原先那里把六个字段手写展开了一遍，与 `is_empty` 各写各的，
    /// 加字段时极易走偏——收成这里一个谓词，两处共用。
    pub fn content_empty(&self) -> bool {
        !(self.body || self.header_footer || self.textbox || self.footnote || self.comment)
    }
}

impl Default for Scope {
    fn default() -> Self {
        Scope::ALL
    }
}

/// 一个段落在文档里的归属位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Body,
    HeaderFooter,
    Textbox,
    Footnote,
    Comment,
}

impl Slot {
    pub fn label(self) -> &'static str {
        match self {
            Slot::Body => "正文",
            Slot::HeaderFooter => "页眉页脚",
            Slot::Textbox => "文本框",
            Slot::Footnote => "脚注",
            Slot::Comment => "批注",
        }
    }
}

/// 由「part 类型 + 是否在文本框内」判定段落归属；`None` = 该 part 不参与替换。
pub fn slot_of(kind: PartKind, in_textbox: bool) -> Option<Slot> {
    match kind {
        PartKind::Document => Some(if in_textbox { Slot::Textbox } else { Slot::Body }),
        PartKind::Header | PartKind::Footer => Some(Slot::HeaderFooter),
        PartKind::Footnotes | PartKind::Endnotes => Some(Slot::Footnote),
        PartKind::Comments => Some(Slot::Comment),
        PartKind::Other => None,
    }
}

/// 一条替换规则。
#[derive(Debug, Clone)]
pub struct Rule {
    /// 序号，用于报告回溯（Excel 里的"序号"列）
    pub id: u32,
    pub find: String,
    pub replace: String,
    pub enabled: bool,

    pub case_sensitive: bool,
    pub whole_word: bool,
    pub use_wildcard: bool,
    /// **区分全半角**（`true` = 不把全角 ASCII / U+3000 归一到半角）。
    ///
    /// 字段名沿用规则表的历史列标识 `kana_sensitive`（flag 别名 `kana` /
    /// `全半角`，见 [`Rule::from_cli_arg`]），**与假名无关**——实际语义就是
    /// [`engine::fold_char`] 的 `width_sensitive`。
    pub kana_sensitive: bool,

    pub scope: Scope,
    pub note: String,
    /// 来源描述（命令行第几条 / 文件第几行），出错时能指回原处
    pub origin: String,
}

impl Rule {
    pub fn new(id: u32, find: impl Into<String>, replace: impl Into<String>) -> Self {
        Rule {
            id,
            find: find.into(),
            replace: replace.into(),
            enabled: true,
            case_sensitive: false,
            whole_word: false,
            use_wildcard: false,
            kana_sensitive: false,
            scope: Scope::ALL,
            note: String::new(),
            origin: String::new(),
        }
    }

    /// 一行摘要，用于控制台回显。
    pub fn summary(&self) -> String {
        let mut opts = Vec::new();
        if self.case_sensitive {
            opts.push("区分大小写");
        }
        if self.whole_word {
            opts.push("全字匹配");
        }
        if self.use_wildcard {
            opts.push("通配符");
        }
        if self.kana_sensitive {
            opts.push("区分全半角");
        }
        format!(
            "#{} {}=>{}  [{}]{}",
            self.id,
            self.find,
            self.replace,
            self.scope.display(),
            if opts.is_empty() {
                String::new()
            } else {
                format!(" {{{}}}", opts.join(","))
            }
        )
    }
}

/// 解析选项字段：逗号分隔的 `case` / `whole` / `kana` / `wildcard` / `off`。
fn apply_options(rule: &mut Rule, s: &str) -> Result<()> {
    for tok in s.split([',', '，', ';', '；', '|', ' ']) {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        match t {
            "case" | "区分大小写" => rule.case_sensitive = true,
            "whole" | "全字匹配" => rule.whole_word = true,
            "kana" | "全半角" | "区分全半角" => rule.kana_sensitive = true,
            "wildcard" | "通配符" => rule.use_wildcard = true,
            "off" | "disable" | "禁用" => rule.enabled = false,
            "on" | "enable" | "启用" => rule.enabled = true,
            _ => bail!("未知规则选项：`{t}`（可用：case, whole, kana, wildcard, on, off）"),
        }
    }
    Ok(())
}

/// 解析文本规则文件的一行。
///
/// 返回 `None` 表示这行是注释或空行。
pub fn parse_line(line: &str, id: u32, defaults: &Rule) -> Result<Option<Rule>> {
    let t = line.trim_end_matches(['\r', '\n']);
    let trimmed = t.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
        return Ok(None);
    }

    let mut rule = defaults.clone();
    rule.id = id;

    if t.contains('\t') {
        // 制表符分隔：查找 / 替换 / 作用域 / 选项 / 备注
        let cols: Vec<&str> = t.split('\t').collect();
        if cols.len() < 2 {
            bail!("制表符分隔的行至少要有两列（查找、替换）");
        }
        rule.find = cols[0].to_string();
        rule.replace = cols[1].to_string();
        if let Some(s) = cols.get(2).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            rule.scope = Scope::parse(s)?;
        }
        if let Some(s) = cols.get(3).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            apply_options(&mut rule, s)?;
        }
        if let Some(s) = cols.get(4) {
            rule.note = s.trim().to_string();
        }
    } else if let Some((f, r)) = t.split_once("=>") {
        rule.find = f.trim().to_string();
        rule.replace = r.trim().to_string();
    } else {
        bail!("无法解析：既没有制表符，也没有 `=>` 分隔符");
    }

    if rule.find.is_empty() {
        return Ok(None); // 查找内容为空 = 该行被注释掉，不报错
    }
    Ok(Some(rule))
}

/// 读入文本规则文件。
pub fn from_text_file(path: &Path, defaults: &Rule, start_id: u32) -> Result<Vec<Rule>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读不到规则文件：{}", path.display()))?;
    let mut out = Vec::new();
    let mut id = start_id;
    for (i, line) in content.lines().enumerate() {
        let lineno = i + 1;
        match parse_line(line, id, defaults).with_context(|| {
            format!("规则文件 {} 第 {lineno} 行解析失败", path.display())
        })? {
            Some(mut r) => {
                r.origin = format!("{}:{lineno}", path.display());
                id += 1;
                out.push(r);
            }
            None => {}
        }
    }
    Ok(out)
}

/// 解析命令行 `--rule "查找=>替换"`。
pub fn from_cli_arg(arg: &str, id: u32, defaults: &Rule) -> Result<Rule> {
    let mut rule = defaults.clone();
    rule.id = id;
    rule.origin = format!("命令行第 {id} 条");
    let (f, r) = arg
        .split_once("=>")
        .with_context(|| format!("规则 `{arg}` 缺少 `=>` 分隔符，应写成 查找=>替换"))?;
    rule.find = f.to_string();
    rule.replace = r.to_string();
    if rule.find.is_empty() {
        bail!("规则 `{arg}` 的查找内容为空");
    }
    Ok(rule)
}

/// 把规则清单渲染成文本规则文件的内容（`rules template` 用）。
pub fn dump_to_string(rules: &[Rule]) -> String {
    let mut s = String::new();
    s.push_str("# 每行格式（制表符分隔）：查找内容\t替换为\t作用域\t选项\t备注\n");
    s.push_str("# 作用域：正文,页眉页脚,文本框,脚注,批注（或 全部）\n");
    s.push_str("# 选项：case(区分大小写), whole(全字匹配), kana(区分全半角), off(禁用)\n");
    s.push_str("# 以 # 开头的行和空行会被忽略\n");
    for r in rules {
        let mut opts = Vec::new();
        if r.case_sensitive {
            opts.push("case");
        }
        if r.whole_word {
            opts.push("whole");
        }
        if r.kana_sensitive {
            opts.push("kana");
        }
        if r.use_wildcard {
            opts.push("wildcard");
        }
        if !r.enabled {
            opts.push("off");
        }
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            r.find,
            r.replace,
            r.scope.display(),
            opts.join(","),
            r.note
        ));
    }
    s
}

// ─────────────────────────── Excel 规则表（里程碑 M4）───────────────────────────

/// 规则表的**固定两列**：第 1 列 = 查找内容，第 2 列 = 替换为。
///
/// 表里带表头时按标题名认（列序可以乱），不带表头时按这个位置读。
pub const XLSX_FIXED_HEADERS: [&str; 2] = ["查找内容", "替换为"];

/// 规则表的**可选列**：带表头时按**标题名**识别，与列序无关。
///
/// 读不读、写不写，由 [`XlsxLayout`] 决定（界面上是一组复选框）——
/// 表里没有的列会走默认值，不必为了省一列去改代码。
pub const XLSX_OPTIONAL_HEADERS: [&str; 7] = [
    "区分大小写",
    "全字匹配",
    "使用通配符",
    "区分全半角",
    "作用域",
    "启用",
    "备注",
];

/// 界面上**可勾选**的 Excel 列 = [`XLSX_OPTIONAL_HEADERS`] 去掉「启用」。
///
/// 界面没有"启用"这个开关：规则既然写进表里，就是要替换的。
/// 但读表时「启用」列**仍然会被认**（见 [`XlsxLayout::for_read`]），
/// 用来剔除标为未启用的行。
pub const XLSX_UI_HEADERS: [&str; 6] = [
    "区分大小写",
    "全字匹配",
    "使用通配符",
    "区分全半角",
    "作用域",
    "备注",
];

/// 历史规则表里出现过的第 1 列列名。
///
/// 早期版本在「查找内容」前面放了一列「序号」。按标题名识别时它会自然落空、
/// 被忽略，不必为它写特例——这里留着是为了让**旧表也能被认成表头**。
pub const XLSX_LEGACY_INDEX_HEADERS: [&str; 3] = ["序号", "编号", "#"];

/// Excel 规则表的列布局：固定两列之外，哪些可选列参与读 / 写。
///
/// 默认**全部启用**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XlsxLayout {
    /// 与 [`XLSX_OPTIONAL_HEADERS`] 同序
    on: Vec<bool>,
}

impl Default for XlsxLayout {
    fn default() -> Self {
        Self {
            on: vec![true; XLSX_OPTIONAL_HEADERS.len()],
        }
    }
}

impl XlsxLayout {
    pub fn all() -> Self {
        Self::default()
    }

    fn slot(header: &str) -> Option<usize> {
        XLSX_OPTIONAL_HEADERS.iter().position(|h| *h == header)
    }

    /// 这一列是否参与读写。未知列名一律 `false`。
    pub fn is_on(&self, header: &str) -> bool {
        Self::slot(header)
            .and_then(|i| self.on.get(i).copied())
            .unwrap_or(false)
    }

    pub fn set(&mut self, header: &str, v: bool) {
        if let Some(i) = Self::slot(header) {
            if let Some(s) = self.on.get_mut(i) {
                *s = v;
            }
        }
    }

    /// 启用的可选列，按 [`XLSX_OPTIONAL_HEADERS`] 的原序。
    pub fn enabled_headers(&self) -> Vec<&'static str> {
        XLSX_OPTIONAL_HEADERS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.on.get(*i).copied().unwrap_or(false))
            .map(|(_, h)| *h)
            .collect()
    }

    /// 写表时用的表头：固定两列在前，启用的可选列依次在后。
    pub fn headers(&self) -> Vec<String> {
        XLSX_FIXED_HEADERS
            .iter()
            .chain(self.enabled_headers().iter())
            .map(|s| s.to_string())
            .collect()
    }

    /// 读表用的布局：在界面上勾的列之外，**强制认「启用」列**。
    ///
    /// 界面不显示"启用"（写进表里的规则就是要执行的），但表里可能带着这一列
    /// ——命令行 `rules template` 生成的模板就有。标着「启用 = 否」的行必须能被
    /// 认出来并剔除，而不是当成普通规则悄悄跑掉：静默执行一条人以为关掉的规则，
    /// 是这个工具最不能犯的错。
    pub fn for_read(&self) -> Self {
        let mut me = self.clone();
        me.set("启用", true);
        me
    }
}

/// 模板里的示例规则——**默认 `启用 = 否`**，误跑也不会改到东西。
pub fn template_rules() -> Vec<Rule> {
    let mut a = Rule::new(1, "某某生物制品有限公司", "某某生物");
    a.enabled = false;
    a.note = "示例：把简称改全称，启用后生效".into();

    let mut b = Rule::new(2, "DP-AUC-01", "DP-AUC-02");
    b.enabled = false;
    b.scope = Scope::BODY_ONLY;
    b.note = "示例：编号替换（含 noBreakHyphen 连字符）".into();

    vec![a, b]
}

fn yn(b: bool) -> &'static str {
    if b { "Y" } else { "N" }
}

/// 按给定布局把一条规则渲染成一行单元格文本，供导出与模板使用。
///
/// `use_wildcard` 在数据模型里是存在的：这里**照实写**，谁传进来的值谁负责，
/// 不在这层替调用方圆谎。
pub fn xlsx_row(layout: &XlsxLayout, r: &Rule) -> Vec<String> {
    let mut row = vec![r.find.clone(), r.replace.clone()];
    for h in layout.enabled_headers() {
        row.push(match h {
            "区分大小写" => yn(r.case_sensitive).to_string(),
            "全字匹配" => yn(r.whole_word).to_string(),
            "使用通配符" => yn(r.use_wildcard).to_string(),
            "区分全半角" => yn(r.kana_sensitive).to_string(),
            "作用域" => r.scope.display(),
            "启用" => if r.enabled { "是" } else { "否" }.to_string(),
            "备注" => r.note.clone(),
            _ => String::new(),
        });
    }
    row
}

/// 解析 Y/N 型单元格。空 = 取默认值；无法识别 = 报错（不猜）。
fn parse_bool(v: &str, default: bool, col: &str, row: usize) -> Result<bool> {
    match v.trim().to_ascii_uppercase().as_str() {
        "" => Ok(default),
        "Y" | "YES" | "TRUE" | "1" | "是" | "启用" | "√" => Ok(true),
        "N" | "NO" | "FALSE" | "0" | "否" | "禁用" | "×" => Ok(false),
        other => bail!("规则表第 {row} 行 `{col}` 列的值 `{other}` 无法识别（用 Y/N、TRUE/FALSE 或 是/否）"),
    }
}

fn cell<'a>(row: &'a [String], i: usize) -> &'a str {
    row.get(i).map(|s| s.as_str()).unwrap_or("")
}

/// 表头判定的兜底词：首行带这些字样，却一个已知列名都对不上，
/// 那就不是我们要的表头 —— **报错**，而不是把它当成数据行读进去。
const HEADER_HINTS: [&str; 8] = [
    "查找", "替换", "原内容", "新内容", "搜索", "find", "search", "replace",
];

fn looks_like_header_row(row: &[String]) -> bool {
    row.iter().take(4).any(|c| {
        let t = c.trim().to_ascii_lowercase();
        !t.is_empty() && HEADER_HINTS.iter().any(|h| t.contains(&h.to_ascii_lowercase()))
    })
}

/// 首行是不是表头？是的话返回「列名 → 列号」。
///
/// 判据：任意一格（去空白后）等于某个已知列名。这样「序号」在最前、
/// 或者可选列顺序被打乱的表都能认出来。
fn header_positions(row: &[String]) -> Option<std::collections::HashMap<String, usize>> {
    let mut map = std::collections::HashMap::new();
    for (i, c) in row.iter().enumerate() {
        let t = c.trim();
        if t.is_empty() {
            continue;
        }
        let known = XLSX_FIXED_HEADERS.contains(&t)
            || XLSX_OPTIONAL_HEADERS.contains(&t)
            || XLSX_LEGACY_INDEX_HEADERS.contains(&t);
        if known {
            map.insert(t.to_string(), i);
        }
    }
    if map.is_empty() { None } else { Some(map) }
}

/// 取一个可选列的值：没启用、或表里没这列，都返回空串（等于走默认值）。
fn opt_cell<'a>(
    head: &'a Option<std::collections::HashMap<String, usize>>,
    row: &'a [String],
    layout: &XlsxLayout,
    name: &str,
) -> &'a str {
    if !layout.is_on(name) {
        return "";
    }
    match head {
        Some(h) => h.get(name).map(|i| cell(row, *i)).unwrap_or(""),
        None => "",
    }
}

/// 把 Excel 规则表的行转成规则清单。
///
/// 列怎么认：
/// - **有表头**（首行含已知列名）→ 全部**按标题名**认，列序随意；
///   旧表的「序号」列自然落空、被忽略。
/// - **没表头** → 只按位置读第 1、2 列 = 查找内容 / 替换为。
///
/// 可选列读不读由 `layout` 决定（界面上是一组复选框）；没启用的一律当"没有这列"。
///
/// 返回 `(规则, 警告)`。警告是**要人去看一眼**的事，不是错误：
/// 例如"查找内容"是个 15 位以上纯数字串——Excel 存储层可能已经把它截断成 0 了。
pub fn from_xlsx_rows(
    rows: &[Vec<String>],
    layout: &XlsxLayout,
    defaults: &Rule,
    start_id: u32,
    source: &str,
) -> Result<(Vec<Rule>, Vec<String>)> {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let mut id = start_id;

    let head = rows.first().and_then(|r| header_positions(r));

    if head.is_none() {
        if let Some(first) = rows.first() {
            // 首行像表头却认不出列名：宁可报错，也不要把「原内容 / 新内容」
            // 当成一条真规则读进去——那种错会一路跑到产物里。
            if looks_like_header_row(first) {
                let seen: Vec<String> = first
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                bail!(
                    "规则表首行像是表头，但没有一个列名认得出来。\n\
                     首行：{}\n\
                     固定两列：{}\n\
                     可选列：{}\n\
                     把列名改成上面的写法即可（列序随意）；确实不要表头就把这一行删掉，\
                     那样会按位置读前两列。",
                    seen.join(" | "),
                    XLSX_FIXED_HEADERS.join(" / "),
                    XLSX_OPTIONAL_HEADERS.join(" / "),
                );
            }
        }
    }

    if head.is_none() && rows.first().map(|r| r.len() > 2).unwrap_or(false) {
        warnings.push(
            "规则表没有表头行：只按位置读了第 1、2 列（查找内容 / 替换为），其余列被忽略。\
             想让可选列生效，请在首行写上列名（如「作用域」「启用」「备注」）"
                .to_string(),
        );
    }
    if let Some(h) = &head {
        if !h.contains_key("查找内容") {
            let seen: Vec<String> = rows[0]
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            bail!(
                "规则表有表头，但找不到「查找内容」列。认得的表头：{} / {} …；这张表的首行是：{}",
                XLSX_FIXED_HEADERS.join(" / "),
                XLSX_OPTIONAL_HEADERS.join(" / "),
                seen.join(" | ")
            );
        }
    }

    for (i, row) in rows.iter().enumerate() {
        let lineno = i + 1;
        if i == 0 && head.is_some() {
            continue; // 表头行
        }

        let (find, replace) = match &head {
            Some(h) => {
                let fi = *h.get("查找内容").expect("上面已确认存在");
                let ri = h.get("替换为").copied();
                (
                    cell(row, fi).to_string(),
                    ri.map(|x| cell(row, x).to_string()).unwrap_or_default(),
                )
            }
            None => (cell(row, 0).to_string(), cell(row, 1).to_string()),
        };
        if find.trim().is_empty() {
            continue; // 查找内容为空 = 该行被注释掉，不报错
        }

        let mut rule = defaults.clone();
        rule.id = id;
        rule.origin = format!("{source} 第 {lineno} 行");
        rule.find = find.clone();
        rule.replace = replace;
        rule.case_sensitive = parse_bool(
            opt_cell(&head, row, layout, "区分大小写"),
            defaults.case_sensitive,
            "区分大小写",
            lineno,
        )?;
        rule.whole_word = parse_bool(
            opt_cell(&head, row, layout, "全字匹配"),
            defaults.whole_word,
            "全字匹配",
            lineno,
        )?;
        rule.use_wildcard = parse_bool(
            opt_cell(&head, row, layout, "使用通配符"),
            defaults.use_wildcard,
            "使用通配符",
            lineno,
        )?;
        rule.kana_sensitive = parse_bool(
            opt_cell(&head, row, layout, "区分全半角"),
            defaults.kana_sensitive,
            "区分全半角",
            lineno,
        )?;
        let scope_cell = opt_cell(&head, row, layout, "作用域");
        if !scope_cell.trim().is_empty() {
            rule.scope = Scope::parse(scope_cell)
                .with_context(|| format!("规则表第 {lineno} 行 `作用域` 列解析失败"))?;
        }
        rule.enabled = parse_bool(opt_cell(&head, row, layout, "启用"), true, "启用", lineno)?;
        rule.note = opt_cell(&head, row, layout, "备注").to_string();

        // Excel 存储层会把 15 位以上的数字截断（超 12 位还变科学记数），这不是我们的 bug，
        // 但必须提示——否则替换目标会悄悄变成错的。
        let digits_only = find.chars().all(|c| c.is_ascii_digit());
        if digits_only && find.len() >= 15 {
            warnings.push(format!(
                "第 {lineno} 行「查找内容」是 {} 位纯数字：Excel 存储层可能已把它截断（15 位以上会丢精度），请核对原值",
                find.len()
            ));
        }

        // XML 非法控制字符（多从别处复制粘贴混进来）：
        // - 在「查找内容」里 → 文档本身是合法 XML，正文不可能含这些字符，这条规则永远命中不了；
        // - 在「替换为」里 → 写入时会被强制丢弃（留着会产出打不开的文档），
        //   报告里显示的仍是规则原文，与产物实际内容不一致。
        // 两种情况都必须在装载时说清楚，不能等写入时静默吞掉。
        for (what, text) in [("查找内容", &find), ("替换为", &rule.replace)] {
            let bad = scan::illegal_xml_chars(text);
            if !bad.is_empty() {
                let names: Vec<String> =
                    bad.iter().map(|c| format!("U+{:04X}", *c as u32)).collect();
                warnings.push(format!(
                    "第 {lineno} 行「{what}」含 XML 非法控制字符（{}）：该字符无法写入文档，{}",
                    names.join("、"),
                    if what == "查找内容" {
                        "正文里不可能存在，这条规则将永远不命中"
                    } else {
                        "写入产物时会被丢弃，实际替换结果不含它"
                    },
                ));
            }
        }

        id += 1;
        out.push(rule);
    }

    Ok((out, warnings))
}

/// 从一整本 xlsx 读规则：**从第一张工作表起依次往下找，取第一张读得出条款的表**。
///
/// 为什么不再写死工作表名：用户手上的规则表不叫「规则」是常态
/// （`Sheet1`、`替换清单`、`2026-003Aa`…），为了能用先改表名是本末倒置。
/// 这里的规矩是：**表名只当提示，内容说了算**。
///
/// 挑法分**两轮**，顺序即优先级：
///
/// 1. **有表头、且表头里认得出列名**（如「查找内容 / 替换为」）的表——强信号；
/// 2. 都没有时，退回"第一张**像裸两列规则表**的表"——老式无表头的表靠这一轮。
///
/// 第二轮额外要求表里**至少有一行第 2 列有内容**：说明页、封面页往往只有第一列
/// 写着字（"本表怎么用"、"制表：张三"），少了这一条就会被它们抢走。
///
/// 为什么要分两轮：多工作表的工作簿里，第一张常常是「填写说明」——它没有表头，
/// 但第一列照样写着字，只按"读得出条款"挑就会把它读成规则。
///
/// 判定"这张表不行"：
/// - 解析成功但一条规则都没有（空表、只有表头）；
/// - 解析报错（首行像表头，却一个列名都认不出来）。
///
/// 两种都在本轮里跳过；只有当**所有**表都读不出时，才把每张表的原因一并报出来。
///
/// 返回 `(规则, 警告, 实际采用的工作表名)`。
pub fn from_xlsx_book(
    path: &Path,
    layout: &XlsxLayout,
    defaults: &Rule,
    start_id: u32,
) -> Result<(Vec<Rule>, Vec<String>, String)> {
    let sheets = crate::report::read_sheets(path)?;
    if sheets.is_empty() {
        bail!("{} 里没有任何工作表", path.display());
    }

    let parse = |name: &str, rows: &[Vec<String>]| {
        from_xlsx_rows(
            rows,
            layout,
            defaults,
            start_id,
            &format!("{}〔工作表「{name}」〕", path.display()),
        )
    };

    // 只有一张表：错误**原样抛出**。表头认不出时那条错误带着"列名该怎么写"的
    // 多行提示，压成一行就废了；而且单表也无所谓"往后找"。
    if sheets.len() == 1 {
        let (name, rows) = &sheets[0];
        let (rs, warns) = parse(name, rows)?;
        if rs.is_empty() {
            bail!(
                "{} 的工作表「{name}」里没有可用的替换条款\
                 （「查找内容」为空的行会被跳过，表头行也跳过）",
                path.display()
            );
        }
        return Ok((rs, warns, name.clone()));
    }

    // 每张表只解析一次，两轮挑选/报错都复用这份结果（不重复解析）
    let parsed: Vec<(String, bool, bool, Result<(Vec<Rule>, Vec<String>)>)> = sheets
        .iter()
        .map(|(name, rows)| {
            let has_head = rows.first().and_then(|r| header_positions(r)).is_some();
            // 「有没有第二列的内容」：无表头的兜底轮凭它认表——
            // 说明页 / 封面页通常只有第一列写字，靠这一条把它们挡在门外
            let has_replace_col = rows
                .iter()
                .any(|r| r.get(1).map(|s| !s.trim().is_empty()).unwrap_or(false));
            (name.clone(), has_head, has_replace_col, parse(name, rows))
        })
        .collect();

    // 第一轮：有表头的表优先（哪怕第 2 列整列留空，那也是用户写明的表）
    for (name, has_head, _, res) in &parsed {
        if !*has_head {
            continue;
        }
        if let Ok((rs, warns)) = res {
            if !rs.is_empty() {
                return Ok((rs.clone(), warns.clone(), name.clone()));
            }
        }
    }
    // 第二轮：放宽到"像裸两列规则表"的表
    for (name, _, has_replace_col, res) in &parsed {
        if !*has_replace_col {
            continue;
        }
        if let Ok((rs, warns)) = res {
            if !rs.is_empty() {
                return Ok((rs.clone(), warns.clone(), name.clone()));
            }
        }
    }

    let why: Vec<String> = parsed
        .iter()
        .map(|(name, _, has_replace_col, res)| match res {
            Ok(_) if !*has_replace_col => {
                format!("「{name}」：只有第 1 列有内容（没有「替换为」），不像规则表")
            }
            Ok(_) => format!("「{name}」：没有可用的替换条款"),
            Err(e) => format!("「{name}」：{}", one_line(&format!("{e:#}"))),
        })
        .collect();
    bail!(
        "{} 里没读出任何规则——工作表已按顺序逐张尝试:\n\
         先找有表头（列名认得出）的表；\n\
         再找第一张像「查找内容 / 替换为」两列的表。\n\
         逐张情况:\n  {}",
        path.display(),
        why.join("\n  ")
    )
}

/// 把多行错误压成一行：塞进"逐张表说明"的列表里用。
fn one_line(s: &str) -> String {
    s.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("；")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    fn defs() -> Rule {
        Rule::new(0, "", "")
    }

    #[test]
    fn headers_have_no_index_column() {
        let h = XlsxLayout::all().headers();
        assert_eq!(h[0], "查找内容", "第 1 列固定是查找内容");
        assert_eq!(h[1], "替换为", "第 2 列固定是替换为");
        assert!(
            !h.iter().any(|c| c == "序号" || c == "编号"),
            "不该再有编号列：{h:?}"
        );
        assert_eq!(h.len(), 2 + XLSX_OPTIONAL_HEADERS.len());
    }

    #[test]
    fn new_format_roundtrip() {
        let layout = XlsxLayout::all();
        let src = template_rules();
        let mut table = vec![layout.headers()];
        for r in &src {
            table.push(xlsx_row(&layout, r));
        }

        let (rs, warns) = from_xlsx_rows(&table, &layout, &defs(), 1, "往返").unwrap();
        assert!(warns.is_empty(), "不该有警告：{warns:?}");
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].find, src[0].find);
        assert_eq!(rs[0].replace, src[0].replace);
        assert!(!rs[0].enabled, "模板示例行默认不启用");
        assert_eq!(rs[1].scope, Scope::BODY_ONLY);
        assert_eq!(rs[1].note, src[1].note);
    }

    #[test]
    fn legacy_index_column_is_ignored() {
        // 旧表：第 1 列是「序号」——按标题名认列，它自然落空
        let t = rows(&[
            &["序号", "查找内容", "替换为", "作用域", "启用"],
            &["1", "AAA", "BBB", "正文", "是"],
        ]);
        let (rs, _) = from_xlsx_rows(&t, &XlsxLayout::all(), &defs(), 1, "旧表").unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].find, "AAA", "序号列不能被当成查找内容");
        assert_eq!(rs[0].replace, "BBB");
        assert_eq!(rs[0].scope, Scope::BODY_ONLY);
        assert!(rs[0].enabled);
    }

    #[test]
    fn headerless_two_column_table() {
        let t = rows(&[&["AAA", "BBB"], &["CCC", "DDD"]]);
        let (rs, warns) = from_xlsx_rows(&t, &XlsxLayout::all(), &defs(), 1, "两列").unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].find, "AAA");
        assert_eq!(rs[0].replace, "BBB");
        assert_eq!(rs[1].replace, "DDD");
        assert!(warns.is_empty(), "两列表不该报噪音：{warns:?}");
    }

    #[test]
    fn shuffled_columns_still_recognized() {
        let t = rows(&[
            &["启用", "作用域", "替换为", "查找内容"],
            &["是", "页眉页脚", "BBB", "AAA"],
        ]);
        let (rs, _) = from_xlsx_rows(&t, &XlsxLayout::all(), &defs(), 1, "乱序").unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].find, "AAA");
        assert_eq!(rs[0].replace, "BBB");
        assert_eq!(rs[0].scope.display(), "页眉页脚");
    }

    #[test]
    fn disabled_column_is_ignored() {
        let t = rows(&[
            &["查找内容", "替换为", "启用", "备注"],
            &["AAA", "BBB", "否", "别理我"],
        ]);
        let mut layout = XlsxLayout::all();
        layout.set("启用", false);
        layout.set("备注", false);
        let (rs, _) = from_xlsx_rows(&t, &layout, &defs(), 1, "关列").unwrap();
        assert_eq!(rs.len(), 1);
        assert!(rs[0].enabled, "「启用」列没参与 → 走默认值 true");
        assert!(rs[0].note.is_empty(), "「备注」列没参与 → 空");
        // 同一张表，列打开后语义就回来了
        let (rs2, _) = from_xlsx_rows(&t, &XlsxLayout::all(), &defs(), 1, "开列").unwrap();
        assert!(!rs2[0].enabled);
        assert_eq!(rs2[0].note, "别理我");
    }

    #[test]
    fn ui_columns_have_no_enable_switch() {
        // 界面不提供「启用」：规则既然写进表里，就是要替换的
        assert!(!XLSX_UI_HEADERS.contains(&"启用"));
        assert_eq!(XLSX_UI_HEADERS.len(), XLSX_OPTIONAL_HEADERS.len() - 1);
        for h in XLSX_UI_HEADERS {
            assert!(XLSX_OPTIONAL_HEADERS.contains(&h), "{h} 不在可选列里");
        }
    }

    #[test]
    fn for_read_still_sees_the_enabled_column() {
        // 界面勾选里没有「启用」，但表里带着「启用 = 否」时必须认出来 ——
        // 否则那行会被当成普通规则跑掉（本工具最不能犯的错）。
        let t = rows(&[&["查找内容", "替换为", "启用"], &["AAA", "BBB", "否"]]);
        // 界面勾选里没有「启用」（见 XLSX_UI_HEADERS），写表也就不写这一列
        let mut ui = XlsxLayout::default();
        ui.set("启用", false);
        let (rs, _) = from_xlsx_rows(&t, &ui.for_read(), &defs(), 1, "读表").unwrap();
        assert!(!rs[0].enabled, "for_read 必须认「启用」列");
        assert!(!ui.headers().iter().any(|h| h == "启用"));
    }

    #[test]
    fn empty_scope_is_detectable() {
        // 「正文,文件名」这类组合会被改名开关掐成空域，必须有办法识别出来
        let mut sc = Scope::parse("文件名").unwrap();
        assert!(!sc.is_empty());
        sc.filename = false;
        assert!(sc.is_empty());
    }

    #[test]
    fn unknown_header_is_an_error_not_a_rule() {
        // 「原内容 / 新内容」这种写法：必须报错，绝不能当成一条真规则读进去
        let t = rows(&[&["原内容", "新内容"], &["AAA", "BBB"]]);
        let e = from_xlsx_rows(&t, &XlsxLayout::all(), &defs(), 1, "怪表头").unwrap_err();
        let m = format!("{e:#}");
        assert!(m.contains("查找内容"), "错误信息要给出认得的列名：{m}");
    }
}
