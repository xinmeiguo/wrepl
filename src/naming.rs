//! 文件名改名——把**同一套规则**作用到文件名上。
//!
//! ## 为什么复用 `engine` 的折叠查找
//!
//! 规则语义（区分大小写 / 区分全半角 / 全字匹配 / 左起不重叠 / 区间重叠算冲突）
//! 必须与正文替换**完全一致**。另写一份匹配代码只会慢慢漂移，最后出现
//! 「正文改了、文件名忘改」——这正是最难看出来的不一致。
//! 所以这里直接调 `engine::fold` + `engine::find_all`，冲突判定也照搬同一套。
//!
//! ## 只改词干，扩展名原样保留
//!
//! `.docx` 一律不动：扩充名参与匹配只会带来意外（比如某条规则真把
//! `docx` 里的片段吃掉）。所以切出词干再改，改完拼回去。

use crate::engine;
use crate::rules::Rule;
use anyhow::{Result, bail};

/// Windows 文件名里不允许出现的字符。
const ILLEGAL: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Windows 保留设备名（不区分大小写，带不带扩展名都不允许）。
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 一次文件名命中。
#[derive(Debug, Clone)]
pub struct NameHit {
    pub rule_id: u32,
    /// 命中位置（词干内的字符下标）
    pub at: usize,
    pub matched: String,
    pub replaced_by: String,
    pub applied: bool,
    pub reason: String,
}

/// 改名计划。
#[derive(Debug, Clone)]
pub struct RenamePlan {
    pub old_name: String,
    pub new_name: String,
    pub hits: Vec<NameHit>,
    pub warnings: Vec<String>,
}

impl RenamePlan {
    pub fn changed(&self) -> bool {
        self.new_name != self.old_name
    }
    pub fn applied(&self) -> usize {
        self.hits.iter().filter(|h| h.applied).count()
    }
    pub fn conflicts(&self) -> usize {
        self.hits.iter().filter(|h| !h.applied).count()
    }
}

#[derive(Debug, Clone)]
struct Cand {
    rule: usize,
    a: usize,
    b: usize,
    matched: String,
}

/// 拆词干与扩展名。`.docx` 这种以点开头的名字视为无扩展名。
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn collect(stem: &str, rules: &[Rule]) -> Result<Vec<Cand>> {
    let chars: Vec<char> = stem.chars().collect();
    let mut out: Vec<Cand> = Vec::new();

    for (ri, r) in rules.iter().enumerate() {
        if !r.enabled || !r.scope.filename {
            continue;
        }
        if r.use_wildcard {
            // 与 engine 的立场一致：通配符未实现，绝不静默按字面处理
            bail!(
                "规则 #{} 开启了通配符，但通配符尚未实现（里程碑 M7）。\
                 为避免静默按字面处理，文件名改名已中止。",
                r.id
            );
        }
        let needle = engine::fold(&r.find, r.case_sensitive, r.kana_sensitive);
        if needle.is_empty() {
            continue;
        }
        let hay = engine::fold(stem, r.case_sensitive, r.kana_sensitive);
        for (a, b) in engine::find_all(&hay, &needle, r.whole_word) {
            out.push(Cand {
                rule: ri,
                a,
                b,
                matched: chars[a..b].iter().collect(),
            });
        }
    }
    Ok(out)
}

/// 标记区间重叠的候选——两条都不替换，不做"取一个"的静默决策（与正文一致）。
fn mark_overlap(cands: &[Cand]) -> Vec<bool> {
    let mut out = vec![false; cands.len()];
    for x in 0..cands.len() {
        for y in (x + 1)..cands.len() {
            let p = &cands[x];
            let q = &cands[y];
            if p.a < q.b && q.a < p.b {
                out[x] = true;
                out[y] = true;
            }
        }
    }
    out
}

/// 把互不重叠的编辑落成新词干。
fn apply_edits(stem: &str, edits: &[(usize, usize, String)]) -> String {
    let chars: Vec<char> = stem.chars().collect();
    let mut out = String::new();
    let mut cur = 0usize;
    for (a, b, rep) in edits {
        if *a < cur {
            continue; // 防御：重叠的已在上游排除
        }
        out.extend(&chars[cur..*a]);
        out.push_str(rep);
        cur = *b;
    }
    out.extend(&chars[cur..]);
    out
}

/// 默认模式：所有规则各自基于**原词干**，重叠报冲突。
pub fn rename_stem(stem: &str, rules: &[Rule]) -> Result<RenamePlan> {
    let cands = collect(stem, rules)?;
    let overlap = mark_overlap(&cands);

    let mut hits = Vec::new();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    for (i, c) in cands.iter().enumerate() {
        let r = &rules[c.rule];
        if overlap[i] {
            hits.push(NameHit {
                rule_id: r.id,
                at: c.a,
                matched: c.matched.clone(),
                replaced_by: r.replace.clone(),
                applied: false,
                reason: "与另一条命中区间重叠，冲突未替换".to_string(),
            });
            continue;
        }
        edits.push((c.a, c.b, r.replace.clone()));
        hits.push(NameHit {
            rule_id: r.id,
            at: c.a,
            matched: c.matched.clone(),
            replaced_by: r.replace.clone(),
            applied: true,
            reason: String::new(),
        });
    }

    edits.sort_by_key(|(a, _, _)| *a);
    let new_stem = apply_edits(stem, &edits);
    let (stem2, warns) = sanitize(&new_stem);

    Ok(RenamePlan {
        old_name: stem.to_string(),
        new_name: stem2,
        hits,
        warnings: warns,
    })
}

/// 链式模式：前一条规则的输出即后一条的输入。
pub fn rename_stem_chained(stem: &str, rules: &[Rule]) -> Result<RenamePlan> {
    let mut cur = stem.to_string();
    let mut hits = Vec::new();

    for r in rules.iter().filter(|r| r.enabled && r.scope.filename) {
        if r.use_wildcard {
            bail!("规则 #{} 开启了通配符，但通配符尚未实现（里程碑 M7）", r.id);
        }
        let needle = engine::fold(&r.find, r.case_sensitive, r.kana_sensitive);
        if needle.is_empty() {
            continue;
        }
        let chars: Vec<char> = cur.chars().collect();
        let hay = engine::fold(&cur, r.case_sensitive, r.kana_sensitive);
        let found = engine::find_all(&hay, &needle, r.whole_word);
        if found.is_empty() {
            continue;
        }
        let edits: Vec<(usize, usize, String)> = found
            .iter()
            .map(|(a, b)| (*a, *b, r.replace.clone()))
            .collect();
        for (a, b) in &found {
            hits.push(NameHit {
                rule_id: r.id,
                at: *a,
                matched: chars[*a..*b].iter().collect(),
                replaced_by: r.replace.clone(),
                applied: true,
                reason: String::new(),
            });
        }
        cur = apply_edits(&cur, &edits);
    }

    let (stem2, warns) = sanitize(&cur);
    Ok(RenamePlan {
        old_name: stem.to_string(),
        new_name: stem2,
        hits,
        warnings: warns,
    })
}

/// 把改名结果修成 Windows 合法文件名。
///
/// **绝不静默**：任何一处修正都记进 `warnings`，进报告，让人能看见。
fn sanitize(stem: &str) -> (String, Vec<String>) {
    let mut warns = Vec::new();

    let mut s: String = stem
        .chars()
        .map(|c| {
            if ILLEGAL.contains(&c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s != stem {
        warns.push(format!(
            "文件名含 Windows 非法字符，已替换为下划线：`{stem}` → `{s}`"
        ));
    }

    let trimmed = s.trim_end_matches(['.', ' ']).to_string();
    if trimmed != s {
        warns.push(format!(
            "文件名以点或空格结尾（Windows 不允许），已去掉：`{s}` → `{trimmed}`"
        ));
        s = trimmed;
    }

    if s.is_empty() {
        warns.push(format!("改名后文件名为空，已放弃改名：`{stem}`"));
        return (stem.to_string(), warns);
    }

    if RESERVED.iter().any(|r| s.eq_ignore_ascii_case(r)) {
        warns.push(format!("`{s}` 是 Windows 保留设备名，已加前缀下划线"));
        s = format!("_{s}");
    }

    (s, warns)
}

/// 对**完整文件名**（含扩展名）改名：只改词干，扩展名原样保留。
pub fn rename_file_name(name: &str, rules: &[Rule], chain: bool) -> Result<RenamePlan> {
    let (stem, ext) = split_ext(name);
    let inner = if chain {
        rename_stem_chained(stem, rules)?
    } else {
        rename_stem(stem, rules)?
    };
    // 改名后词干为空 → sanitize 已回退成原词干，等价于不改
    let mut p = inner;
    p.old_name = name.to_string();
    p.new_name = format!("{}{}", p.new_name, ext);
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Scope;

    fn rule(id: u32, f: &str, r: &str) -> Rule {
        let mut x = Rule::new(id, f, r);
        x.scope = Scope::ALL;
        x
    }

    #[test]
    fn basic_rename() {
        let rs = vec![rule(1, "2026-001CE", "2026-002CE")];
        let p = rename_file_name("标4-2026-001CE-MCL-主要部件清单.docx", &rs, false).unwrap();
        assert_eq!(p.new_name, "标4-2026-002CE-MCL-主要部件清单.docx");
        assert_eq!(p.applied(), 1);
    }

    #[test]
    fn ext_never_touched() {
        let rs = vec![rule(1, "docx", "XXXX")];
        let p = rename_file_name("a.docx", &rs, false).unwrap();
        assert_eq!(p.new_name, "a.docx");
    }

    #[test]
    fn illegal_chars_are_reported() {
        let rs = vec![rule(1, "A", "B/C:D")];
        let p = rename_file_name("A.docx", &rs, false).unwrap();
        assert_eq!(p.new_name, "B_C_D.docx");
        assert!(!p.warnings.is_empty());
    }

    #[test]
    fn scope_without_filename_is_ignored() {
        let mut r = rule(1, "A", "B");
        r.scope = Scope::BODY_ONLY;
        let p = rename_file_name("A.docx", &[r], false).unwrap();
        assert_eq!(p.new_name, "A.docx");
        assert_eq!(p.hits.len(), 0);
    }

    #[test]
    fn overlap_is_conflict_not_silent_pick() {
        let rs = vec![rule(1, "abc", "X"), rule(2, "bc", "Y")];
        let p = rename_file_name("abc.docx", &rs, false).unwrap();
        assert_eq!(p.new_name, "abc.docx");
        assert_eq!(p.conflicts(), 2);
    }
}
