//! 匹配引擎——`scan`（预览）与 `apply`（落盘）**共用这一份**。
//!
//! 设计规格里写死的一条：不允许"预览一套、执行一套"。所以匹配、冲突判定、
//! 编辑生成都只在这里实现一次，两种命令只是最后落不落盘的区别。
//!
//! ## 折叠（fold）
//!
//! 搜"区分大小写 / 区分全角半角"不靠正则，而是把待搜文本与查找串
//! **逐字符折叠**成同一个可比形式，再逐字符比较。这样：
//!
//! - 折叠前后**字符数一一对应**，命中下标可以直接映射回原始可见字符下标
//! - 不引入正则引擎，就没有"正则把偏移算错"这一整类问题
//! - 全角/半角在中文文档里是实打实的差异（`Ａ` 与 `A`、`，` 与 `,`），必须能区分
//!
//! ## 区间重叠的两种裁决（`adjudicate`）
//!
//! 默认模式下所有规则**各自基于原文**匹配，所以两条规则的命中区间可能压在一起。
//! 这时有两种裁决，由 [`plan_part`] 的 `longest_first` 选定：
//!
//! - **默认：两条都不替换**。宁可报出来交给用户改规则表，也不悄悄挑一条。
//! - **最长匹配优先：只让更长的落笔**，被挤掉的记进报告（谁让给谁）。
//!
//! 两者都只取决于规则集合与命中区间，**与文件、线程数、运行次序无关**。
//! 链式模式（[`plan_part_chained`]）是先后作用，不存在重叠，无需裁决。

use crate::docx::package::PartKind;
use crate::docx::rewrite::{self, Edit};
use crate::docx::scan::{self, Para};
use crate::rules::{Rule, Slot, slot_of};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;

/// 一个字符的折叠结果（与输入**一一对应**，长度不变）。
pub fn fold_char(c: char, case_sensitive: bool, kana_sensitive: bool) -> char {
    let mut c = c;
    if !kana_sensitive {
        let u = c as u32;
        // 全角 ASCII → 半角
        if (0xFF01..=0xFF5E).contains(&u) {
            if let Some(x) = char::from_u32(u - 0xFEE0) {
                c = x;
            }
        } else if u == 0x3000 {
            // 全角空格
            c = ' ';
        }
    }
    if !case_sensitive {
        // 只做**单字符**小写化：多字符展开（如 ß → ss）会破坏一一对应，
        // 那种字符保持原样（按"区分大小写"处理），这是刻意的取舍。
        let mut it = c.to_lowercase();
        if let Some(first) = it.next() {
            if it.next().is_none() {
                c = first;
            }
        }
    }
    c
}

/// 把字符串折叠成字符向量。
pub fn fold(s: &str, case_sensitive: bool, kana_sensitive: bool) -> Vec<char> {
    s.chars()
        .map(|c| fold_char(c, case_sensitive, kana_sensitive))
        .collect()
}

/// 整字匹配用的"词字符"判定。
///
/// ⚠ 与 `\b` **不是一回事**：官方定义是"在有效分隔符范围内整段完全相等"。
/// 汉字属于词字符，所以 `某某生物制品有限公司` 里查 `某某`（开启全字匹配）**不匹配**——
/// 这与 Word 一致；中文场景一般不要开这个开关。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 在折叠后的字符序列里查找全部命中（左起、不重叠）。
///
/// 返回**原始下标**区间 `(a, b)`，可直接用于 `Para.map`。
pub fn find_all(hay: &[char], needle: &[char], whole_word: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (n, m) = (hay.len(), needle.len());
    if m == 0 || m > n {
        return out;
    }

    let boundary_ok = |i: usize| -> bool {
        if !whole_word {
            return true;
        }
        let left_ok = i == 0 || !is_word_char(hay[i - 1]);
        let j = i + m;
        let right_ok = j >= n || !is_word_char(hay[j]);
        left_ok && right_ok
    };

    let mut i = 0usize;
    while i + m <= n {
        if hay[i] == needle[0] && &hay[i..i + m] == needle && boundary_ok(i) {
            out.push((i, i + m));
            i += m; // 同一规则内不重叠
        } else {
            i += 1;
        }
    }
    out
}

/// 一次候选命中。
#[derive(Debug, Clone)]
struct Cand {
    rule: usize,
    para: usize,
    a: usize,
    b: usize,
    matched: String,
    slot: Slot,
}

/// 一条命中在报告里的形态。
#[derive(Debug, Clone)]
pub struct Hit {
    pub rule_id: u32,
    pub rule_index: usize,
    pub para: usize,
    pub slot: Slot,
    pub a: usize,
    pub b: usize,
    pub matched: String,
    pub replaced_by: String,
    pub applied: bool,
    pub reason: String,
    pub strategy: String,
}

/// 单个 part 的处理结果。
#[derive(Debug, Clone)]
pub struct PartPlan {
    pub part: String,
    pub kind: PartKind,
    /// 改写后的字节流（未改动时与输入相同）
    pub new_bytes: Vec<u8>,
    pub changed: bool,
    pub hits: Vec<Hit>,
    pub conflicts: usize,
}

/// 扫描一个 part，按作用域过滤，收集全部候选命中。
fn collect(paras: &[Para], kind: PartKind, rules: &[Rule]) -> Result<Vec<Cand>> {
    let mut cands = Vec::new();

    for (ri, r) in rules.iter().enumerate() {
        if !r.enabled {
            continue;
        }
        if r.use_wildcard {
            bail!(
                "规则 #{} 开启了通配符，但通配符尚未实现（里程碑 M7）。\
                 为避免静默按字面处理，这里直接中止。",
                r.id
            );
        }
        let needle = fold(&r.find, r.case_sensitive, r.kana_sensitive);
        if needle.is_empty() {
            continue;
        }

        for p in paras {
            let slot = match slot_of(kind, p.in_textbox) {
                Some(s) => s,
                None => continue,
            };
            if !r.scope.allows(slot) {
                continue;
            }
            let hay = fold(&p.visible, r.case_sensitive, r.kana_sensitive);
            for (a, b) in find_all(&hay, &needle, r.whole_word) {
                let matched: String = p
                    .visible
                    .chars()
                    .skip(a)
                    .take(b - a)
                    .collect();
                cands.push(Cand {
                    rule: ri,
                    para: p.index,
                    a,
                    b,
                    matched,
                    slot,
                });
            }
        }
    }

    Ok(cands)
}

/// 把候选命中落成编辑计划。
///
/// `rej` 是 [`adjudicate`] 的裁决结果：`None` = 落笔，`Some(w)` = 被候选 `w` 挤出（不替换，但要进报告）。
fn build_edits(
    xml: &[u8],
    paras: &[Para],
    cands: &[Cand],
    rules: &[Rule],
    rej: &[Option<usize>],
    longest_first: bool,
) -> Result<(Vec<Edit>, Vec<Hit>)> {
    let mut edits: Vec<Edit> = Vec::new();
    let mut hits: Vec<Hit> = Vec::new();

    for (i, c) in cands.iter().enumerate() {
        let r = &rules[c.rule];
        let para = &paras[c.para];

        if let Some(by) = rej[i] {
            hits.push(Hit {
                rule_id: r.id,
                rule_index: c.rule,
                para: c.para,
                slot: c.slot,
                a: c.a,
                b: c.b,
                matched: c.matched.clone(),
                replaced_by: r.replace.clone(),
                applied: false,
                reason: reject_reason(cands, rules, Some(by), longest_first),
                strategy: "—".to_string(),
            });
            continue;
        }

        let he = rewrite::build(xml, para, c.a, c.b, &r.replace).with_context(|| {
            format!(
                "规则 #{} 在 {} 第 {} 段生成编辑失败",
                r.id, para.part, para.index
            )
        })?;
        let desc = he.describe();
        edits.extend(he.edits);
        hits.push(Hit {
            rule_id: r.id,
            rule_index: c.rule,
            para: c.para,
            slot: c.slot,
            a: c.a,
            b: c.b,
            matched: c.matched.clone(),
            replaced_by: r.replace.clone(),
            applied: true,
            reason: String::new(),
            strategy: desc,
        });
    }

    Ok((edits, hits))
}

/// 一条被拒命中的原因文案。
fn reject_reason(
    cands: &[Cand],
    rules: &[Rule],
    by: Option<usize>,
    longest_first: bool,
) -> String {
    if !longest_first {
        return "与另一条命中区间重叠，冲突未替换".to_string();
    }
    match by.and_then(|w| cands.get(w)) {
        Some(w) => format!(
            "区间与规则#{}（命中 `{}`）重叠，让给更长的规则（最长匹配优先）",
            rules[w.rule].id, w.matched
        ),
        None => "区间与更长的规则重叠（最长匹配优先）".to_string(),
    }
}

/// 裁决候选命中：`None` = 落笔，`Some(w)` = 被候选 `w` 挤出（不落笔）。
///
/// 两种策略，**都只看规则集合与这一段的命中区间，与文件、线程数无关**：
///
/// - **默认（`longest_first = false`）**：同一段落内任意两条区间重叠 → **两条都拒**。
///   不替用户挑一条（见 `mark_overlap` 的旧名"不猜"策略），把冲突如实报出来。
///
/// - **最长匹配优先（`longest_first = true`）**：同一段落内按「命中更长者优先、
///   等长按规则表序、再按段内位置」排出优先级，依次取用；与已取用区间重叠的
///   降级为「被挤掉」。同一位置被多条规则盯上时（真实案例：`-50℃` 同时命中
///   `-50℃` / `50℃` / `0℃`）结果仍然唯一确定，且「谁让给谁」能写进报告。
///
/// 重叠判定用半开区间：`p.a < q.b && q.a < p.b` —— 首尾相接（`p.b == q.a`）不算重叠。
fn adjudicate(cands: &[Cand], longest_first: bool) -> Vec<Option<usize>> {
    let mut out: Vec<Option<usize>> = vec![None; cands.len()];
    if cands.is_empty() {
        return out;
    }
    let mut by_para: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, c) in cands.iter().enumerate() {
        by_para.entry(c.para).or_default().push(i);
    }
    for (_, idxs) in by_para {
        if longest_first {
            // 优先级：命中长度降序 → 规则表序升序 → 段内起始位置升序。
            // 末两项是**决胜项**，保证同一份规则表在任何机器上跑出同一个结果。
            let mut order = idxs.clone();
            order.sort_by(|&x, &y| {
                let (p, q) = (&cands[x], &cands[y]);
                (q.b - q.a)
                    .cmp(&(p.b - p.a))
                    .then(p.rule.cmp(&q.rule))
                    .then(p.a.cmp(&q.a))
            });
            // `taken` 是按优先级顺序 push 的，所以第一个撞上的就是"挤掉它的主因"。
            let mut taken: Vec<usize> = Vec::new();
            for &i in &order {
                let c = &cands[i];
                let blocker = taken.iter().copied().find(|&t| {
                    let t = &cands[t];
                    t.a < c.b && c.a < t.b
                });
                match blocker {
                    Some(t) => out[i] = Some(t),
                    None => taken.push(i),
                }
            }
        } else {
            for x in 0..idxs.len() {
                for y in (x + 1)..idxs.len() {
                    let p = &cands[idxs[x]];
                    let q = &cands[idxs[y]];
                    if p.a < q.b && q.a < p.b {
                        out[idxs[x]] = out[idxs[x]].or(Some(idxs[y]));
                        out[idxs[y]] = out[idxs[y]].or(Some(idxs[x]));
                    }
                }
            }
        }
    }
    out
}

/// 处理单个 part（默认模式：所有规则各自基于原文，一次扫描）。
pub fn plan_part(
    part: &str,
    kind: PartKind,
    xml: &[u8],
    rules: &[Rule],
    longest_first: bool,
) -> Result<PartPlan> {
    let text = std::str::from_utf8(xml)
        .with_context(|| format!("{part} 不是 UTF-8 文本，无法处理"))?;
    let (paras, _) = scan::scan_part(part, text)?;

    let cands = collect(&paras, kind, rules)?;
    let rej = adjudicate(&cands, longest_first);
    let conflicts = rej.iter().filter(|x| x.is_some()).count();

    let (edits, hits) = build_edits(xml, &paras, &cands, rules, &rej, longest_first)?;
    let new_bytes = if edits.is_empty() {
        xml.to_vec()
    } else {
        rewrite::apply(xml, &edits)?
    };

    Ok(PartPlan {
        part: part.to_string(),
        kind,
        changed: new_bytes != xml,
        new_bytes,
        hits,
        conflicts,
    })
}

/// 处理单个 part（链式模式：前一条规则的输出即后一条的输入，逐个规则重新扫描）。
///
/// 链式下不需要冲突检测——规则是**先后**作用的，顺序本身就是语义。
pub fn plan_part_chained(
    part: &str,
    kind: PartKind,
    xml: &[u8],
    rules: &[Rule],
) -> Result<PartPlan> {
    let mut cur = xml.to_vec();
    let mut hits: Vec<Hit> = Vec::new();

    for r in rules.iter().filter(|r| r.enabled) {
        if r.use_wildcard {
            bail!("规则 #{} 开启了通配符，但通配符尚未实现（里程碑 M7）", r.id);
        }
        let needle = fold(&r.find, r.case_sensitive, r.kana_sensitive);
        if needle.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(&cur)
            .with_context(|| format!("{part} 链式处理中变为非 UTF-8"))?
            .to_string();
        let (paras, _) = scan::scan_part(part, &text)?;

        let mut edits: Vec<Edit> = Vec::new();
        for p in &paras {
            let slot = match slot_of(kind, p.in_textbox) {
                Some(s) => s,
                None => continue,
            };
            if !r.scope.allows(slot) {
                continue;
            }
            let hay = fold(&p.visible, r.case_sensitive, r.kana_sensitive);
            for (a, b) in find_all(&hay, &needle, r.whole_word) {
                let matched: String = p.visible.chars().skip(a).take(b - a).collect();
                let he = rewrite::build(&cur, p, a, b, &r.replace)?;
                let desc = he.describe();
                edits.extend(he.edits);
                hits.push(Hit {
                    rule_id: r.id,
                    rule_index: 0,
                    para: p.index,
                    slot,
                    a,
                    b,
                    matched,
                    replaced_by: r.replace.clone(),
                    applied: true,
                    reason: String::new(),
                    strategy: desc,
                });
            }
        }
        if !edits.is_empty() {
            cur = rewrite::apply(&cur, &edits)?;
        }
    }

    Ok(PartPlan {
        part: part.to_string(),
        kind,
        changed: cur != xml,
        new_bytes: cur,
        hits,
        conflicts: 0,
    })
}
