//! 验证关卡 1 / 2 —— 格式保全的硬证据，**全程只读文件本身，不调用任何外部程序**。
//!
//! ## 关卡 1：part 级字节比对
//!
//! 除被明确改动的 part 外，其余 part 的解压内容必须 **SHA256 完全一致**；
//! 且「其它」类 part（styles / settings / numbering / media …）**一个字节都不许变**。
//!
//! ## 关卡 2：XML 语义骨架比对
//!
//! 把两份 XML 归一成"骨架 token 流"：**丢弃文本承载元素**
//! （`w:t` / `w:delText` / `w:instrText` 整棵子树）后，其余元素名、属性、
//! 层级、以及元素间空白必须**逐一完全相同**。
//!
//! 为什么这样定义：替换的**唯一合法改动面**就是文本承载元素的内容。
//! 骨架不变 ⇒ `<w:pPr>`、`<w:rPr>`、`<w:tbl>`、`<w:sectPr>` 等格式标记分毫未动。
//! 反过来，任何"多删了一个 run""少了一个 rPr"都会被这一步抓住。
//!
//! 另外单独统计三类承载元素的数量：`delText` / `instrText` 的数量**必须相等**
//! （修订内容与域代码绝不允许被改动），`w:t` 的数量变化则如实报出——只有
//! 「命中全是空元素」这一种情况会新增 `<w:t>`。

use crate::docx::package::{self, PartKind};
use crate::docx::scan;
use crate::engine;
use crate::rules::Rule;
use anyhow::{Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// 关卡 1 中一个 part 的比对结果。
#[derive(Debug, Clone)]
pub struct L1Part {
    pub name: String,
    pub kind: PartKind,
    pub same: bool,
    pub size_a: u64,
    pub size_b: u64,
    pub sha_a: String,
    pub sha_b: String,
}

/// 关卡 1 报告。
#[derive(Debug, Clone, Default)]
pub struct L1Report {
    pub parts: Vec<L1Part>,
    pub only_a: Vec<String>,
    pub only_b: Vec<String>,
}

impl L1Report {
    /// **禁止**发生的改动：非文本容器 part（styles / settings / media …）内容变了。
    pub fn forbidden(&self) -> Vec<&L1Part> {
        self.parts
            .iter()
            .filter(|p| !p.same && !p.kind.is_text_bearing())
            .collect()
    }

    /// 发生了改动的 part。
    pub fn changed(&self) -> Vec<&L1Part> {
        self.parts.iter().filter(|p| !p.same).collect()
    }

    pub fn pass(&self) -> bool {
        self.only_a.is_empty() && self.only_b.is_empty() && self.forbidden().is_empty()
    }
}

// ─────────────────────────── 验证快照 ───────────────────────────

/// 一个包在**某一时刻**的验证证据：关卡 1 与关卡 2 需要的全部信息。
///
/// ## 为什么必须有它
///
/// **就地替换没有留得住的"原件"。** 磁盘上那一份原文件在改写的那一刻就被覆盖了，
/// 事后再想读一遍原件做比对是不可能的——唯一的窗口是"改写之前"。
///
/// 没有快照时，"验证"只能拿 `(src, dst)` 两条路径去读。就地替换下这两条路径
/// **指的是同一个文件**，等于把文件跟它自己比 ⇒ 关卡 1/2 必然全等通过。
/// 那不是"验证通过"，是**根本没有验证**。文件一旦改名（`src` 在改名后失效），
/// 连这个假通过都维持不住，直接报"打不开文件"。
///
/// 所以就地路径的做法是：**写盘前**采一份快照，写完立刻与产物比完、随即丢弃。
/// 同一时刻内存里只有"线程数 × 一份骨架"的量级，不随文件数增长。
#[derive(Debug, Clone, Default)]
pub struct PkgSnap {
    pub parts: Vec<PartSnap>,
    /// 文本 part 名 → 骨架（关卡 2）。
    /// 读不出 UTF-8 的 part 不进这张表，比对时按"有一侧缺失"跳过——与旧行为一致。
    pub skel: BTreeMap<String, SkelSnap>,
}

#[derive(Debug, Clone)]
pub struct PartSnap {
    pub name: String,
    pub kind: PartKind,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct SkelSnap {
    pub tokens: Vec<String>,
    pub carriers: CarrierCounts,
}

/// 采集一个包的验证快照。
///
/// ★ **一个包只开一次容器。**
///
/// 原实现是 `inspect(path)`（开 1 次、解压全包算 SHA）＋ 对**每个文本 part 各调一次
/// `read_part(path, name)`** —— 而 `read_part` 每次都重开容器、重解一遍中央目录，
/// 一个包被打开 `1 + N` 次（N ＝ 文本 part 数）。
/// `package::read_parts_where` 的文档注释早把规矩写死了：
/// 「**需要读一个以上条目时一律走这里，只开一次**」，这里补上。
///
/// 行为与旧实现一致：全部 part 都进 `parts`（关卡 1 要全量指纹），
/// 只有文本容器做骨架（关卡 2）；非 UTF-8 的文本 part 仍然报错而非静默跳过。
pub fn snapshot(path: &Path) -> Result<PkgSnap> {
    let raw = package::read_parts_where(path, |_, _| true)?;
    let mut parts = Vec::with_capacity(raw.len());
    let mut skel = BTreeMap::new();
    for (name, kind, bytes) in raw {
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let size = bytes.len() as u64;
        if kind.is_text_bearing() {
            let s = std::str::from_utf8(&bytes).with_context(|| format!("{name} 不是 UTF-8"))?;
            let (tokens, carriers) = skeleton(s).with_context(|| format!("{name} 骨架解析失败"))?;
            skel.insert(name.clone(), SkelSnap { tokens, carriers });
        }
        parts.push(PartSnap {
            name,
            kind,
            sha256,
            size,
        });
    }
    Ok(PkgSnap { parts, skel })
}

/// 执行关卡 1（按路径读两侧）。
pub fn level1(a: &Path, b: &Path) -> Result<L1Report> {
    let sa = snapshot(a).context("读左侧文件失败")?;
    let sb = snapshot(b).context("读右侧文件失败")?;
    Ok(compare_level1(&sa, &sb))
}

/// 关卡 1 的**比对本体**：两份快照的 part 级字节比对。
///
/// 路径入口与"就地替换的内存快照"入口都收敛到这一个函数——
/// 判定逻辑只允许有一份，否则两条路径迟早给出不同结论。
pub fn compare_level1(a: &PkgSnap, b: &PkgSnap) -> L1Report {
    let mut rep = L1Report::default();
    let map_b: std::collections::HashMap<&str, &PartSnap> =
        b.parts.iter().map(|p| (p.name.as_str(), p)).collect();

    for x in &a.parts {
        match map_b.get(x.name.as_str()) {
            Some(y) => rep.parts.push(L1Part {
                name: x.name.clone(),
                kind: x.kind,
                same: x.sha256 == y.sha256,
                size_a: x.size,
                size_b: y.size,
                sha_a: x.sha256.clone(),
                sha_b: y.sha256.clone(),
            }),
            None => rep.only_a.push(x.name.clone()),
        }
    }
    for y in &b.parts {
        if !a.parts.iter().any(|p| p.name == y.name) {
            rep.only_b.push(y.name.clone());
        }
    }
    rep
}

/// 三类文本承载元素的数量（`w:t` / `w:delText` / `w:instrText`）。
pub type CarrierCounts = [usize; 3];

fn carrier_slot(local: &str) -> Option<usize> {
    match local {
        "t" => Some(0),
        "delText" => Some(1),
        "instrText" => Some(2),
        _ => None,
    }
}

/// 关卡 2 中一个 part 的比对结果。
#[derive(Debug, Clone)]
pub struct L2Part {
    pub name: String,
    pub pass: bool,
    pub tokens_a: usize,
    pub tokens_b: usize,
    pub carriers_a: CarrierCounts,
    pub carriers_b: CarrierCounts,
    /// 骨架中消失的 token——**全部**必须是 run 内的空元素，否则判不通过
    pub deleted: Vec<String>,
    /// 无法解释的差异（出现新结构、或删了不该删的）
    pub bad: Vec<String>,
}

/// 关卡 2 报告。
#[derive(Debug, Clone, Default)]
pub struct L2Report {
    pub parts: Vec<L2Part>,
}

impl L2Report {
    pub fn pass(&self) -> bool {
        self.parts.iter().all(|p| p.pass)
    }
}

/// 执行关卡 2：对两侧都存在、且是文本容器的 part 逐一比对骨架（按路径读两侧）。
pub fn level2(a: &Path, b: &Path) -> Result<L2Report> {
    let sa = snapshot(a).context("读左侧文件失败")?;
    let sb = snapshot(b).context("读右侧文件失败")?;
    Ok(compare_level2(&sa, &sb))
}

/// 关卡 2 的**比对本体**：两份快照的骨架逐 token 比对。
pub fn compare_level2(a: &PkgSnap, b: &PkgSnap) -> L2Report {
    let mut rep = L2Report::default();

    for x in &a.parts {
        if !x.kind.is_text_bearing() {
            continue;
        }
        let (Some(sa), Some(sb)) = (a.skel.get(&x.name), b.skel.get(&x.name)) else {
            continue;
        };
        let (ta, ca) = (&sa.tokens, sa.carriers);
        let (tb, cb) = (&sb.tokens, sb.carriers);

        let (deleted, bad) = compare_skeleton(ta, tb);

        // delText / instrText 数量必须相等（修订内容与域代码绝不允许被改动）
        let mut bad = bad;
        if ca[1] != cb[1] {
            bad.push(format!(
                "w:delText 数量变化 {} → {}（修订删除内容不允许被改动）",
                ca[1], cb[1]
            ));
        }
        if ca[2] != cb[2] {
            bad.push(format!(
                "w:instrText 数量变化 {} → {}（域代码不允许被改动）",
                ca[2], cb[2]
            ));
        }
        if cb[0] < ca[0] {
            bad.push(format!(
                "w:t 数量减少 {} → {}（文本承载元素只应新增，不应被删除）",
                ca[0], cb[0]
            ));
        }

        rep.parts.push(L2Part {
            name: x.name.clone(),
            pass: bad.is_empty(),
            tokens_a: ta.len(),
            tokens_b: tb.len(),
            carriers_a: ca,
            carriers_b: cb,
            deleted,
            bad,
        });
    }

    rep
}

/// 判断 `B` 是否由 `A` 仅通过「删除 run 内空元素」得到。
///
/// 返回 `(被删除的 token, 无法解释的差异)`。
///
/// 做法：把 B 当作 A 的子序列去匹配（贪心取最早位置），
/// A 中没被匹配上的就是"消失的 token"。这样一次比较同时抓住三类问题：
/// 出现的**新**结构（B 里有 A 没有的）、**顺序变化**、以及**删了不该删的元素**。
fn compare_skeleton(a: &[String], b: &[String]) -> (Vec<String>, Vec<String>) {
    let mut deleted: Vec<String> = Vec::new();
    let mut bad: Vec<String> = Vec::new();
    let mut i = 0usize;

    for tb in b {
        while i < a.len() && &a[i] != tb {
            deleted.push(a[i].clone());
            i += 1;
        }
        if i >= a.len() {
            if bad.len() < 20 {
                bad.push(format!("出现了原本没有的结构：{tb}"));
            }
            continue;
        }
        i += 1;
    }
    while i < a.len() {
        deleted.push(a[i].clone());
        i += 1;
    }

    for d in &deleted {
        if !is_allowed_empty_elem(d) && bad.len() < 20 {
            bad.push(format!("删除了不允许删除的元素：{d}"));
        }
    }

    (deleted, bad)
}

/// 允许被删除的元素——**只有** run 内的空元素：
/// 命中的文本里含 `-`（`noBreakHyphen`）或制表符、换行时，删掉元素本身是唯一能"去掉它"的办法。
///
/// 其它任何元素（`<w:r>`、`<w:rPr>`、`<w:p>`、`<w:tbl>` …）被删掉都必须判不通过。
fn is_allowed_empty_elem(tok: &str) -> bool {
    let t = tok.trim_end();
    if !t.ends_with("/>") {
        return false; // 带子元素的开始标签不是空元素
    }
    let inner = &t[1..t.len() - 2];
    // 先切掉属性部分，再剥命名空间前缀
    let name_part = inner.split_whitespace().next().unwrap_or(inner);
    let local = name_part.rsplit(':').next().unwrap_or(name_part);
    matches!(
        local,
        "noBreakHyphen" | "tab" | "br" | "cr" | "softHyphen"
    )
}

/// 把一份 XML 归一成骨架 token 流，并统计三类承载元素数量。
fn skeleton(xml: &str) -> Result<(Vec<String>, CarrierCounts)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = false;

    let mut out: Vec<String> = Vec::new();
    let mut carriers: CarrierCounts = [0, 0, 0];
    // 元素 local name 栈（用于判断"承载元素的父节点是不是 run"）
    let mut stack: Vec<String> = Vec::new();
    // 正在跳过的承载元素：记录进入时的栈深度
    let mut skip_depth: Option<usize> = None;

    loop {
        let ev = reader.read_event().context("XML 解析失败")?;
        match ev {
            Event::Eof => break,
            Event::Start(e) => {
                let local = e.local_name().as_ref().to_string();
                if skip_depth.is_none() {
                    let parent_is_run = stack.last().map(|s| s.as_str()) == Some("r");
                    match (parent_is_run, carrier_slot(&local)) {
                        (true, Some(k)) => {
                            carriers[k] += 1;
                            skip_depth = Some(stack.len());
                        }
                        _ => out.push(format!("<{}>", tag_of(&e))),
                    }
                }
                stack.push(local);
            }
            Event::Empty(e) => {
                if skip_depth.is_none() {
                    let local = e.local_name().as_ref().to_string();
                    let parent_is_run = stack.last().map(|s| s.as_str()) == Some("r");
                    match (parent_is_run, carrier_slot(&local)) {
                        (true, Some(k)) => carriers[k] += 1,
                        _ => out.push(format!("<{}/>", tag_of(&e))),
                    }
                }
            }
            Event::End(e) => {
                let local = e.local_name().as_ref().to_string();
                let closing_skip_root =
                    matches!(skip_depth, Some(d) if d + 1 == stack.len());
                stack.pop();
                if closing_skip_root {
                    skip_depth = None;
                } else if skip_depth.is_none() {
                    out.push(format!("</{local}>"));
                }
            }
            Event::Text(_) | Event::CData(_) => {
                if skip_depth.is_none() {
                    out.push("#text".to_string());
                }
            }
            _ => {}
        }
    }

    Ok((out, carriers))
}

/// 元素在骨架里的标识：限定名 + 全部属性（顺序原样保留，值不转义处理）。
fn tag_of(e: &BytesStart<'_>) -> String {
    let mut s: String = e.name().as_ref().to_string();
    for a in e.attributes().flatten() {
        let k: &str = a.key.as_ref();
        s.push(' ');
        s.push_str(k);
        s.push('=');
        s.push_str(&a.value);
    }
    s
}

// ═══════════════════════════ 残留自检 ═══════════════════════════
//
// 「关卡 1 / 2」证明的是**格式没被动**，证明不了**该改的都改了**。
// 这一层补上后半句：拿规则里的「查找内容」回头扫**整个包**
// （含 docProps / rels / 所有 xml），看还有没有漏网的。
//
// 为什么必须扫全包而不只看可见文本：编号与客户名会出现在
// `docProps/core.xml` 的标题、`app.xml` 的公司字段里，这些不是
// 「文本容器 part」，只看正文会漏判成"干净"。
//
// 为什么复用 `engine::fold` / `find_all` 而不是 `str::contains`：
// 规则可能开着「不区分大小写」或「不分全半角」，用字面 contains
// 会把本该算残留的写法漏掉，反过来也可能误报。必须与替换时同一套语义。

/// 一条规则在产物里的残留。
#[derive(Debug, Clone)]
pub struct ResidueEntry {
    pub rule_id: u32,
    pub needle: String,
    pub count: usize,
    /// 其中落在**可见文本**（正文/页眉页脚/脚注/批注的 `w:t`）里的处数。
    ///
    /// `count - in_text` 全在 XML 标记与属性里（`w:author`、`docProps`、
    /// `settings.xml` 的 rsid 十六进制……）。**只看 in_text 才代表"真的没改到"**，
    /// 所以这一栏必须报告出来，否则用户会被总数字吓到却找不到病根。
    pub in_text: usize,
    /// 出现位置（part 名，最多记 5 处）
    pub where_: Vec<String>,
}

/// 残留自检报告。
#[derive(Debug, Clone, Default)]
pub struct ResidueReport {
    /// 扫过的 part 数（含非文本容器）
    pub parts_scanned: usize,
    pub entries: Vec<ResidueEntry>,
}

impl ResidueReport {
    /// 干干净净 = 所有规则的「查找内容」在产物里一次都不剩。
    pub fn clean(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn total(&self) -> usize {
        self.entries.iter().map(|e| e.count).sum()
    }
    /// 其中落在**可见文本**里的处数——这才是"真的没改到"。
    pub fn text_total(&self) -> usize {
        self.entries.iter().map(|e| e.in_text).sum()
    }
}

/// 取出一个包里**所有可参与匹配的文本**：文本容器走扫描器（跨 run 拼接后），
/// 其余 `.xml` / `.rels` 直接当纯文本过一遍。
/// 返回 `(part 名, 参与匹配的文本, 是否文本容器)`。
fn matchable_parts(path: &Path) -> Result<Vec<(String, String, bool)>> {
    // 一次开容器，把"要扫的"和"要原样看的"一起读出来。
    //
    // 原实现是三次重复劳动：`text_parts()` 走一遍 `inspect()`（全包解压 + 逐个 SHA），
    // 紧接着 `inspect()` 再来一遍，然后每个 part 又各 `read_part` 一次——
    // 一个文件开 N+2 次容器。这里合成一次。
    let raw = package::read_parts_where(path, |name, kind| {
        kind.is_text_bearing() || name.ends_with(".xml") || name.ends_with(".rels")
    })?;

    let mut out = Vec::new();
    for (name, kind, bytes) in raw {
        let Ok(s) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if kind.is_text_bearing() {
            // 文本容器：只取可见文本（`w:t` 等真实渲染出来的字），
            // 这样"查找内容"在 XML 标记里出现不算残留。
            let (paras, _) = scan::scan_part(&name, s)?;
            let joined = paras
                .iter()
                .map(|p| p.visible.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            out.push((name, joined, true));
        } else {
            // 参数/关系/样式：整份文本都算数，逐字找
            out.push((name, s.to_string(), false));
        }
    }
    Ok(out)
}

fn count_with_rule(text: &str, r: &Rule) -> usize {
    let needle = engine::fold(&r.find, r.case_sensitive, r.kana_sensitive);
    if needle.is_empty() {
        return 0;
    }
    let hay = engine::fold(text, r.case_sensitive, r.kana_sensitive);
    engine::find_all(&hay, &needle, r.whole_word).len()
}

/// 在整包里找这些规则的「查找内容」还剩多少。
pub fn residue(path: &Path, rules: &[Rule]) -> Result<ResidueReport> {
    let parts = matchable_parts(path)?;
    let mut rep = ResidueReport {
        parts_scanned: parts.len(),
        entries: Vec::new(),
    };

    for r in rules.iter().filter(|r| r.enabled) {
        if r.scope.filename && !r.scope.body && !r.scope.header_footer && !r.scope.textbox
            && !r.scope.footnote && !r.scope.comment
        {
            continue; // 纯文件名规则，包里本来就不该有
        }
        let mut count = 0usize;
        let mut in_text = 0usize;
        let mut where_: Vec<String> = Vec::new();
        for (name, text, is_text) in &parts {
            let c = count_with_rule(text, r);
            if c > 0 {
                count += c;
                if *is_text {
                    in_text += c;
                }
                if where_.len() < 5 {
                    where_.push(format!("{name} ×{c}"));
                }
            }
        }
        if count > 0 {
            rep.entries.push(ResidueEntry {
                rule_id: r.id,
                needle: r.find.clone(),
                count,
                in_text,
                where_,
            });
        }
    }
    // **按处数从多到少排**。报告里的「备注」只列前几条，若按规则号排，
    // 排在前面的永远是小数目条目，真正的元凶（往往是 `settings.xml` 里
    // 成千上万个 rsid 撞上裸数字规则）反而被截掉，用户会读错病因。
    rep.entries.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| b.in_text.cmp(&a.in_text))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
    });
    Ok(rep)
}

/// 统计每条规则的「查找内容」(`use_replace = false`) 或「替换为」
/// (`use_replace = true`) 在整包里的出现次数。
///
/// 用途：对比处理前后——`替换为` 的增量应当等于实际替换数。
pub fn count_in_package(path: &Path, rules: &[Rule], use_replace: bool) -> Result<Vec<(u32, usize)>> {
    let parts = matchable_parts(path)?;
    let mut out = Vec::new();
    for r in rules.iter().filter(|r| r.enabled) {
        let probe = if use_replace {
            Rule {
                find: r.replace.clone(),
                replace: String::new(),
                ..r.clone()
            }
        } else {
            r.clone()
        };
        if probe.find.is_empty() {
            out.push((r.id, 0));
            continue;
        }
        let total: usize = parts.iter().map(|(_, t, _)| count_with_rule(t, &probe)).sum();
        out.push((r.id, total));
    }
    Ok(out)
}

// ═══════════════════════════ 批量验证 ═══════════════════════════

/// 一组（处理前, 处理后）的验证结论。
#[derive(Debug, Clone)]
pub struct PairVerdict {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub l1_pass: bool,
    pub l2_pass: bool,
    pub changed_parts: Vec<String>,
    pub note: String,
}

impl PairVerdict {
    pub fn ok(&self) -> bool {
        self.l1_pass && self.l2_pass
    }
}

/// 验一对（原件, 产物），返回裁决。
///
/// `verify_pairs` 和 `pipeline` 的**写副本**路径共用这一个实现——
/// 那条路径上原件仍在磁盘上，按路径读两侧即可。
///
/// **就地替换不走这里**：它的原件在改写那一刻就没了，必须用
/// [`verify_snap`]（左侧是写盘前扣下的快照）。两条路最终都进 [`judge`]，
/// 结论的措辞与判据只有那一份。
pub fn verify_one(a: &Path, b: &Path) -> PairVerdict {
    let sa = match snapshot(a).context("读左侧文件失败") {
        Ok(v) => v,
        Err(e) => {
            return failed(a, b, format!("关卡1 读取失败：{e:#}"));
        }
    };
    let sb = match snapshot(b).context("读右侧文件失败") {
        Ok(v) => v,
        Err(e) => {
            return failed(a, b, format!("关卡2 读取失败：{e:#}"));
        }
    };
    judge(
        &compare_level1(&sa, &sb),
        &compare_level2(&sa, &sb),
        a,
        b,
    )
}

/// 用**写盘前扣下的左侧快照**验一个产物。
///
/// 就地替换专用：`before` 是原文件被覆盖之前的快照，`after` 是覆盖之后的同一路径。
pub fn verify_snap(before: &PkgSnap, after: &Path) -> PairVerdict {
    let sb = match snapshot(after).context("读右侧文件失败") {
        Ok(v) => v,
        Err(e) => {
            return failed(after, after, format!("关卡2 读取失败：{e:#}"));
        }
    };
    judge(
        &compare_level1(before, &sb),
        &compare_level2(before, &sb),
        after,
        after,
    )
}

fn failed(src: &Path, dst: &Path, note: String) -> PairVerdict {
    PairVerdict {
        src: src.to_path_buf(),
        dst: dst.to_path_buf(),
        l1_pass: false,
        l2_pass: false,
        changed_parts: Vec::new(),
        note,
    }
}

/// 由两份报告得出裁决——**结论措辞只在这里产生**。
fn judge(x: &L1Report, y: &L2Report, a: &Path, b: &Path) -> PairVerdict {
    let (l1_pass, l2_pass, changed, note) = (
        x.pass(),
        y.pass(),
        x.changed().iter().map(|p| p.name.clone()).collect(),
        if x.pass() && y.pass() {
            String::new()
        } else {
            let mut m = Vec::new();
            if !x.pass() {
                let f: Vec<String> = x.forbidden().iter().map(|p| p.name.clone()).collect();
                if !f.is_empty() {
                    m.push(format!("关卡1：不应改动的 part 被改 {}", f.join(", ")));
                }
                if !x.only_a.is_empty() {
                    m.push(format!("关卡1：仅原文件有 {}", x.only_a.join(", ")));
                }
                if !x.only_b.is_empty() {
                    m.push(format!("关卡1：仅产物有 {}", x.only_b.join(", ")));
                }
            }
            if !y.pass() {
                let bad: Vec<String> = y
                    .parts
                    .iter()
                    .filter(|p| !p.pass)
                    .flat_map(|p| p.bad.iter().cloned())
                    .take(3)
                    .collect();
                m.push(format!("关卡2：{}", bad.join("；")));
            }
            m.join("　")
        },
    );

    PairVerdict {
        src: a.to_path_buf(),
        dst: b.to_path_buf(),
        l1_pass,
        l2_pass,
        changed_parts: changed,
        note,
    }
}

/// 一次验一整批，替代"外部循环调 verify"。
///
/// 串行版本：给命令行 `verify --batch` 用（那里还要做结构指纹配对，
/// 顺序本身有语义）。`pipeline` 的批量验证走并行版，两者底层是同一次
/// [`verify_one`]，结论必须一致。
pub fn verify_pairs(pairs: &[(PathBuf, PathBuf)]) -> Vec<PairVerdict> {
    pairs.iter().map(|(a, b)| verify_one(a, b)).collect()
}

// ─────────────────────────── 改名后的批量配对 ───────────────────────────

/// 一份 docx 的「结构指纹」——**与文字内容无关**。
///
/// 组成：part 名单 + 每个 part 的指纹。非文本 part 用原始字节 SHA；
/// 文本 part 用**骨架**哈希（`skeleton()` 已把文本承载元素归一成占位符，
/// 属性/层级/元素间空白全部保留）。
///
/// 因此：同一模板只改文字、甚至把文件改了名，指纹仍然**完全不变**。
/// 这正是「产物已改名」时仍能把原件与产物正确配对的依据。
pub fn structure_signature(path: &Path) -> Result<String> {
    let pa = package::inspect(path).with_context(|| format!("读 {} 失败", path.display()))?;
    let mut items: Vec<(String, String)> = Vec::with_capacity(pa.len());

    for x in &pa {
        let fp = if x.kind.is_text_bearing() {
            match package::read_part(path, &x.name).and_then(|raw| {
                std::str::from_utf8(&raw)
                    .map(|s| s.to_string())
                    .map_err(|e| anyhow::anyhow!("{e}"))
            }) {
                Ok(s) => match skeleton(&s) {
                    Ok((toks, carriers)) => {
                        let mut h = Sha256::new();
                        for t in &toks {
                            h.update(t.as_bytes());
                            h.update(b"\n");
                        }
                        h.update(
                            format!("|c/{}/{}/{}", carriers[0], carriers[1], carriers[2])
                                .as_bytes(),
                        );
                        hex::encode(h.finalize())
                    }
                    // 骨架解析失败：退化为字节指纹（宁可配不上，也不误配）
                    Err(_) => format!("!skel!{}", x.sha256),
                },
                Err(_) => format!("!utf8!{}", x.sha256),
            }
        } else {
            x.sha256.clone()
        };
        items.push((x.name.clone(), fp));
    }

    items.sort();
    let mut h = Sha256::new();
    for (n, fp) in &items {
        h.update(n.as_bytes());
        h.update(b"=");
        h.update(fp.as_bytes());
        h.update(b"\n");
    }
    Ok(hex::encode(h.finalize()))
}

/// 批量配对的完整结论。
#[derive(Debug, Default)]
pub struct BatchPairing {
    /// 最终配对：(原件, 产物)
    pub pairs: Vec<(PathBuf, PathBuf)>,
    /// 其中靠「同名」配上的条数
    pub by_name: usize,
    /// 其中靠「结构指纹」配上的条数（产物已改名）
    pub by_signature: usize,
    /// 指纹歧义，未配对的说明（如"2 个原件指纹相同"）
    pub ambiguous: Vec<String>,
    /// 没能配上的产物
    pub unmatched_products: Vec<PathBuf>,
    /// 没能配上的原件
    pub unmatched_originals: Vec<PathBuf>,
}

fn docx_files_under(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for e in walkdir::WalkDir::new(dir).max_depth(8).follow_links(false) {
        let e = e.with_context(|| format!("遍历目录失败：{}", dir.display()))?;
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        if !p
            .extension()
            .map(|s| s.eq_ignore_ascii_case("docx"))
            .unwrap_or(false)
        {
            continue;
        }
        let rel = p
            .strip_prefix(dir)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        out.push((rel, p.to_path_buf()));
    }
    out.sort();
    Ok(out)
}

/// 把「原件目录」与「产物目录」配对。
///
/// 两级策略，都不做含糊的猜测：
/// 1. **同名**（相对路径相同）优先——最可靠；
/// 2. 剩下的按 **结构指纹** 配：同一模板只改文字时骨架不变，所以改了名也能配上；
///    但只在「该指纹在原件里唯一、在产物里也唯一」时才配对，否则记为歧义不配。
pub fn pair_dirs(dir_a: &Path, dir_b: &Path) -> Result<BatchPairing> {
    let a_files = docx_files_under(dir_a)?;
    let b_files = docx_files_under(dir_b)?;

    let mut a_by_rel: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (rel, p) in &a_files {
        a_by_rel.insert(rel.clone(), p.clone());
    }
    let mut b_by_rel: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (rel, p) in &b_files {
        b_by_rel.insert(rel.clone(), p.clone());
    }

    let mut rep = BatchPairing::default();

    // ① 同名优先
    for (rel, ap) in &a_by_rel {
        if let Some(bp) = b_by_rel.get(rel) {
            rep.pairs.push((ap.clone(), bp.clone()));
            rep.by_name += 1;
        }
    }

    // ② 剩下的按结构指纹
    let mut rest_a: Vec<(String, PathBuf)> = Vec::new();
    for (rel, p) in &a_by_rel {
        if !b_by_rel.contains_key(rel) {
            rest_a.push((rel.clone(), p.clone()));
        }
    }
    let mut rest_b: Vec<(String, PathBuf)> = Vec::new();
    for (rel, p) in &b_by_rel {
        if !a_by_rel.contains_key(rel) {
            rest_b.push((rel.clone(), p.clone()));
        }
    }

    // 指纹 → 名单（指纹算不出来的记为 None，绝不参与配对）
    let mut sig_a: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    for (rel, p) in &rest_a {
        if let Ok(s) = structure_signature(p) {
            sig_a.entry(s).or_default().push((rel.clone(), p.clone()));
        } else {
            rep.ambiguous
                .push(format!("原件指纹读取失败（不参与配对）：{rel}"));
        }
    }
    let mut sig_b: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    for (rel, p) in &rest_b {
        if let Ok(s) = structure_signature(p) {
            sig_b.entry(s).or_default().push((rel.clone(), p.clone()));
        } else {
            rep.ambiguous
                .push(format!("产物指纹读取失败（不参与配对）：{rel}"));
        }
    }

    let mut matched_a: HashSet<String> = HashSet::new();
    let mut matched_b: HashSet<String> = HashSet::new();

    for (sig, av) in &sig_a {
        let Some(bv) = sig_b.get(sig) else { continue };
        if av.len() == 1 && bv.len() == 1 {
            rep.pairs.push((av[0].1.clone(), bv[0].1.clone()));
            rep.by_signature += 1;
            matched_a.insert(av[0].0.clone());
            matched_b.insert(bv[0].0.clone());
        } else {
            rep.ambiguous.push(format!(
                "结构指纹相同、无法唯一配对：原件 {} 个（{}）↔ 产物 {} 个（{}）",
                av.len(),
                av.iter()
                    .map(|(r, _)| r.as_str())
                    .collect::<Vec<_>>()
                    .join("、"),
                bv.len(),
                bv.iter()
                    .map(|(r, _)| r.as_str())
                    .collect::<Vec<_>>()
                    .join("、"),
            ));
        }
    }

    for (rel, p) in &rest_a {
        if !matched_a.contains(rel) {
            rep.unmatched_originals.push(p.clone());
        }
    }
    for (rel, p) in &rest_b {
        if !matched_b.contains(rel) {
            rep.unmatched_products.push(p.clone());
        }
    }

    rep.pairs.sort();
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_slots_are_distinct() {
        assert_eq!(carrier_slot("t"), Some(0));
        assert_eq!(carrier_slot("delText"), Some(1));
        assert_eq!(carrier_slot("instrText"), Some(2));
        assert_eq!(carrier_slot("rPr"), None);
    }

    #[test]
    fn skeleton_drops_text_keeps_structure() {
        let a = r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>甲</w:t></w:r></w:p>"#;
        let b = r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>乙丙丁</w:t></w:r></w:p>"#;
        let (ta, _) = skeleton(a).unwrap();
        let (tb, _) = skeleton(b).unwrap();
        assert_eq!(ta, tb, "只改文字时骨架必须相同");

        let c = r#"<w:p><w:r><w:rPr><w:i/></w:rPr><w:t>乙</w:t></w:r></w:p>"#;
        let (tc, _) = skeleton(c).unwrap();
        assert_ne!(ta, tc, "格式标记变了骨架必须不同");
    }

    #[test]
    fn skeleton_counts_carriers() {
        let a = r#"<w:p><w:r><w:t>x</w:t></w:r><w:r><w:delText>y</w:delText></w:r></w:p>"#;
        let (_, c) = skeleton(a).unwrap();
        assert_eq!(c[0], 1);
        assert_eq!(c[1], 1);
    }
}
