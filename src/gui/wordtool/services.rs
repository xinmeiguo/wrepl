//! 两个业务服务：批量打印、批量导出 PDF。
//!
//! 2026-10-19：页眉替换的 COM 实现（文本 / 图片两条）已随 COM 引擎退役删除 ——
//! 页眉替换现在只有纯 Rust 一条路（`wrepl::hdr`），不再需要 Word。

use super::com::{Disp, DispResult, var_bool, var_bstr, var_i4};
use super::engine::WordKind;
use super::wd;
use super::word_session::{run_on_documents, run_on_documents_printing};
use std::path::{Path, PathBuf};
use windows::Win32::System::Variant::VARIANT;

// ==================== 批量打印 ====================

/// 批量打印：page_range 留空打印全部，否则按 "1-3,5,7-9" 格式打印指定页。
///
/// `printer` 是 Word 的 `ActivePrinter` 字符串（`"名称 on 端口"`，见
/// [`super::printers`]）；`None` = 用系统默认那台。
pub fn print_run(
    doc_paths: &[String],
    page_range: &str,
    printer: Option<&str>,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
) -> (i32, i32) {
    let pages = page_range.trim().to_string();
    run_on_documents_printing(
        doc_paths,
        |doc, _src| unsafe {
            // Background=false 同步打印，避免 Close 抢在打印完成前关闭文档
            if pages.is_empty() {
                doc.call_named("PrintOut", vec![("Background", var_bool(false))])?;
            } else {
                doc.call_named(
                    "PrintOut",
                    vec![
                        ("Background", var_bool(false)),
                        ("Range", var_i4(wd::PRINT_RANGE_OF_PAGES)),
                        ("Pages", var_bstr(&pages)),
                    ],
                )?;
            }
            Ok(None)
        },
        log,
        progress,
        false, // 打印不改动文档，无需保存
        "已发送打印",
        printer,
    )
}

// ==================== 导出 PDF ====================

/// 导出的页码范围。
///
/// **只支持"全部"或"一段连续区间"** —— 这不是偷懒：Word 的
/// `ExportAsFixedFormat` 一次只认一组 `From`/`To`，`"1-3,5"` 在接口层面
/// 就表达不出来。要多段只能导出多次再合并 PDF，而合并需要 PDF 库
/// （本项目零新依赖）。所以界面上宁可**明确拒绝**并说清楚，
/// 也不把 `1-3,5` 悄悄当成 `1-3` 或 `1-5`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PdfPages {
    All,
    FromTo(u32, u32),
}

impl PdfPages {
    /// 界面 / 日志用的人话。
    pub fn describe(self) -> String {
        match self {
            PdfPages::All => "全部页".to_string(),
            PdfPages::FromTo(a, b) if a == b => format!("第 {a} 页"),
            PdfPages::FromTo(a, b) => format!("第 {a}-{b} 页"),
        }
    }
}

/// 把界面上的「页码范围」解析成导出能吃的形状。留空 = 全部。
///
/// 认 `3`、`3-7`，也认全角连字符与 `~`（用户在中文输入法下很容易打出这两个）。
/// 不认逗号 / 分号 / 顿号 —— 见 [`PdfPages`] 上的说明。
pub fn parse_pdf_pages(raw: &str) -> Result<PdfPages, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(PdfPages::All);
    }
    if s.contains([',', '，', ';', '；', '、']) {
        return Err("导出 PDF 一次只能导一段连续页码 —— 要分段请删掉逗号，分几次导出".to_string());
    }
    let norm = s.replace(['－', '—', '–', '~', '～'], "-");
    let parts: Vec<&str> = norm.split('-').map(str::trim).collect();
    let num = |t: &str| -> Result<u32, String> {
        let v: u32 = t
            .parse()
            .map_err(|_| format!("「{raw}」读不出页码（要写成 5 或 3-7）"))?;
        if v == 0 {
            return Err(format!("页码从 1 开始，「{raw}」里出现了 0"));
        }
        Ok(v)
    };
    match parts.as_slice() {
        [one] => {
            let n = num(one)?;
            Ok(PdfPages::FromTo(n, n))
        }
        [a, b] => {
            let (a, b) = (num(a)?, num(b)?);
            if a > b {
                return Err(format!("起止页写反了：「{raw}」（应当写成 小-大）"));
            }
            Ok(PdfPages::FromTo(a, b))
        }
        _ => Err(format!("「{raw}」读不出页码范围（要写成 5 或 3-7）")),
    }
}

/// 导出目录的落点：给了就用它；留空则落**每份文档旁**的 `pdf/` 子目录
/// （与「页眉替换」默认写 `<父目录>/out` 同一个思路：产物分开放，不混进交付目录）。
pub fn pdf_target_dir(explicit: Option<&Path>, src: &str) -> PathBuf {
    match explicit {
        Some(d) => d.to_path_buf(),
        None => Path::new(src)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("pdf"),
    }
}

/// 产物名：源文件名换扩展名为 `.pdf`。
fn pdf_stem(src: &str) -> String {
    Path::new(src)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "document".to_string())
}

/// 在 `dir` 下挑一个不撞名的落点，返回 `(路径, 是否因撞名改过名)`。
///
/// 与「页眉替换」同一口径：**绝不覆盖已经存在的 PDF**。导出的 PDF 可能已经
/// 被人工批注过、或拿去合并过，覆盖等于不可逆地毁掉一份人工成果；
/// 而"再导一次"多出来的代价只是名字后面多个 `(2)`。
fn unique_target(dir: &Path, stem: &str) -> (PathBuf, bool) {
    let first = dir.join(format!("{stem}.pdf"));
    if !first.exists() {
        return (first, false);
    }
    for n in 2..10_000u32 {
        let p = dir.join(format!("{stem} ({n}).pdf"));
        if !p.exists() {
            return (p, true);
        }
    }
    (first, true)
}

/// 建目录并重试几次。
///
/// 本机实测过「同一个目录刚才建不出来、几分钟后就建得出来」的情况
/// （企业管控 / 杀软 / 同步盘的瞬时占用），所以这里退避重试 3 轮再放弃。
fn create_dir_retry(dir: &Path) -> std::io::Result<()> {
    let mut last = std::fs::create_dir_all(dir);
    let mut wait = 100u64;
    for _ in 0..3 {
        if last.is_ok() {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(wait));
        last = std::fs::create_dir_all(dir);
        wait *= 2;
    }
    last
}

/// 导出 PDF：`out_dir` 为 `None` 时落到各文档旁的 `pdf/`，给了就全部导到那一处。
///
/// `pdf_a` = 勾了「PDF/A 归档格式」→ `UseISO19005_1: True`。
/// ⚠ 实测（Microsoft 365，2026-10-19）：这个开关产出的是 **PDF/A-3b**
/// （XMP 里 `pdfaid:part=3 / conformance=B`），不是 ISO 19005-1 的 1b ——
/// 所以界面文案不写编号，免得承诺一个 Word 并不给的东西。
pub fn export_pdf_run(
    doc_paths: &[String],
    out_dir: Option<&Path>,
    pages: PdfPages,
    pdf_a: bool,
    kind: WordKind,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(usize, usize),
) -> (i32, i32) {
    // 显式给了目录：**动手前**建一次，建不出来就放弃整批。
    // 不这么做的话，一个建不出来的目录会顺着每份文档各记一条 ERROR，
    // 结论上只剩"N 个文件失败"，根因被埋掉（「批量替换」那边踩过一次）。
    if let Some(d) = out_dir {
        if let Err(e) = create_dir_retry(d) {
            log(format!(
                "输出目录建不出来：{}（{e}）—— 已放弃本次导出",
                d.display()
            ));
            return (0, doc_paths.len() as i32);
        }
    }
    // 降级是**使用者点过确认**才会走到这里的（见 `ui::ClickPlan`），
    // 但日志里也要留一条：事后翻日志的人得知道这批 PDF 是谁排的版。
    if kind == WordKind::Wps {
        log("这批 PDF 由 WPS 排版，分页与 Word 不同".to_string());
    }
    run_on_documents(
        kind,
        doc_paths,
        |doc, src| unsafe { export_one(doc, src, out_dir, pages, pdf_a) },
        log,
        progress,
        false, // 导出不改动文档，无需保存
        "已导出",
    )
}

unsafe fn export_one(
    doc: &Disp,
    src: &str,
    out_dir: Option<&Path>,
    pages: PdfPages,
    pdf_a: bool,
) -> DispResult<Option<String>> {
    unsafe {
        let dir = pdf_target_dir(out_dir, src);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("建不了目录 {}：{e}", dir.display()))?;
        let (target, collided) = unique_target(&dir, &pdf_stem(src));
        let out = target.to_string_lossy().to_string();

        let mut args: Vec<(&str, VARIANT)> = vec![
            ("OutputFileName", var_bstr(&out)),
            ("ExportFormat", var_i4(wd::EXPORT_FORMAT_PDF)),
            // ★ 必须显式关掉：这个参数默认是 True，Word 导出完会把 PDF 拉起来
            //   开一个窗口 —— 整批就此卡在"等人关窗"上，而且窗口不可见时更糟
            //   （看不见、关不掉，批次永远不结束）。
            ("OpenAfterExport", var_bool(false)),
            ("OptimizeFor", var_i4(wd::EXPORT_OPTIMIZE_FOR_PRINT)),
            ("UseISO19005_1", var_bool(pdf_a)),
        ];
        if let PdfPages::FromTo(a, b) = pages {
            args.push(("Range", var_i4(wd::EXPORT_RANGE_FROM_TO)));
            args.push(("From", var_i4(a as i32)));
            args.push(("To", var_i4(b as i32)));
        }
        doc.call_named("ExportAsFixedFormat", args)?;

        // ★★ Word 会把输出路径里的**空格**写成 `%20`（2026-10-19 实测）：
        //    传 `… FAT (2).pdf`，磁盘上落的是 `… FAT%20(2).pdf`。
        //    中文本身不受影响（实测「2026-005CE-FAT-方案」原样保留），所以这个坑
        //    只在"文件名带空格"时露头 —— 而交付件里空格恰恰很常见。
        //    不处理的话有两重后果：产物名字带 `%20`（人看着莫名其妙），
        //    以及下面那句产物校验会判它"没导出来"。
        //    ⇒ 先按 URL 编码形态找一次，找到就改名回本名。
        if !target.exists() {
            let encoded = PathBuf::from(out.replace(' ', "%20"));
            if encoded.exists() && std::fs::rename(&encoded, &target).is_ok() {
                // 改名成功就走下面的正常路径；失败则留给产物校验报错。
            }
        }

        // 不信返回值，验产物：Word 在某些失败路径上照样返回成功。
        if !target.exists() {
            return Err(format!("Word 报成功，但没找到产物：{}", target.display()));
        }
        Ok(Some(if collided {
            format!(" -> {}（同名已存在，未覆盖，改存这个）", target.display())
        } else {
            format!(" -> {}", target.display())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_empty_means_all() {
        assert_eq!(parse_pdf_pages("").unwrap(), PdfPages::All);
        assert_eq!(parse_pdf_pages("   ").unwrap(), PdfPages::All);
    }

    #[test]
    fn pages_single_and_range() {
        assert_eq!(parse_pdf_pages("5").unwrap(), PdfPages::FromTo(5, 5));
        assert_eq!(parse_pdf_pages("3-7").unwrap(), PdfPages::FromTo(3, 7));
        assert_eq!(parse_pdf_pages(" 3 - 7 ").unwrap(), PdfPages::FromTo(3, 7));
        // 中文输入法下最常见的两个连字符，认它们是因为用户真的会打出来
        assert_eq!(parse_pdf_pages("3～7").unwrap(), PdfPages::FromTo(3, 7));
        assert_eq!(parse_pdf_pages("3－7").unwrap(), PdfPages::FromTo(3, 7));
    }

    #[test]
    fn pages_reject_multi_ranges_explicitly() {
        let e = parse_pdf_pages("1-3,5").unwrap_err();
        assert!(e.contains("连续"), "必须说清只能导一段：{e}");
        assert!(parse_pdf_pages("1，3").is_err(), "全角逗号也算多段");
        assert!(parse_pdf_pages("1;3").is_err());
        assert!(parse_pdf_pages("1、3").is_err());
    }

    #[test]
    fn pages_reject_nonsense() {
        assert!(parse_pdf_pages("7-3").is_err(), "写反要报错，不能自动对调");
        assert!(parse_pdf_pages("0").is_err(), "页码从 1 开始");
        assert!(parse_pdf_pages("abc").is_err());
        assert!(parse_pdf_pages("1-2-3").is_err());
    }

    #[test]
    fn target_dir_defaults_next_to_source() {
        assert_eq!(
            pdf_target_dir(None, "D:/x/y/a.docx"),
            PathBuf::from("D:/x/y/pdf")
        );
        // 只给裸文件名（没有父目录）时落在当前目录，与"旁"一个意思
        assert_eq!(pdf_target_dir(None, "a.docx"), PathBuf::from("pdf"));
        assert_eq!(
            pdf_target_dir(Some(Path::new("E:/out")), "D:/x/a.docx"),
            PathBuf::from("E:/out")
        );
    }

    #[test]
    fn target_name_swaps_extension() {
        assert_eq!(pdf_stem("D:/x/标12-交接单.docx"), "标12-交接单");
        assert_eq!(pdf_stem("D:/x/a.doc"), "a");
        assert_eq!(pdf_stem("noext"), "noext");
    }

    /// 「已经有一份 PDF 就另起名字」是用户可见的硬保证 —— 导出的 PDF 常已被人工
    /// 批注过、或拿去合并过，覆盖等于不可逆地毁掉人工成果。所以这里真建目录、
    /// 真写文件，不做 mock。
    #[test]
    fn existing_pdf_is_never_overwritten() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("wrepl-pdf-target-{stamp}"));
        std::fs::create_dir_all(&dir).unwrap();
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();

        let (p0, dup0) = unique_target(&dir, "标12");
        assert_eq!(name(&p0), "标12.pdf");
        assert!(!dup0, "空目录里不该改名");
        let old = "人工批注过的旧件".as_bytes().to_vec();
        std::fs::write(&p0, &old).unwrap();

        let (p1, dup1) = unique_target(&dir, "标12");
        assert_eq!(name(&p1), "标12 (2).pdf");
        assert!(dup1, "撞名要报出来，界面据此提示用户");
        std::fs::write(&p1, b"x").unwrap();

        let (p2, _) = unique_target(&dir, "标12");
        assert_eq!(name(&p2), "标12 (3).pdf", "要一直往后找，不能停在 (2)");

        // 老件必须还在原处、内容一字未动
        assert_eq!(std::fs::read(&p0).unwrap(), old, "旧 PDF 被覆盖了");
    }
}
