//! 侦察：扫一个目录，列出里面**实际出现**的候选「项目编号」与候选「客户名」。
//!
//! 目的很实际：每换一个项目就重写一遍侦察脚本太蠢。界面里点一下「侦察目录」，
//! 就能从真实内容里挑出要替换什么，而不是凭空想源串——**猜源串是这类活里
//! 最容易出事故的一步**（猜短了改出半截，猜长了匹配不到）。
//!
//! ## 为什么不引 `regex`
//!
//! 依赖表里没有它。为这一个功能拖进一个大依赖，还得连带处理它的
//! Unicode 特性开关，不划算。这里要识别的模式很窄，手写字符扫描更可控。
//!
//! ## 覆盖面
//!
//! - 正文 / 页眉页脚 / 文本框 / 脚注 / 批注（走同一套扫描器，**跨 run 拼接后**再匹配）
//! - 文件名
//! - `docProps/*.xml`（标题、主题里常藏着客户名与编号；粗暴去标签后一起扫）

use crate::docx::package;
use crate::docx::scan;
use crate::par::parallel_map;
use anyhow::Result;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 一个候选串。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub text: String,
    /// 出现总次数
    pub count: usize,
    /// 出现在多少个文件里
    pub files: usize,
    /// 首次出现的位置提示
    pub first_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProbeReport {
    pub files_scanned: usize,
    pub skipped: Vec<(PathBuf, String)>,
    /// 正文里出现的项目编号
    pub codes_in_text: Vec<Candidate>,
    /// 文件名里出现的项目编号
    pub codes_in_names: Vec<Candidate>,
    /// 中文客户名候选
    pub names_cn: Vec<Candidate>,
    /// 英文客户名候选
    pub names_en: Vec<Candidate>,
}

impl ProbeReport {
    /// 开箱即用的规则建议（编号 + 中英客户名，按出现文件数降序）。
    pub fn suggested(&self) -> Vec<(String, &'static str)> {
        let mut v = Vec::new();
        for c in &self.codes_in_text {
            v.push((c.text.clone(), "项目编号"));
        }
        for c in &self.names_cn {
            v.push((c.text.clone(), "客户名(中文)"));
        }
        for c in &self.names_en {
            v.push((c.text.clone(), "客户名(英文)"));
        }
        v
    }
}

// ─────────────────────────────── 聚合 ───────────────────────────────

#[derive(Default)]
struct Agg {
    count: BTreeMap<String, usize>,
    files: BTreeMap<String, HashSet<usize>>,
    first: BTreeMap<String, String>,
}

impl Agg {
    fn add(&mut self, s: &str, fi: usize, hint: &str) {
        *self.count.entry(s.to_string()).or_insert(0) += 1;
        self.files.entry(s.to_string()).or_default().insert(fi);
        self.first
            .entry(s.to_string())
            .or_insert_with(|| hint.to_string());
    }

    fn finish(self) -> Vec<Candidate> {
        let Agg { count, files, first } = self;
        let mut v: Vec<Candidate> = count
            .into_iter()
            .map(|(text, count)| Candidate {
                files: files.get(&text).map(|s| s.len()).unwrap_or(0),
                first_at: first.get(&text).cloned().unwrap_or_default(),
                text,
                count,
            })
            .collect();
        v.sort_by(|a, b| {
            b.files
                .cmp(&a.files)
                .then(b.count.cmp(&a.count))
                .then(a.text.cmp(&b.text))
        });
        v
    }
}

// ─────────────────────────── 模式识别（手写扫描）───────────────────────────

/// 项目编号：4 位年份 + `-` + 3 位流水 + 可选 1~4 位字母数字。
///
/// 覆盖 `2026-001CE`、`2024-006CE`、`2025-004Da`、`2025-005AUTc` 这些实际出现过的形态。
fn find_codes(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < c.len() {
        if !c[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // 年份：正好 4 位数字
        let y0 = i;
        let mut j = i;
        while j < c.len() && c[j].is_ascii_digit() {
            j += 1;
        }
        if j - y0 != 4 || j >= c.len() || c[j] != '-' {
            i = j.max(i + 1);
            continue;
        }
        // 流水：正好 3 位数字
        let k0 = j + 1;
        let mut k = k0;
        while k < c.len() && c[k].is_ascii_digit() {
            k += 1;
        }
        if k - k0 != 3 {
            i = k.max(i + 1);
            continue;
        }
        // 后缀：最多 4 位字母数字
        let mut m = k;
        while m < c.len() && c[m].is_ascii_alphanumeric() && m - k < 4 {
            m += 1;
        }
        // 右边界：后面不能再接字母数字（否则是更长串的一部分，不完整）
        let right_ok = m >= c.len() || !c[m].is_ascii_alphanumeric();
        if right_ok {
            out.push(c[y0..m].iter().collect());
            i = m;
        } else {
            i = m.max(i + 1);
        }
    }
    out
}

fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
}

/// 中文机构名：以常见后缀收尾，向前扩展到非中文字符为止。
fn find_cn_names(s: &str) -> Vec<String> {
    const SUF: &[&str] = &[
        "有限责任公司",
        "股份有限公司",
        "有限公司",
        "集团公司",
        "研究所",
        "研究院",
        "集团",
        "公司",
    ];
    let c: Vec<char> = s.chars().collect();
    let mut covered = vec![false; c.len()];
    let mut out = Vec::new();

    for suf in SUF {
        let sc: Vec<char> = suf.chars().collect();
        let mut i = 0usize;
        while i + sc.len() <= c.len() {
            if c[i..i + sc.len()] != sc[..] {
                i += 1;
                continue;
            }
            let end = i + sc.len();
            if covered[i..end].iter().any(|x| *x) {
                i = end;
                continue;
            }
            // 向前扩展
            let mut start = i;
            let mut n = 0usize;
            while start > 0 && n < 24 {
                let ch = c[start - 1];
                if is_cjk(ch) {
                    start -= 1;
                    n += 1;
                } else {
                    break;
                }
            }
            let name: String = c[start..end].iter().collect();
            let cnt = name.chars().count();
            // 至少要 "XX公司" 这个量级，且必须真的含中文
            if cnt >= 4 && name.chars().any(is_cjk) {
                for x in covered.iter_mut().take(end).skip(start) {
                    *x = true;
                }
                out.push(name);
            }
            i = end;
        }
    }
    out
}

/// 英文机构名：以常见后缀收尾，向前扩展到 ASCII 词字符为止。
fn find_en_names(s: &str) -> Vec<String> {
    const SUF: &[&str] = &[
        "Co., Ltd.",
        "Co.,Ltd.",
        "Co. Ltd.",
        "Pvt. Ltd.",
        "Limited",
        "Ltd.",
        "Inc.",
        "LLC",
        "GmbH",
        "S.p.A.",
        "S.A.",
    ];
    // 长的优先，避免 "Ltd." 抢先吃掉 "Co., Ltd." 的尾巴
    let mut sufs: Vec<&str> = SUF.to_vec();
    sufs.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));

    let c: Vec<char> = s.chars().collect();
    let mut covered = vec![false; c.len()];
    let mut out = Vec::new();

    let stoppable = |ch: char| -> bool {
        is_cjk(ch) || matches!(ch, '\n' | '\r' | '\t' | ';' | ':' | '|' | '（' | '）' | '，' | '。')
    };
    let name_char = |ch: char| -> bool {
        ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '&' | '-' | '\'' | '(' | ')' | '.' | ',')
    };

    for suf in sufs {
        let sc: Vec<char> = suf.chars().collect();
        let mut i = 0usize;
        while i + sc.len() <= c.len() {
            if c[i..i + sc.len()] != sc[..] {
                i += 1;
                continue;
            }
            let end = i + sc.len();
            if covered[i..end].iter().any(|x| *x) {
                i = end;
                continue;
            }
            let mut start = i;
            let mut n = 0usize;
            while start > 0 && n < 48 {
                let ch = c[start - 1];
                if stoppable(ch) || !name_char(ch) {
                    break;
                }
                start -= 1;
                n += 1;
            }
            let raw: String = c[start..end].iter().collect();
            let name = raw
                .trim()
                .trim_start_matches([',', '.', ';', '&', '-', '(', ' '])
                .trim()
                .to_string();
            if name.chars().count() >= 4
                && name.chars().any(|x| x.is_ascii_alphabetic())
                && !name.chars().next().map(|x| x.is_ascii_digit()).unwrap_or(true)
            {
                for x in covered.iter_mut().take(end).skip(start) {
                    *x = true;
                }
                out.push(name);
            }
            i = end;
        }
    }
    out
}

// ─────────────────────────────── 取文本 ───────────────────────────────

/// 粗暴去标签：只用于 `docProps/*.xml` 这类简单 XML 的候选发现。
fn strip_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut depth = 0usize;
    for ch in xml.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// 取一个 docx 里参与侦察的可见文本（跨 run 拼接后）。
///
/// ## 一次开容器，不是 N+1 次
///
/// 原实现是 `text_parts()` 拿名单、随后**每个 part 各 `read_part` 一次**
/// （外加 docProps 两次）—— 一个文件要开 N+1 次 zip、重解 N+1 遍中央目录。
/// 2026-10-20 改成一次 `read_parts_where` 把要看的一次全读出来。
///
/// part 之间的**顺序变了**（原来"先全部文本 part、再 docProps"，
/// 现在按 zip 中央目录顺序混在一起）。这不影响结果：下面只是把文本拼成
/// 一整段供扫描器找候选，候选的 `first_at` 提示来自文件名、不来自文本位置。
fn visible_text_of(path: &Path) -> Result<String> {
    let raw = package::read_parts_where(path, |name, kind| {
        kind.is_text_bearing() || name == "docProps/core.xml" || name == "docProps/app.xml"
    })?;

    let mut buf = String::new();
    for (name, kind, bytes) in raw {
        let Ok(xml) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if kind.is_text_bearing() {
            let (paras, _) = scan::scan_part(&name, xml)?;
            for p in paras {
                if !p.visible.is_empty() {
                    buf.push_str(&p.visible);
                    buf.push('\n');
                }
            }
        } else {
            // docProps：标题/主题里常藏客户名与编号
            buf.push_str(&strip_tags(xml));
            buf.push('\n');
        }
    }
    Ok(buf)
}

// ─────────────────────────────── 主入口 ───────────────────────────────

/// 一个文件的侦察结果（并行阶段产出、串行阶段合并）。
struct OneFile {
    /// 文件名里出现的编号
    name_codes: Vec<String>,
    /// 可见文本；`Err` = 读不出来（原因进 `skipped`）
    text: Result<String>,
}

/// 侦察一个目录。
///
/// ## 为什么按文件并行
///
/// 侦察是纯 Rust、纯只读的活：一个文件的结果与其它文件毫无关系。
/// 原来整个循环串行，25 个真实交付包要一两分钟 —— 时间几乎全花在
/// 解压 + 扫描上（CPU 密集，不是 IO 等待），所以这条路径天然适合并行。
/// 2026-10-20 改成按文件并行（[`crate::par::parallel_map`]）。
///
/// ## 并行不改变报告内容
///
/// 遍历阶段先把目录走成一张**有序的条目表**（"要扫的文件"与"跳过的文件"
/// 按相遇顺序排在一起），并行阶段只处理文件、产出各文件自己的结果，
/// 收尾时**按条目表顺序**回填 `skipped` 与四组聚合。
/// 因此：跳过项的排列、候选的 `files` 计数、`first_at` 提示，与串行版相同。
///
/// ★ 一个刻意的细节：文件名里的编号**先记、后读文本**，读失败也不回滚
/// （它本来就是从文件名看出来的）。读取失败的文件**不占**文件序号 ——
/// 这两条都是串行版的原语义，改并行时照着复刻，报告才不会有差异。
pub fn probe_dir(dir: &Path, recursive: bool) -> Result<ProbeReport> {
    probe_dir_with(dir, recursive, 0)
}

/// 同 [`probe_dir`]，但能指定线程数（`0` = 自动）。
///
/// 只有单元测试直接用它：拿同一个目录分别跑 `threads = 1`（纯串行）与
/// `threads = 4`（真并行），**整份报告必须逐字相同**。这是"并行没有改变
/// 结论"唯一能自动验证的方式 —— 光看输出对不对，区分不了"并行是对的"
/// 还是"并行碰巧也是对的"。
fn probe_dir_with(dir: &Path, recursive: bool, threads: usize) -> Result<ProbeReport> {
    let depth = if recursive { usize::MAX } else { 1 };

    // ── ① 遍历：只收集，不扫描 ──
    enum Entry {
        /// 跳过（不是 docx / 老格式 / 遍历出错），原因在遍历时就已确定
        Skip(PathBuf, String),
        /// 要扫的文件
        Doc { fname: String, path: PathBuf },
    }

    let mut entries: Vec<Entry> = Vec::new();
    for e in WalkDir::new(dir).max_depth(depth).follow_links(false) {
        let e = match e {
            Ok(e) => e,
            Err(err) => {
                entries.push(Entry::Skip(dir.to_path_buf(), format!("遍历失败：{err}")));
                continue;
            }
        };
        if !e.file_type().is_file() {
            continue;
        }
        let path = e.path();
        let fname = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        if fname.starts_with("~$") || fname.starts_with(".~lock") {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "docx" {
            entries.push(Entry::Skip(
                path.to_path_buf(),
                if ext == "doc" {
                    "老格式 .doc（OLE 二进制），本工具不处理".to_string()
                } else {
                    format!("不是 .docx（.{ext}）")
                },
            ));
            continue;
        }
        entries.push(Entry::Doc {
            fname,
            path: path.to_path_buf(),
        });
    }

    // ── ② 并行：每个文件各自扫出候选 ──
    let docs: Vec<(String, PathBuf)> = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Doc { fname, path } => Some((fname.clone(), path.clone())),
            Entry::Skip(..) => None,
        })
        .collect();
    let scanned = parallel_map(&docs, threads, |_, (fname, path)| OneFile {
        name_codes: find_codes(fname),
        text: visible_text_of(path),
    });

    // ── ③ 按条目表顺序回填 ──
    let mut rep = ProbeReport::default();
    let mut agg_code_text = Agg::default();
    let mut agg_code_name = Agg::default();
    let mut agg_cn = Agg::default();
    let mut agg_en = Agg::default();
    let mut fi = 0usize;
    let mut scanned = scanned.into_iter();

    for ent in entries {
        match ent {
            Entry::Skip(p, why) => rep.skipped.push((p, why)),
            Entry::Doc { fname, path } => {
                let one = scanned.next().expect("条目表与扫描结果一一对应");
                let hint_name = format!("文件名：{fname}");
                for code in &one.name_codes {
                    agg_code_name.add(code, fi, &hint_name);
                }
                match one.text {
                    Ok(text) => {
                        for code in find_codes(&text) {
                            agg_code_text.add(&code, fi, &fname);
                        }
                        for n in find_cn_names(&text) {
                            agg_cn.add(&n, fi, &fname);
                        }
                        for n in find_en_names(&text) {
                            agg_en.add(&n, fi, &fname);
                        }
                        fi += 1;
                    }
                    Err(e) => {
                        // 读取失败：不占文件序号（与串行版一致）
                        rep.skipped.push((path, format!("读取失败：{e}")));
                    }
                }
            }
        }
    }

    rep.files_scanned = fi;
    rep.codes_in_text = agg_code_text.finish();
    rep.codes_in_names = agg_code_name.finish();
    rep.names_cn = agg_cn.finish();
    rep.names_en = agg_en.finish();
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let mut v = find_codes("标4-2026-001CE-MCL 与 2024-006CE 和 2025-005AUTc 还有 2025-004Da");
        v.sort();
        assert_eq!(
            v,
            vec!["2024-006CE", "2025-004Da", "2025-005AUTc", "2026-001CE"]
        );
    }

    #[test]
    fn codes_reject_incomplete() {
        // 5 位年份、2 位流水、右边界接字母 → 都不算
        assert!(find_codes("12026-001CE").iter().all(|c| c != "2026-001CE"));
        assert!(find_codes("2026-05CE").is_empty());
        assert!(find_codes("2026-001CExyz").is_empty());
    }

    #[test]
    fn cn_names_longest_wins() {
        let v = find_cn_names("客户：某某生物制品有限公司（甲方）");
        assert_eq!(v, vec!["某某生物制品有限公司"]);
    }

    #[test]
    fn en_names() {
        let v = find_en_names("Client: Northwind Pharmaceutical Co., Ltd. here");
        assert_eq!(v, vec!["Northwind Pharmaceutical Co., Ltd."]);
    }

    // ═════════════════ 单次开容器 + 并行 ═════════════════

    use std::io::Write;
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    fn tmp_dir(tag: &str) -> PathBuf {
        // 与 `verify` 的测试同一套理由：`%TEMP%` 在 C 盘、本机禁写，
        // 而测试二进制所在的 `target/<profile>/` 一定可写。
        let exe = std::env::current_exe().expect("取当前测试可执行文件路径");
        let dir = exe
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(exe.as_path())
            .join(format!("wrepl-probe-tmp-{tag}"));
        std::fs::create_dir_all(&dir).expect("建测试临时目录");
        dir
    }

    /// 写一个最小 docx：正文 + `docProps/core.xml`（后者用来验证
    /// 「一次开容器同时拿到正文与文档属性」）。
    fn write_docx(path: &Path, body_text: &str, title: &str) {
        let f = std::fs::File::create(path).expect("建 docx");
        let mut zw = zip::ZipWriter::new(f);
        let opt = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        zw.start_file("word/document.xml", opt).unwrap();
        zw.write_all(
            format!(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{body_text}</w:t></w:r></w:p></w:body></w:document>"#
            )
            .as_bytes(),
        )
        .unwrap();

        zw.start_file("docProps/core.xml", opt).unwrap();
        zw.write_all(
            format!(r#"<cp:coreProperties xmlns:cp="x"><dc:title>{title}</dc:title></cp:coreProperties>"#)
                .as_bytes(),
        )
        .unwrap();
        zw.finish().unwrap();
    }

    /// ★ 钉住"一次开容器"没漏东西：正文与 `docProps` 里的候选都要在。
    ///
    /// 原实现是 `text_parts()` + 每个 part 各 `read_part` + docProps 再两次 ——
    /// 合成一次 `read_parts_where` 后，最容易犯的错就是 `want` 谓词漏掉某一类，
    /// 于是"文档属性里的客户名"静默消失（报告看起来正常，只是少了几条候选）。
    #[test]
    fn visible_text_covers_body_and_docprops() {
        let dir = tmp_dir("cover");
        let p = dir.join("封面.docx");
        write_docx(
            &p,
            "正文提到 2026-009CE",
            "标题里藏着 2025-005AUTc 与某某生物制品有限公司",
        );

        let text = visible_text_of(&p).expect("读可见文本");
        assert!(text.contains("2026-009CE"), "正文编号丢了：{text}");
        assert!(text.contains("2025-005AUTc"), "docProps 编号丢了：{text}");
        assert!(
            text.contains("某某生物制品有限公司"),
            "docProps 客户名丢了：{text}"
        );
        // 去标签后不该把 XML 标记本身带进来（否则会在 `<w:t>` 里凭空匹配）
        assert!(!text.contains("<w:t>"), "标记没被剥掉：{text}");
    }

    /// ★ 串行与并行必须给出一份**逐字相同**的报告。
    ///
    /// 比较的是整份 `ProbeReport` 的 Debug 输出 —— 包括 `skipped` 的排列、
    /// 候选的 `files` 计数、`first_at` 提示。任一处因线程调度不同都会红。
    #[test]
    fn probe_report_identical_serial_vs_parallel() {
        let dir = tmp_dir("par");

        // 8 个真 docx：编号/客户名故意交叉，让"哪些文件算过"变得敏感
        for i in 0..8 {
            let code = if i % 2 == 0 { "2026-009CE" } else { "2025-005AUTc" };
            let cn = if i % 3 == 0 {
                "某某生物制品有限公司"
            } else {
                "另一家制药有限公司"
            };
            write_docx(
                &dir.join(format!("{i:02}-{code}.docx")),
                &format!("正文 {code} {cn}"),
                &format!("标题 {code}"),
            );
        }
        // 非 docx 与老格式：验证 `skipped` 的排列也不受并行影响
        std::fs::write(dir.join("说明.txt"), "不是文档").unwrap();
        std::fs::write(dir.join("老格式.doc"), "OLE").unwrap();
        std::fs::write(dir.join("~$锁文件.docx"), "锁").unwrap();

        let serial = probe_dir_with(&dir, false, 1).expect("串行侦察");
        let parallel = probe_dir_with(&dir, false, 4).expect("并行侦察");

        assert_eq!(
            format!("{serial:?}"),
            format!("{parallel:?}"),
            "串行与并行的报告不一致"
        );
        assert_eq!(serial.files_scanned, 8, "跳过的文件不该计入");
    }
}
