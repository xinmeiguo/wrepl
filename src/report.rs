//! Excel 报告导出（里程碑 M6）与规则表读入（里程碑 M4）。
//!
//! ## 为什么手写 xlsx 而不用第三方 crate
//!
//! xlsx 就是一个 zip + 几个固定 XML。手写的好处：
//!
//! - **零新增依赖**：只要 `zip`（已在用），不动编译时间与体积
//! - **格式完全可控**：表头冻结、列宽、冲突行染黄，想怎么定就怎么定
//! - **读也不必引入 calamine**：规则表结构固定（第一个工作表、10 列），
//!   用已在用的 quick-xml 直接流式取单元格足够
//!
//! 单元格文本一律用 `inlineStr` 写入，省掉 sharedStrings 这一层。

use crate::docx::scan::escape_text;
use anyhow::{Context, Result, bail};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::io::Write;
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

/// 报告里的一行明细。
#[derive(Debug, Clone)]
pub struct HitRow {
    pub file: String,
    pub rule_id: u32,
    pub find: String,
    pub replace: String,
    pub slot: String,
    pub para: usize,
    pub matched: String,
    pub applied: bool,
    pub reason: String,
    pub strategy: String,
}

/// 报告里的一个文件。
#[derive(Debug, Clone)]
pub struct FileRow {
    pub file: String,
    pub path: String,
    pub size_kb: f64,
    pub mtime: String,
    pub rule_count: usize,
    pub total_hits: usize,
    pub applied: usize,
    pub conflicts: usize,
    pub status: String,
    pub sha_before: String,
    pub sha_after: String,
    pub parts_changed: String,
    pub note: String,
}

/// 报告元信息（封面用）。QA 归档时，一份报告必须能自证"谁在什么时候、
/// 用什么版本的规则、对哪些文件做了什么"，所以这些字段是必填的。
#[derive(Debug, Clone, Default)]
pub struct ReportMeta {
    pub tool: String,
    pub version: String,
    pub generated_at: String,
    pub operator: String,
    pub input: String,
    pub output: String,
    pub rule_source: String,
    pub mode: String,
    pub verdict: String,
}

/// 规则清单里的一行（规则快照）。
#[derive(Debug, Clone)]
pub struct RuleRow {
    pub id: u32,
    pub enabled: bool,
    pub find: String,
    pub replace: String,
    pub scope: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub kana_sensitive: bool,
    pub wildcard: bool,
    pub note: String,
}

/// 文件名对照：改名前后的映射。
///
/// 为什么必须进报告：文件名一改，归档时"报告里写的文件名"和"磁盘上的文件名"
/// 就对不上了。这张表就是那份对不上的帐。
#[derive(Debug, Clone)]
pub struct NameRow {
    pub index: usize,
    pub old_name: String,
    pub new_name: String,
    pub changed: bool,
    pub note: String,
}

/// 验证结论：残留自检 + 格式保全（关卡 1+2）。
#[derive(Debug, Clone)]
pub struct VerifyRow {
    pub file: String,
    pub level1: String,
    pub level2: String,
    pub residue: String,
    pub note: String,
}

impl VerifyRow {
    /// 通过 = 关卡 1 通过 + 关卡 2 通过 + 全包无残留。
    pub fn ok(&self) -> bool {
        self.level1 == "通过" && self.level2 == "通过" && self.residue == "无残留"
    }
}

const SUMMARY_HEADERS: &[&str] = &[
    "文件",
    "规则数",
    "总命中",
    "已替换",
    "冲突",
    "状态",
    "处理前 SHA256",
    "处理后 SHA256",
    "改动 part",
];

const DETAIL_HEADERS: &[&str] = &[
    "文件",
    "规则序号",
    "查找内容",
    "替换为",
    "位置类型",
    "段落序号",
    "命中文本",
    "是否替换",
    "原因",
    "改写方式",
];

const LIST_HEADERS: &[&str] = &["序号", "路径", "大小(KB)", "修改时间", "状态", "备注"];

const RULE_HEADERS: &[&str] = &[
    "序号", "启用", "查找内容", "替换为", "作用域", "区分大小写", "全字匹配", "全半角敏感",
    "通配符", "备注",
];

const NAME_HEADERS: &[&str] = &["序号", "处理前文件名", "处理后文件名", "是否改动", "备注"];

const VERIFY_HEADERS: &[&str] = &["文件", "关卡1 格式", "关卡2 骨架", "残留自检", "备注"];

/// 样式索引（与 `styles()` 里的 cellXfs 顺序严格对应）
const STYLE_HEADER: usize = 1;
const STYLE_WARN: usize = 2;
const STYLE_TITLE: usize = 3;
const STYLE_LABEL: usize = 4;
const STYLE_BODY: usize = 5;

const COVER_LABELS: &str = "报告信息";

/// 写出一份七表报告：报告信息（封面）/ 替换汇总 / 替换明细 / 文件清单 /
/// 文件名对照 / 验证结论 / 规则清单。
///
/// `names` 与 `verifies` 允许为空：为空时对应工作表里写一行说明，
/// 而不是留一张空白表——归档的人需要知道"这张表为什么是空的"。
#[allow(clippy::too_many_arguments)]
pub fn write_xlsx(
    path: &Path,
    meta: &ReportMeta,
    files: &[FileRow],
    hits: &[HitRow],
    rules: &[RuleRow],
    name_rows: &[NameRow],
    verify_rows: &[VerifyRow],
) -> Result<()> {
    // ── 封面 ──
    let total_hits: usize = files.iter().map(|f| f.total_hits).sum();
    let total_applied: usize = files.iter().map(|f| f.applied).sum();
    let total_conflicts: usize = files.iter().map(|f| f.conflicts).sum();
    let err_files = files
        .iter()
        .filter(|f| f.status == "ERROR")
        .count();
    let skipped_files = files
        .iter()
        .filter(|f| f.status == "SKIP")
        .count();
    let ok_files = files.len() - err_files - skipped_files;

    let cover = cover_sheet(
        meta,
        &[
            ("报告生成时间", meta.generated_at.clone()),
            ("工具", format!("{} v{}", meta.tool, meta.version)),
            ("操作者（登录账户）", meta.operator.clone()),
            ("运行模式", meta.mode.clone()),
            ("输入", meta.input.clone()),
            ("输出", meta.output.clone()),
            ("规则来源", meta.rule_source.clone()),
            ("规则条数", rules.len().to_string()),
            ("", String::new()),
            (
                "处理文件数",
                format!("共 {} 个（成功 {} / 跳过 {} / 失败 {}）", files.len(), ok_files, skipped_files, err_files),
            ),
            ("命中 / 已替换 / 冲突", format!("{total_hits} / {total_applied} / {total_conflicts}")),
            ("", String::new()),
            ("判定", meta.verdict.clone()),
        ],
    );

    let sheet_summary = sheet(
        &SUMMARY_HEADERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        files.iter().map(|f| {
            vec![
                Cell::text(&f.file),
                Cell::num(f.rule_count as f64),
                Cell::num(f.total_hits as f64),
                Cell::num(f.applied as f64),
                Cell::num(f.conflicts as f64),
                Cell::text(&f.status),
                Cell::text(&f.sha_before),
                Cell::text(&f.sha_after),
                Cell::text(&f.parts_changed),
            ]
        }),
        &[34.0, 9.0, 9.0, 9.0, 8.0, 10.0, 20.0, 20.0, 34.0],
        Some(STYLE_BODY),
    );

    let sheet_detail = sheet(
        &DETAIL_HEADERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        hits.iter().map(|h| {
            // 未替换（冲突/被跳过）的行染黄底，一眼看出需要人工复核
            let style = if h.applied { STYLE_BODY } else { STYLE_WARN };
            vec![
                Cell::styled(&h.file, style),
                Cell::num_styled(h.rule_id as f64, style),
                Cell::styled(&h.find, style),
                Cell::styled(&h.replace, style),
                Cell::styled(&h.slot, style),
                Cell::num_styled(h.para as f64, style),
                Cell::styled(&h.matched, style),
                Cell::styled(if h.applied { "是" } else { "否" }, style),
                Cell::styled(&h.reason, style),
                Cell::styled(&h.strategy, style),
            ]
        }),
        &[26.0, 9.0, 34.0, 34.0, 11.0, 9.0, 34.0, 9.0, 22.0, 24.0],
        Some(STYLE_BODY),
    );

    let sheet_list = sheet(
        &LIST_HEADERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        files.iter().enumerate().map(|(i, f)| {
            vec![
                Cell::num((i + 1) as f64),
                Cell::text(&f.path),
                Cell::text(&format!("{:.1}", f.size_kb)),
                Cell::text(&f.mtime),
                Cell::text(&f.status),
                Cell::text(&f.note),
            ]
        }),
        &[7.0, 62.0, 11.0, 20.0, 10.0, 46.0],
        Some(STYLE_BODY),
    );

    let sheet_rules = sheet(
        &RULE_HEADERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        rules.iter().map(|r| {
            let yn = |b: bool| if b { "是" } else { "否" };
            vec![
                Cell::num(r.id as f64),
                Cell::text(yn(r.enabled)),
                Cell::text(&r.find),
                Cell::text(&r.replace),
                Cell::text(&r.scope),
                Cell::text(yn(r.case_sensitive)),
                Cell::text(yn(r.whole_word)),
                Cell::text(yn(r.kana_sensitive)),
                Cell::text(yn(r.wildcard)),
                Cell::text(&r.note),
            ]
        }),
        &[7.0, 7.0, 42.0, 42.0, 12.0, 12.0, 10.0, 12.0, 9.0, 30.0],
        Some(STYLE_BODY),
    );

    let name_sheet = if name_rows.is_empty() {
        sheet(
            &NAME_HEADERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            vec![vec![
                Cell::text("—"),
                Cell::text("（本次未开启文件名同步改名）"),
                Cell::text(""),
                Cell::text("否"),
                Cell::text("如需文件名同步改动：命令行加 --rename-files，界面勾选「同步修改文件名」"),
            ]]
            .into_iter(),
            &[7.0, 46.0, 46.0, 10.0, 48.0],
            Some(STYLE_BODY),
        )
    } else {
        sheet(
            &NAME_HEADERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            name_rows.iter().map(|n| {
                vec![
                    Cell::num(n.index as f64),
                    Cell::text(&n.old_name),
                    Cell::text(&n.new_name),
                    Cell::text(if n.changed { "是" } else { "否" }),
                    Cell::text(&n.note),
                ]
            }),
            &[7.0, 46.0, 46.0, 10.0, 48.0],
            Some(STYLE_BODY),
        )
    };

    let verify_sheet = if verify_rows.is_empty() {
        sheet(
            &VERIFY_HEADERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            vec![vec![
                Cell::text("—"),
                Cell::text("（本次未开启执行后自动验证）"),
                Cell::text(""),
                Cell::text(""),
                Cell::text("如需自动验证：命令行加 --verify-after，界面勾选「执行后自动验证」"),
            ]]
            .into_iter(),
            &[40.0, 12.0, 12.0, 44.0, 46.0],
            Some(STYLE_BODY),
        )
    } else {
        sheet(
            &VERIFY_HEADERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            verify_rows.iter().map(|v| {
                let ok = v.level1 == "通过" && v.level2 == "通过" && v.residue == "无残留";
                let style = if ok { STYLE_BODY } else { STYLE_WARN };
                vec![
                    Cell::styled(&v.file, style),
                    Cell::styled(&v.level1, style),
                    Cell::styled(&v.level2, style),
                    Cell::styled(&v.residue, style),
                    Cell::styled(&v.note, style),
                ]
            }),
            &[40.0, 12.0, 12.0, 44.0, 46.0],
            Some(STYLE_BODY),
        )
    };

    let sheet_names: Vec<String> = vec![
        COVER_LABELS.into(),
        "替换汇总".into(),
        "替换明细".into(),
        "文件清单".into(),
        "文件名对照".into(),
        "验证结论".into(),
        "规则清单".into(),
    ];

    let parts: Vec<(String, String)> = vec![
        ("[Content_Types].xml".into(), content_types_n(7)),
        ("_rels/.rels".into(), root_rels()),
        ("xl/workbook.xml".into(), workbook_n(&sheet_names)),
        ("xl/_rels/workbook.xml.rels".into(), workbook_rels_n(7)),
        ("xl/styles.xml".into(), styles()),
        ("xl/worksheets/sheet1.xml".into(), cover),
        ("xl/worksheets/sheet2.xml".into(), sheet_summary),
        ("xl/worksheets/sheet3.xml".into(), sheet_detail),
        ("xl/worksheets/sheet4.xml".into(), sheet_list),
        ("xl/worksheets/sheet5.xml".into(), name_sheet),
        ("xl/worksheets/sheet6.xml".into(), verify_sheet),
        ("xl/worksheets/sheet7.xml".into(), sheet_rules),
    ];

    write_zip(path, parts)
}

/// 封面表：A 列标签、B 列内容，A1 跨列合并作标题。
fn cover_sheet(meta: &ReportMeta, rows: &[(&str, String)]) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">",
    );
    x.push_str("<sheetViews><sheetView workbookViewId=\"0\" showGridLines=\"0\"/></sheetViews>");
    x.push_str("<cols><col min=\"1\" max=\"1\" width=\"24\" customWidth=\"1\"/>\
                <col min=\"2\" max=\"2\" width=\"86\" customWidth=\"1\"/></cols>");
    x.push_str("<sheetData>");

    // 标题行（A1 与 B1 合并）
    x.push_str(&format!(
        "<row r=\"1\" ht=\"30\" customHeight=\"1\">\
         <c r=\"A1\" s=\"{T}\" t=\"inlineStr\"><is><t>{title}</t></is></c>\
         <c r=\"B1\" s=\"{T}\"/></row>",
        T = STYLE_TITLE,
        title = escape_text(&format!("{} v{}　Word 批量替换报告", meta.tool, meta.version))
    ));

    for (i, (k, v)) in rows.iter().enumerate() {
        let r = i + 2;
        if k.is_empty() {
            x.push_str(&format!("<row r=\"{r}\"/>"));
            continue;
        }
        x.push_str(&format!(
            "<row r=\"{r}\">\
             <c r=\"A{r}\" s=\"{L}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{k}</t></is></c>\
             <c r=\"B{r}\" s=\"{B}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{v}</t></is></c>\
             </row>",
            L = STYLE_LABEL,
            B = STYLE_BODY,
            k = escape_text(k),
            v = escape_text(v)
        ));
    }

    x.push_str("</sheetData>");
    x.push_str("<mergeCells count=\"1\"><mergeCell ref=\"A1:B1\"/></mergeCells>");
    x.push_str("</worksheet>");
    x
}

/// 一个单元格。
pub struct Cell {
    pub value: String,
    pub numeric: bool,
    /// `None` = 沿用所在表的默认正文样式
    pub style: Option<usize>,
}

impl Cell {
    pub fn text(s: &str) -> Self {
        Cell {
            value: s.to_string(),
            numeric: false,
            style: None,
        }
    }
    pub fn styled(s: &str, style: usize) -> Self {
        Cell {
            value: s.to_string(),
            numeric: false,
            style: Some(style),
        }
    }
    pub fn num(v: f64) -> Self {
        Cell {
            value: format!("{v}"),
            numeric: true,
            style: None,
        }
    }
    pub fn num_styled(v: f64, style: usize) -> Self {
        Cell {
            value: format!("{v}"),
            numeric: true,
            style: Some(style),
        }
    }
}

/// 写一个只含单个工作表的 xlsx（规则模板用）。
pub fn write_single_sheet(
    path: &Path,
    sheet_name: &str,
    headers: &[String],
    rows: Vec<Vec<Cell>>,
) -> Result<()> {
    let widths: Vec<f64> = headers
        .iter()
        .map(|_| 18.0)
        .collect();
    let body = sheet(headers, rows.into_iter(), &widths, Some(STYLE_BODY));
    let parts: Vec<(String, String)> = vec![
        ("[Content_Types].xml".into(), content_types_n(1)),
        ("_rels/.rels".into(), root_rels()),
        (
            "xl/workbook.xml".into(),
            workbook_n(&[sheet_name.to_string()]),
        ),
        (
            "xl/_rels/workbook.xml.rels".into(),
            workbook_rels_n(1),
        ),
        ("xl/styles.xml".into(), styles()),
        ("xl/worksheets/sheet1.xml".into(), body),
    ];
    write_zip(path, parts)
}

fn write_zip(path: &Path, parts: Vec<(String, String)>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败：{}", parent.display()))?;
        }
    }
    let out = std::fs::File::create(path)
        .with_context(|| format!("创建 xlsx 失败：{}", path.display()))?;
    let mut zw = ZipWriter::new(std::io::BufWriter::new(out));
    let opt = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in parts {
        zw.start_file(name.clone(), opt)
            .with_context(|| format!("写入 {name} 失败"))?;
        zw.write_all(body.as_bytes())
            .with_context(|| format!("写入 {name} 内容失败"))?;
    }
    let mut bw = zw.finish().context("收尾 xlsx 失败")?;
    bw.flush().context("写 xlsx 到磁盘失败")?;
    Ok(())
}

/// 列号 → Excel 列名（0 → A）。
fn col_name(mut i: usize) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (i % 26) as u8) as char);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s
}

fn sheet(
    headers: &[String],
    rows: impl Iterator<Item = Vec<Cell>>,
    widths: &[f64],
    body_style: Option<usize>,
) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">",
    );
    // 冻结首行
    x.push_str(
        "<sheetViews><sheetView workbookViewId=\"0\">\
         <pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/>\
         </sheetView></sheetViews>",
    );
    // 列宽
    x.push_str("<cols>");
    for i in 0..headers.len() {
        let w = widths.get(i).copied().unwrap_or(18.0);
        x.push_str(&format!(
            "<col min=\"{}\" max=\"{}\" width=\"{}\" customWidth=\"1\"/>",
            i + 1,
            i + 1,
            w
        ));
    }
    x.push_str("</cols><sheetData>");

    x.push_str("<row r=\"1\" ht=\"20\" customHeight=\"1\">");
    for (i, h) in headers.iter().enumerate() {
        x.push_str(&format!(
            "<c r=\"{c}1\" s=\"{S}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{v}</t></is></c>",
            c = col_name(i),
            S = STYLE_HEADER,
            v = escape_text(h)
        ));
    }
    x.push_str("</row>");

    for (ri, row) in rows.enumerate() {
        let r = ri + 2;
        x.push_str(&format!("<row r=\"{r}\">"));
        for (ci, cell) in row.iter().enumerate() {
            let addr = format!("{}{}", col_name(ci), r);
            let s = match cell.style.or(body_style) {
                Some(n) => format!(" s=\"{n}\""),
                None => String::new(),
            };
            if cell.numeric && !cell.value.is_empty() {
                x.push_str(&format!("<c r=\"{addr}\"{s}><v>{}</v></c>", cell.value));
            } else {
                x.push_str(&format!(
                    "<c r=\"{addr}\"{s} t=\"inlineStr\"><is><t xml:space=\"preserve\">{}</t></is></c>",
                    escape_text(&cell.value)
                ));
            }
        }
        x.push_str("</row>");
    }

    x.push_str("</sheetData></worksheet>");
    x
}

fn content_types_n(sheets: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
         <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>",
    );
    for i in 1..=sheets {
        s.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{i}.xml\" \
             ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        ));
    }
    s.push_str(
        "<Override PartName=\"/xl/styles.xml\" \
         ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\
         </Types>",
    );
    s
}

fn root_rels() -> String {
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
     <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
     <Relationship Id=\"rId1\" \
     Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" \
     Target=\"xl/workbook.xml\"/></Relationships>"
        .to_string()
}

fn workbook_n(names: &[String]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
         <sheets>",
    );
    for (i, n) in names.iter().enumerate() {
        s.push_str(&format!(
            "<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rId{}\"/>",
            escape_text(n),
            i + 1,
            i + 1
        ));
    }
    s.push_str("</sheets></workbook>");
    s
}

fn workbook_rels_n(sheets: usize) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for i in 1..=sheets {
        s.push_str(&format!(
            "<Relationship Id=\"rId{i}\" \
             Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" \
             Target=\"worksheets/sheet{i}.xml\"/>"
        ));
    }
    s.push_str(&format!(
        "<Relationship Id=\"rId{}\" \
         Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" \
         Target=\"styles.xml\"/></Relationships>",
        sheets + 1
    ));
    s
}

fn styles() -> String {
    // 样式索引（与 sheet() / cover_sheet() 里的常量对应）：
    //   0 默认　1 表头　2 待复核黄底　3 标题　4 标签　5 正文（带边框）
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
     <styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
     <fonts count=\"5\">\
     <font><sz val=\"10\"/><name val=\"微软雅黑\"/></font>\
     <font><b/><sz val=\"10\"/><color rgb=\"FFFFFFFF\"/><name val=\"微软雅黑\"/></font>\
     <font><b/><sz val=\"15\"/><color rgb=\"FF1F4E79\"/><name val=\"微软雅黑\"/></font>\
     <font><b/><sz val=\"10\"/><color rgb=\"FF1F4E79\"/><name val=\"微软雅黑\"/></font>\
     <font><sz val=\"10\"/><color rgb=\"FF9C5700\"/><name val=\"微软雅黑\"/></font>\
     </fonts>\
     <fills count=\"5\">\
     <fill><patternFill patternType=\"none\"/></fill>\
     <fill><patternFill patternType=\"gray125\"/></fill>\
     <fill><patternFill patternType=\"solid\"><fgColor rgb=\"FF1F4E79\"/><bgColor indexed=\"64\"/></patternFill></fill>\
     <fill><patternFill patternType=\"solid\"><fgColor rgb=\"FFFFF2CC\"/><bgColor indexed=\"64\"/></patternFill></fill>\
     <fill><patternFill patternType=\"solid\"><fgColor rgb=\"FFEAF1F8\"/><bgColor indexed=\"64\"/></patternFill></fill>\
     </fills>\
     <borders count=\"2\">\
     <border><left/><right/><top/><bottom/><diagonal/></border>\
     <border>\
     <left style=\"thin\"><color rgb=\"FFB4C6E7\"/></left>\
     <right style=\"thin\"><color rgb=\"FFB4C6E7\"/></right>\
     <top style=\"thin\"><color rgb=\"FFB4C6E7\"/></top>\
     <bottom style=\"thin\"><color rgb=\"FFB4C6E7\"/></bottom>\
     <diagonal/></border>\
     </borders>\
     <cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
     <cellXfs count=\"6\">\
     <xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>\
     <xf numFmtId=\"0\" fontId=\"1\" fillId=\"2\" borderId=\"1\" xfId=\"0\" applyFont=\"1\" applyFill=\"1\" applyBorder=\"1\" applyAlignment=\"1\"><alignment horizontal=\"center\" vertical=\"center\" wrapText=\"1\"/></xf>\
     <xf numFmtId=\"0\" fontId=\"4\" fillId=\"3\" borderId=\"1\" xfId=\"0\" applyFont=\"1\" applyFill=\"1\" applyBorder=\"1\" applyAlignment=\"1\"><alignment vertical=\"top\" wrapText=\"1\"/></xf>\
     <xf numFmtId=\"0\" fontId=\"2\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyFont=\"1\" applyAlignment=\"1\"><alignment horizontal=\"left\" vertical=\"center\"/></xf>\
     <xf numFmtId=\"0\" fontId=\"3\" fillId=\"4\" borderId=\"1\" xfId=\"0\" applyFont=\"1\" applyFill=\"1\" applyBorder=\"1\" applyAlignment=\"1\"><alignment vertical=\"center\" wrapText=\"1\"/></xf>\
     <xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"1\" xfId=\"0\" applyBorder=\"1\" applyAlignment=\"1\"><alignment vertical=\"top\" wrapText=\"1\"/></xf>\
     </cellXfs>\
     <cellStyles count=\"1\"><cellStyle name=\"常规\" xfId=\"0\" builtinId=\"0\"/></cellStyles>\
     </styleSheet>"
        .to_string()
}

/// 打开的 xlsx：zip 句柄 + 共享字符串 + `(工作表名, 内部 part 路径)` 列表。
///
/// 一次打开、多处读取：读一本工作簿要碰 workbook.xml / rels / sharedStrings /
/// 若干 worksheet 四类 part，每次都重新开一遍 zip 是白费。
struct Book {
    zip: zip::ZipArchive<std::io::BufReader<std::fs::File>>,
    shared: Vec<String>,
    /// 顺序＝`xl/workbook.xml` 里的先后顺序（**不是** zip 内 sheet1.xml/sheet2.xml 的顺序，
    /// 两者可以不一致；用户看到的是前者）。
    sheets: Vec<(String, String)>,
}

fn open_book(path: &Path) -> Result<Book> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("打不开 xlsx：{}", path.display()))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("不是合法的 xlsx：{}", path.display()))?;

    let wb = read_zip_str(&mut zip, "xl/workbook.xml")?;
    let rels = read_zip_str(&mut zip, "xl/_rels/workbook.xml.rels")?;
    let sheets = sheet_targets(&wb, &rels)?;

    let shared = match read_zip_str(&mut zip, "xl/sharedStrings.xml") {
        Ok(s) => parse_shared_strings(&s)?,
        Err(_) => Vec::new(),
    };

    Ok(Book {
        zip,
        shared,
        sheets,
    })
}

/// 按名字取某一张工作表的内容（逐行、逐列）。
///
/// 不用 calamine 的原因：结构固定，quick-xml 流式取单元格足够，
/// 且能顺手处理 `inlineStr` 与 `sharedStrings` 两种写法。
pub fn read_sheet(path: &Path, sheet_name: &str) -> Result<Vec<Vec<String>>> {
    let mut book = open_book(path)?;
    let target = match book.sheets.iter().find(|(n, _)| n == sheet_name) {
        Some((_, t)) => t.clone(),
        None => bail!(
            "xlsx 里没有名为 `{sheet_name}` 的工作表（现有：{}）",
            sheet_names_text(&book.sheets)
        ),
    };
    let body = read_zip_str(&mut book.zip, &target)?;
    parse_sheet(&body, &book.shared)
}

/// 读**全部**工作表，按 `xl/workbook.xml` 里的先后顺序返回 `(表名, 行)`。
///
/// 给「规则表不必叫『规则』」用：上层从第一张开始试着读规则，读不出就换下一张
/// （见 `rules::from_xlsx_book`）。读不出来或本来就没有单元格的表（图表页、空表）
/// 以**空行集**返回——判断"这张表有没有条款"是上层的活，这里不替它下结论。
pub fn read_sheets(path: &Path) -> Result<Vec<(String, Vec<Vec<String>>)>> {
    let mut book = open_book(path)?;
    let mut out = Vec::with_capacity(book.sheets.len());
    for (name, target) in book.sheets.clone() {
        let rows = match read_zip_str(&mut book.zip, &target) {
            Ok(body) => parse_sheet(&body, &book.shared).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        out.push((name, rows));
    }
    Ok(out)
}

fn sheet_names_text(sheets: &[(String, String)]) -> String {
    if sheets.is_empty() {
        "无".to_string()
    } else {
        sheets
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>()
            .join("、")
    }
}

fn read_zip_str(zip: &mut zip::ZipArchive<std::io::BufReader<std::fs::File>>, name: &str) -> Result<String> {
    use std::io::Read;
    let mut f = zip
        .by_name(name)
        .with_context(|| format!("xlsx 内缺少 {name}"))?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .with_context(|| format!("{name} 不是 UTF-8 文本"))?;
    Ok(s)
}

/// 由 `xl/workbook.xml` + rels 解出 `(工作表名, 内部 part 路径)`，**保持文档顺序**。
fn sheet_targets(wb: &str, rels: &str) -> Result<Vec<(String, String)>> {
    // 1. 工作表名 → rId（workbook.xml 里出现的先后顺序就是用户看到的标签顺序）
    let mut reader = Reader::from_str(wb);
    let mut list: Vec<(String, String)> = Vec::new();
    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                if e.local_name().as_ref() == "sheet" {
                    let mut name = None;
                    let mut id = None;
                    for a in e.attributes().flatten() {
                        let k: &str = a.key.as_ref();
                        let v: &str = a.value.as_ref();
                        if k == "name" {
                            name = Some(v.to_string());
                        }
                        if k.ends_with("id") && k.contains(':') {
                            id = Some(v.to_string());
                        }
                    }
                    if let (Some(n), Some(i)) = (name, id) {
                        list.push((n, i));
                    }
                }
            }
            _ => {}
        }
    }
    if list.is_empty() {
        bail!("xlsx 的 xl/workbook.xml 里没有任何工作表");
    }

    // 2. rId → Target
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut reader = Reader::from_str(rels);
    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                if e.local_name().as_ref() == "Relationship" {
                    let mut id = None;
                    let mut target = None;
                    for a in e.attributes().flatten() {
                        let k: &str = a.key.as_ref();
                        let v: &str = a.value.as_ref();
                        if k == "Id" {
                            id = Some(v.to_string());
                        }
                        if k == "Target" {
                            target = Some(v.to_string());
                        }
                    }
                    if let (Some(i), Some(t)) = (id, target) {
                        map.insert(i, sheet_part(&t));
                    }
                }
            }
            _ => {}
        }
    }

    // 3. 拼起来。关系缺失的表照留（路径为空 → 上层当空表跳过），
    //    不整本报错：一本工作簿里混着图表页、宏表是常态，不该因此读不了规则。
    Ok(list
        .into_iter()
        .map(|(n, i)| {
            let t = map.get(&i).cloned().unwrap_or_default();
            (n, t)
        })
        .collect())
}

/// 关系里的 `Target` → zip 内的 part 路径。
fn sheet_part(target: &str) -> String {
    let t = target.trim_start_matches('/');
    if t.starts_with("xl/") {
        t.to_string()
    } else {
        format!("xl/{t}")
    }
}

/// 解析 `sharedStrings.xml`。
fn parse_shared_strings(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) => {
                if e.local_name().as_ref() == "si" {
                    cur = Some(String::new());
                }
            }
            Event::Text(e) => {
                if let Some(c) = cur.as_mut() {
                    c.push_str(&decode_text(e.into_inner()));
                }
            }
            Event::GeneralRef(e) => {
                if let Some(c) = cur.as_mut() {
                    c.push_str(&resolve_ref(&e));
                }
            }
            Event::End(e) => {
                if e.local_name().as_ref() == "si" {
                    out.push(cur.take().unwrap_or_default());
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// 把转义态文本解码成普通文本（实体失败时退回原文，不丢内容）。
fn decode_text(raw: std::borrow::Cow<'_, str>) -> String {
    match quick_xml::escape::unescape(&raw) {
        Ok(c) => c.into_owned(),
        Err(_) => raw.into_owned(),
    }
}

/// 解析一个引用事件（`&#nnn;` / `&amp;`）为字符。
///
/// **必须显式处理**：quick-xml 把引用单独发成 `Event::GeneralRef`，
/// 不属于 `Event::Text`。漏掉的话，凡是把非 ASCII 写成字符引用的 xlsx
/// ——openpyxl 就是这种写法（`<t>&#36149;&#24030;</t>`）——整列中文会被静默读成空。
/// 实测后果：3 条规则只解析出 1 条，且不报任何错。
fn resolve_ref(e: &quick_xml::events::BytesRef<'_>) -> String {
    match e.resolve_char_ref() {
        // `&#nnn;` / `&#xNN;`
        Ok(Some(c)) => c.to_string(),
        _ => {
            // 命名实体：只认 XML 五个预定义，其余按原文保留（不猜、不丢）
            let name = e.clone().into_inner();
            match name.as_ref() {
                "amp" => "&".to_string(),
                "lt" => "<".to_string(),
                "gt" => ">".to_string(),
                "quot" => "\"".to_string(),
                "apos" => "'".to_string(),
                other => format!("&{other};"),
            }
        }
    }
}

/// 解析工作表：返回按行、按列展开的文本（缺失单元格补空串）。
fn parse_sheet(xml: &str, shared: &[String]) -> Result<Vec<Vec<String>>> {
    let mut reader = Reader::from_str(xml);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Option<Vec<String>> = None;
    let mut cell_ref: Option<String> = None;
    let mut cell_type: Option<String> = None;
    let mut cell_text = String::new();
    let mut in_value = false;

    let flush = |row: &mut Option<Vec<String>>, r: &Option<String>, t: &str| {
        if let (Some(v), Some(reff)) = (row.as_mut(), r.as_ref()) {
            let col = col_index(reff);
            while v.len() <= col {
                v.push(String::new());
            }
            v[col] = t.to_string();
        }
    };

    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) => {
                let n = e.local_name().as_ref().to_string();
                match n.as_str() {
                    "row" => row = Some(Vec::new()),
                    "c" => {
                        cell_ref = None;
                        cell_type = None;
                        cell_text.clear();
                        for a in e.attributes().flatten() {
                            let k: &str = a.key.as_ref();
                            let v: &str = a.value.as_ref();
                            if k == "r" {
                                cell_ref = Some(v.to_string());
                            }
                            if k == "t" {
                                cell_type = Some(v.to_string());
                            }
                        }
                    }
                    "v" | "t" => in_value = true,
                    _ => {}
                }
            }
            Event::Text(e) => {
                if in_value {
                    cell_text.push_str(&decode_text(e.into_inner()));
                }
            }
            Event::GeneralRef(e) => {
                if in_value {
                    cell_text.push_str(&resolve_ref(&e));
                }
            }
            Event::End(e) => {
                let n = e.local_name().as_ref().to_string();
                match n.as_str() {
                    "v" | "t" => in_value = false,
                    "c" => {
                        let text = match cell_type.as_deref() {
                            // 共享字符串：值是 sharedStrings 里的下标
                            Some("s") => cell_text
                                .trim()
                                .parse::<usize>()
                                .ok()
                                .and_then(|i| shared.get(i).cloned())
                                .unwrap_or_default(),
                            Some("b") => {
                                if cell_text.trim() == "1" {
                                    "TRUE".into()
                                } else {
                                    "FALSE".into()
                                }
                            }
                            _ => cell_text.clone(),
                        };
                        flush(&mut row, &cell_ref, &text);
                    }
                    "row" => {
                        if let Some(r) = row.take() {
                            rows.push(r);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(rows)
}

/// `B7` → 列下标 1。
fn col_index(reff: &str) -> usize {
    let mut v = 0usize;
    for c in reff.chars() {
        if c.is_ascii_alphabetic() {
            v = v * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1);
        } else {
            break;
        }
    }
    v.saturating_sub(1)
}
