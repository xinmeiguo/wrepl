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
fn visible_text_of(path: &Path) -> Result<String> {
    let mut buf = String::new();
    for (name, _kind) in package::text_parts(path)? {
        let bytes = match package::read_part(path, &name) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let xml = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let (paras, _) = scan::scan_part(&name, xml)?;
        for p in paras {
            if !p.visible.is_empty() {
                buf.push_str(&p.visible);
                buf.push('\n');
            }
        }
    }
    // docProps：标题/主题里常藏客户名与编号
    for name in ["docProps/core.xml", "docProps/app.xml"] {
        if let Ok(b) = package::read_part(path, name) {
            if let Ok(s) = std::str::from_utf8(&b) {
                buf.push_str(&strip_tags(s));
                buf.push('\n');
            }
        }
    }
    Ok(buf)
}

// ─────────────────────────────── 主入口 ───────────────────────────────

/// 侦察一个目录。
pub fn probe_dir(dir: &Path, recursive: bool) -> Result<ProbeReport> {
    let depth = if recursive { usize::MAX } else { 1 };
    let mut rep = ProbeReport::default();

    let mut agg_code_text = Agg::default();
    let mut agg_code_name = Agg::default();
    let mut agg_cn = Agg::default();
    let mut agg_en = Agg::default();

    for e in WalkDir::new(dir).max_depth(depth).follow_links(false) {
        let e = match e {
            Ok(e) => e,
            Err(err) => {
                rep.skipped
                    .push((dir.to_path_buf(), format!("遍历失败：{err}")));
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
            rep.skipped.push((
                path.to_path_buf(),
                if ext == "doc" {
                    "老格式 .doc（OLE 二进制），本工具不处理".to_string()
                } else {
                    format!("不是 .docx（.{ext}）")
                },
            ));
            continue;
        }

        let fi = rep.files_scanned;
        rep.files_scanned += 1;
        let hint_name = format!("文件名：{fname}");

        for code in find_codes(&fname) {
            agg_code_name.add(&code, fi, &hint_name);
        }

        let text = match visible_text_of(path) {
            Ok(t) => t,
            Err(e) => {
                rep.skipped
                    .push((path.to_path_buf(), format!("读取失败：{e}")));
                rep.files_scanned -= 1;
                continue;
            }
        };
        let hint = format!("{fname}");

        for code in find_codes(&text) {
            agg_code_text.add(&code, fi, &hint);
        }
        for n in find_cn_names(&text) {
            agg_cn.add(&n, fi, &hint);
        }
        for n in find_en_names(&text) {
            agg_en.add(&n, fi, &hint);
        }
    }

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
}
