//! 诊断工具：六件「看一眼」的事，命令行与图形界面共用一份实现。
//!
//! | 工具 | 回答什么问题 |
//! |---|---|
//! | [`inspect`] | 这个 docx 里有哪些 part，各多大、指纹是多少 |
//! | [`passthrough`] | 读进来再写出去，字节会不会变（拷贝链路的体检） |
//! | [`diff`] | 两个 docx 的 part 级差异在哪 |
//! | [`dump`] | 每个段落是怎么被切成 run 的（人工核对偏移） |
//! | [`probe`] | 一个目录里出现过哪些项目编号 / 客户名（出规则前的侦察） |
//! | [`verify`] | 处理前后逐 part 比字节 + 比 XML 骨架（关卡 1+2） |
//!
//! ## 为什么在这里写，而不是留在 `main.rs`
//!
//! 这些能力命令行一直有、界面一直没有入口（见 `docs/ROADMAP.md` §7.1）。
//! 关键不在"加个按钮"，而在**同一件事只能有一份实现**：如果界面另写一套，
//! 同一个文件在两边会得出不同的结论，那才是最难查的那种 bug。
//!
//! 所以函数**只产出文本**，不碰 stdout —— 命令行打到终端，界面显示在结果区，
//! 一份 `String` 两处用。

use crate::docx::package::{self, PartInfo, PartKind};
use crate::docx::scan::{self, ScanStats};
use crate::{probe, verify};
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::path::Path;

/// 一次工具运行的结果。
///
/// `text` 是给人看的整段报告；`ok` 是**结论**（命令行的退出码、界面上那行
/// 「通过 / 不通过」都由它决定）。这两件事分开，是因为文本里当然可以写
/// "不通过"，但不该让调用方去解析自己的输出来判断成败。
#[derive(Debug)]
pub struct ToolOutput {
    pub text: String,
    pub ok: bool,
}

/// 列出 docx 内部所有 part 及其指纹。
pub fn inspect(file: &Path) -> Result<ToolOutput> {
    let parts = package::inspect(file)?;
    let mut text = String::new();

    let _ = writeln!(text, "文件：{}", file.display());
    let _ = writeln!(text, "part 总数：{}", parts.len());
    let _ = writeln!(text);

    let text_parts: Vec<&PartInfo> = parts.iter().filter(|p| p.kind.is_text_bearing()).collect();
    let _ = writeln!(
        text,
        "文本容器 part（会被替换引擎扫描）：{}",
        text_parts.len()
    );
    for p in &text_parts {
        let _ = writeln!(
            text,
            "  [{}] {:<28} {:>9} B  {}",
            p.kind.label(),
            p.name,
            p.uncompressed_size,
            &p.sha256[..16]
        );
    }
    let _ = writeln!(text);

    let other: Vec<&PartInfo> = parts.iter().filter(|p| !p.kind.is_text_bearing()).collect();
    let _ = writeln!(text, "其它 part（一律不动）：{}", other.len());
    for p in other {
        let _ = writeln!(
            text,
            "  {:<40} {:>9} B  {:>7} B压缩  {}  {}",
            p.name,
            p.uncompressed_size,
            p.compressed_size,
            p.method,
            &p.sha256[..16]
        );
    }
    Ok(ToolOutput { text, ok: true })
}

/// 零改动透传：读入再写出，逐 part 比 SHA256。
///
/// 这条是**拷贝链路的体检** —— 替换逻辑再对，只要"读一遍写一遍"就会动字节，
/// 产物照样不能交付。
pub fn passthrough(input: &Path, output: &Path) -> Result<ToolOutput> {
    let mut text = String::new();
    let _ = writeln!(text, "零改动透传：");
    let _ = writeln!(text, "  输入 {}", input.display());
    let _ = writeln!(text, "  输出 {}", output.display());
    let _ = writeln!(text);

    let before = package::inspect(input).context("读取输入指纹失败")?;
    // 过程提示插在报告中间（"读了什么 / 写了什么"之后、"比得怎样"之前），
    // 与命令行时代的位置一致 —— 那几行是复原记账字段的证据，不是收尾附注。
    for n in package::passthrough(input, output)? {
        let _ = writeln!(text, "{n}");
    }
    let after = package::inspect(output).context("读取输出指纹失败")?;

    if before.len() != after.len() {
        let _ = writeln!(
            text,
            "✗ part 数量不一致：{} → {}",
            before.len(),
            after.len()
        );
        return Ok(ToolOutput { text, ok: false });
    }

    let mut bad = 0usize;
    for (b, a) in before.iter().zip(after.iter()) {
        if b.name != a.name || b.sha256 != a.sha256 {
            bad += 1;
            let _ = writeln!(text, "✗ 差异：{}", b.name);
            if b.sha256 != a.sha256 {
                let _ = writeln!(text, "    前 {}", &b.sha256[..24]);
                let _ = writeln!(text, "    后 {}", &a.sha256[..24]);
            }
        }
        if b.method != a.method {
            let _ = writeln!(
                text,
                "⚠ 压缩方式变化：{} {} → {}",
                b.name, b.method, a.method
            );
        }
    }

    let _ = writeln!(text);
    if bad == 0 {
        let _ = writeln!(
            text,
            "✓ 全部 {} 个 part 内容 SHA256 完全一致 —— 零改动透传验收通过",
            before.len()
        );
        Ok(ToolOutput { text, ok: true })
    } else {
        let _ = writeln!(text, "✗ 共 {} 个 part 发生变化 —— 验收失败", bad);
        Ok(ToolOutput { text, ok: false })
    }
}

/// 比对两个 docx 的 part 级差异。
pub fn diff(a: &Path, b: &Path) -> Result<ToolOutput> {
    let diffs = package::diff_parts(a, b)?;
    let mut text = String::new();
    if diffs.is_empty() {
        let _ = writeln!(text, "✓ 两个文件全部 part 内容一致");
    } else {
        let _ = writeln!(text, "发现 {} 处 part 差异：", diffs.len());
        for (name, sa, sb) in diffs {
            let _ = writeln!(text, "  {name}");
            let _ = writeln!(text, "    左 {}", &sa[..sa.len().min(24)]);
            let _ = writeln!(text, "    右 {}", &sb[..sb.len().min(24)]);
        }
    }
    Ok(ToolOutput { text, ok: true })
}

/// 导出「段落 → run 切分」报告。
pub fn dump(
    file: &Path,
    part: Option<&str>,
    grep: Option<&str>,
    limit: usize,
    special: bool,
) -> Result<ToolOutput> {
    let targets: Vec<(String, PartKind)> = match part {
        Some(p) => vec![(p.to_string(), PartKind::from_name(p))],
        None => package::text_parts(file)?,
    };

    let mut text = String::new();
    let _ = writeln!(text, "文件：{}", file.display());
    let _ = writeln!(text, "扫描 part：{} 个", targets.len());
    let _ = writeln!(text);

    let mut total = ScanStats::default();

    for (name, kind) in &targets {
        let bytes = package::read_part(file, name)?;
        let xml = std::str::from_utf8(&bytes)
            .with_context(|| format!("{name} 不是 UTF-8 文本，无法做偏移扫描"))?;
        let (paras, stats) = scan::scan_part(name, xml)?;
        total.merge(&stats);

        let _ = writeln!(
            text,
            "══════════════════════════════════════════════════════════════"
        );
        let _ = writeln!(
            text,
            "[{}] {}   段落 {} / run {} / 可见字符 {}",
            kind.label(),
            name,
            stats.paragraphs,
            stats.runs,
            stats.visible_chars
        );
        let _ = writeln!(
            text,
            "    文本节点 {}（含 preserve {}）  跨 run 段落 {}  空元素 {}  delText {}  instrText {}  文本框内段落 {}",
            stats.text_nodes,
            stats.preserve_space_nodes,
            stats.split_paragraphs,
            stats.empty_elem_nodes,
            stats.del_text_nodes,
            stats.instr_text_nodes,
            stats.textbox_paragraphs
        );
        let _ = writeln!(
            text,
            "══════════════════════════════════════════════════════════════"
        );

        let mut shown = 0usize;
        let mut matched = 0usize;
        for p in &paras {
            if let Some(g) = grep {
                if !p.visible.contains(g) {
                    continue;
                }
            }
            matched += 1;
            if limit > 0 && shown >= limit {
                continue;
            }
            render_para(&mut text, p, special);
            shown += 1;
        }
        if limit > 0 && matched > shown {
            let _ = writeln!(text);
            let _ = writeln!(
                text,
                "  …还有 {} 段未显示（--limit 0 可全部显示）",
                matched - shown
            );
        }
        let _ = writeln!(text);
    }

    let _ = writeln!(
        text,
        "────────────────────── 汇总 ──────────────────────"
    );
    let _ = writeln!(
        text,
        "段落 {} ｜ run {} ｜ 可见字符 {} ｜ 文本节点 {}",
        total.paragraphs, total.runs, total.visible_chars, total.text_nodes
    );
    let _ = writeln!(
        text,
        "跨 run 段落 {} ｜ 空元素节点 {} ｜ delText {} ｜ instrText {} ｜ 文本框内段落 {} ｜ preserve 节点 {}",
        total.split_paragraphs,
        total.empty_elem_nodes,
        total.del_text_nodes,
        total.instr_text_nodes,
        total.textbox_paragraphs,
        total.preserve_space_nodes
    );
    if total.split_paragraphs > 0 {
        let _ = writeln!(text);
        let _ = writeln!(
            text,
            "⚠ {} 个段落的可见文本被切分在多个 <w:t> 上——这些段落若按单个 <w:t> 查找会静默漏掉，",
            total.split_paragraphs
        );
        let _ = writeln!(
            text,
            "  正是本模块存在的理由（上面报告里的「字符 → 字节」映射即为其解决方案）。"
        );
    }
    Ok(ToolOutput { text, ok: true })
}

fn render_para(text: &mut String, p: &scan::Para, special: bool) {
    let _ = writeln!(text);
    let _ = writeln!(
        text,
        "── #{}  字节[{}..{})  run {}  可见 {} 字符{}",
        p.index,
        p.elem_start,
        p.elem_end,
        p.run_count,
        p.visible.chars().count(),
        if p.in_textbox { "  [文本框内]" } else { "" }
    );
    if !p.visible.is_empty() {
        let _ = writeln!(text, "     可见: {}", scan::display_visible(&p.visible));
    }

    for (i, n) in p.nodes.iter().enumerate() {
        if !special && !matches!(n.kind, scan::NodeKind::Text) {
            continue;
        }
        let mut note = String::new();
        if n.run_has_rpr {
            note.push_str(" rPr");
        }
        if n.preserve_space {
            note.push_str(" preserve");
        }
        // 空元素**没有文本节点**，印它的"内容区间"永远是空的、毫无意义；
        // 要印的是元素自身的字节区间。
        let (range, raw) = if n.kind.is_empty_elem() {
            (
                format!("elem b[{}..{})", n.elem_start, n.elem_end),
                format!(
                    "→ {:?}  ← 空元素，无文本节点",
                    n.kind.virtual_char().unwrap_or(' ')
                ),
            )
        } else {
            (
                format!("b[{}..{})", n.content_start, n.content_end),
                format!("{:?}", n.raw),
            )
        };
        let _ = writeln!(
            text,
            "     #{:<3} {}  {}  {}{}",
            i,
            n.kind.label(),
            range,
            raw,
            note
        );
    }

    if p.is_split_across_runs() {
        // 关键证据：整段可见文本对应的原始字节区间。
        // ⚠ 这个区间**中间夹着 `</w:t></w:r><w:r><w:rPr>…` 等标记**——
        // 直接整段覆盖会把中间的 run 属性（也就是格式）一起删掉。
        // 跨 run 替换必须**逐节点**分发，不能整段替换。
        match p.byte_span(0, p.map.len()) {
            Some(sp) => {
                let _ = writeln!(
                    text,
                    "     ↳ 跨 run：可见文本跨 {} 个 <w:t>，最小外包字节区间 {}..{}（{} 字节）",
                    p.visible_text_nodes(),
                    sp.start,
                    sp.end,
                    sp.end - sp.start
                );
            }
            None => {
                let _ = writeln!(
                    text,
                    "     ↳ 跨 run：区间首尾落在空元素上，需显式处理（空元素无法用删字节表达）"
                );
            }
        }
        let mut s = String::from("     ↳ 字符→字节: ");
        for (i, ch) in p.visible.chars().enumerate().take(24) {
            let r = p.map[i];
            if r.is_virtual() {
                s.push_str(&format!("{}@虚拟 ", ch));
            } else {
                s.push_str(&format!("{}@{} +{} ", ch, r.off, r.len));
            }
        }
        if p.visible.chars().count() > 24 {
            s.push_str("…");
        }
        let _ = writeln!(text, "{s}");
    }
}

/// 侦察：扫目录，列出候选项目编号与客户名。
///
/// 生成的「建议规则文件」**故意把替换列留空、并把规则标成 `off`（禁用）**：
/// 空替换的语义是"删除"，若直接执行会把查找内容整段删掉。
/// 填好替换列、去掉 `off` 才可执行——这个门槛是刻意留的。
pub fn probe_dir_report(
    dir: &Path,
    recursive: bool,
    out: Option<&Path>,
) -> Result<ToolOutput> {
    if !dir.is_dir() {
        bail!("不是目录：{}", dir.display());
    }
    let rep = probe::probe_dir(dir, recursive)?;

    let mut text = String::new();
    let _ = writeln!(text, "侦察目录：{}", dir.display());
    let _ = writeln!(
        text,
        "扫描 .docx {} 个{}",
        rep.files_scanned,
        if recursive { "（已递归子目录）" } else { "" }
    );
    if rep.files_scanned == 0 {
        let _ = writeln!(text, "\n该目录下没找到 .docx。");
        push_probe_skipped(&mut text, &rep);
        return Ok(ToolOutput { text, ok: true });
    }
    let _ = writeln!(text);

    let section = |text: &mut String, title: &str, list: &[probe::Candidate], hint: &str| {
        if list.is_empty() {
            return;
        }
        let _ = writeln!(text, "### {title}（{hint}）");
        for c in list {
            let _ = writeln!(
                text,
                "  {:>5} 处　{:>3} 个文件　{}",
                c.count, c.files, c.text
            );
        }
        let _ = writeln!(text);
    };

    section(
        &mut text,
        "候选项目编号（正文）",
        &rep.codes_in_text,
        "替换前先确认要改成哪个",
    );
    section(
        &mut text,
        "候选项目编号（文件名）",
        &rep.codes_in_names,
        "文件名里出现，可配合 --rename-files",
    );
    section(
        &mut text,
        "候选客户名 · 中文",
        &rep.names_cn,
        "改成新客户名",
    );
    section(
        &mut text,
        "候选客户名 · 英文",
        &rep.names_en,
        "注意与中文成对改，别只改一半",
    );

    push_probe_skipped(&mut text, &rep);

    let sug = rep.suggested();
    if sug.is_empty() {
        let _ = writeln!(
            text,
            "没有扫出可用的候选串（可能这批文件里没有项目编号/客户名）。"
        );
        return Ok(ToolOutput { text, ok: true });
    }

    match out {
        Some(p) => {
            let mut body = String::new();
            body.push_str("# wrepl 侦察建议规则 —— 由 `wrepl probe` 生成\n");
            body.push_str("#\n");
            body.push_str("# ⚠ 替换列目前是空的，空替换的语义是「删除」。\n");
            body.push_str("#   所有规则都已标为 off（禁用），直接执行不会有任何改动。\n");
            body.push_str("#   请：① 在替换列填上目标文本　② 去掉选项列的 off　再执行。\n");
            body.push_str("#\n");
            body.push_str("# 格式：查找内容\t替换为\t作用域\t选项\t备注\n");
            for (t, kind) in &sug {
                let _ = writeln!(body, "{t}\t\t全部\toff\t{kind}");
            }
            std::fs::write(p, &body).with_context(|| format!("写入失败：{}", p.display()))?;
            let _ = writeln!(text, "已写出建议规则 {} 条：{}", sug.len(), p.display());
            let _ = writeln!(text, "（替换列留空 + 全部 off，填好再跑）");
        }
        None => {
            let _ = writeln!(
                text,
                "建议规则 {} 条（用 --out <文件> 可导出成规则文件）：",
                sug.len()
            );
            for (t, kind) in &sug {
                let _ = writeln!(text, "  {t}\t→（待填）\t{kind}");
            }
        }
    }
    Ok(ToolOutput { text, ok: true })
}

fn push_probe_skipped(text: &mut String, rep: &probe::ProbeReport) {
    if rep.skipped.is_empty() {
        return;
    }
    let _ = writeln!(text, "### 未纳入扫描的文件（{} 个）", rep.skipped.len());
    for (p, why) in &rep.skipped {
        let _ = writeln!(
            text,
            "  {}　{}",
            p.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
            why
        );
    }
    let _ = writeln!(text);
}

/// 验证关卡 1 + 2。
///
/// `batch = true` 时 A、B 当目录、成对验证（见 [`verify_batch`]）。
pub fn verify(a: &Path, b: &Path, level1_only: bool, batch: bool) -> Result<ToolOutput> {
    if batch {
        return verify_batch(a, b);
    }

    let mut text = String::new();
    let _ = writeln!(text, "验证：");
    let _ = writeln!(text, "  A（处理前） {}", a.display());
    let _ = writeln!(text, "  B（处理后） {}", b.display());
    let _ = writeln!(text);

    let _ = writeln!(text, "### 关卡 1　part 级字节比对");
    let l1 = verify::level1(a, b)?;
    let _ = writeln!(
        text,
        "  part 总数 {}　内容一致 {}　有差异 {}",
        l1.parts.len(),
        l1.parts.iter().filter(|p| p.same).count(),
        l1.changed().len()
    );
    for p in l1.changed() {
        let _ = writeln!(
            text,
            "  · [{}] {}　{} B → {} B",
            p.kind.label(),
            p.name,
            p.size_a,
            p.size_b
        );
    }
    if !l1.only_a.is_empty() {
        let _ = writeln!(text, "  ⚠ 仅 A 有：{}", l1.only_a.join(", "));
    }
    if !l1.only_b.is_empty() {
        let _ = writeln!(text, "  ⚠ 仅 B 有：{}", l1.only_b.join(", "));
    }
    let forbidden = l1.forbidden();
    if forbidden.is_empty() {
        let _ = writeln!(
            text,
            "  ✓ 非文本容器 part（styles / settings / media …）零改动"
        );
    } else {
        let _ = writeln!(text, "  ✗ 以下**不应改动**的 part 发生了变化：");
        for p in forbidden {
            let _ = writeln!(text, "      {}", p.name);
        }
    }
    let _ = writeln!(
        text,
        "  关卡 1 结论：{}",
        if l1.pass() { "通过" } else { "不通过" }
    );
    let _ = writeln!(text);

    if level1_only {
        return Ok(ToolOutput {
            text,
            ok: l1.pass(),
        });
    }

    let _ = writeln!(text, "### 关卡 2　XML 语义骨架比对（忽略文本承载元素的内容）");
    let _ = writeln!(
        text,
        "  判定准则：除「run 内空元素被删除」外，骨架必须逐一完全相同。"
    );
    let l2 = verify::level2(a, b)?;
    for p in &l2.parts {
        let _ = writeln!(
            text,
            "  {}　{}　骨架 token {} → {}{}",
            if p.pass { "✓" } else { "✗" },
            p.name,
            p.tokens_a,
            p.tokens_b,
            if p.deleted.is_empty() {
                String::new()
            } else {
                format!("　删除 {} 个元素", p.deleted.len())
            }
        );
        let _ = writeln!(
            text,
            "      w:t {}→{}　delText {}→{}　instrText {}→{}",
            p.carriers_a[0],
            p.carriers_b[0],
            p.carriers_a[1],
            p.carriers_b[1],
            p.carriers_a[2],
            p.carriers_b[2]
        );
        if !p.deleted.is_empty() {
            let mut kinds: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for d in &p.deleted {
                *kinds.entry(d.as_str()).or_insert(0) += 1;
            }
            let list: Vec<String> = kinds.iter().map(|(k, v)| format!("{k} ×{v}")).collect();
            let _ = writeln!(text, "      删除的元素：{}", list.join("　"));
        }
        for b in p.bad.iter().take(8) {
            let _ = writeln!(text, "      ✗ {b}");
        }
        if p.bad.len() > 8 {
            let _ = writeln!(text, "      …另有 {} 条异常", p.bad.len() - 8);
        }
    }
    let _ = writeln!(
        text,
        "  关卡 2 结论：{}",
        if l2.pass() { "通过" } else { "不通过" }
    );
    let _ = writeln!(text);

    let ok = l1.pass() && l2.pass();
    let _ = writeln!(
        text,
        "{}",
        if ok {
            "══════ 总判定：通过（格式标记层分毫未动）══════"
        } else {
            "══════ 总判定：不通过 ══════"
        }
    );
    Ok(ToolOutput { text, ok })
}

/// `verify --batch`：A、B 两个目录，成对逐个验证。
///
/// 配对是两级策略：
/// 1. **相对路径同名**优先；
/// 2. 剩下的按**结构指纹**配 —— 同一模板只改文字时骨架不变，
///    所以即使执行时开了「文件名同步改名」，也照样能配上。
///    只在两个方向都唯一时才配对，绝不猜；歧义项单独列出。
pub fn verify_batch(dir_a: &Path, dir_b: &Path) -> Result<ToolOutput> {
    if !dir_a.is_dir() || !dir_b.is_dir() {
        // 措辞与命令行一致：界面里也照引 `--batch`，因为**它调的就是同一条路**
        bail!(
            "--batch 模式下 A、B 必须都是目录（当前：{} / {}）",
            dir_a.display(),
            dir_b.display()
        );
    }

    let pr = verify::pair_dirs(dir_a, dir_b)?;
    let mut text = String::new();

    if pr.pairs.is_empty() {
        let _ = writeln!(text, "没能在两个目录间配对到任何 .docx。");
        for n in pr.unmatched_products.iter().take(20) {
            let _ = writeln!(text, "  · 产物未能配对：{}", short_path(n, dir_b));
        }
        for m in pr.ambiguous.iter().take(10) {
            let _ = writeln!(text, "  ! {m}");
        }
        return Ok(ToolOutput { text, ok: true });
    }

    let _ = writeln!(text, "批量验证：{} 组", pr.pairs.len());
    let _ = writeln!(text, "  A（处理前） {}", dir_a.display());
    let _ = writeln!(text, "  B（处理后） {}", dir_b.display());
    let _ = writeln!(
        text,
        "  配对方式：同名 {} ｜ 结构指纹（已改名）{}",
        pr.by_name, pr.by_signature
    );
    let _ = writeln!(text);

    let verdicts = verify::verify_pairs(&pr.pairs);
    let mut pass = 0usize;
    for v in &verdicts {
        let name = short_path(&v.dst, dir_b);
        let src_name = v
            .src
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let dst_name = v
            .dst
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let tag = if src_name != dst_name {
            format!("  ← {src_name}")
        } else {
            String::new()
        };
        if v.ok() {
            pass += 1;
            let _ = writeln!(text, "  ✓ {name}{tag}");
        } else {
            let _ = writeln!(text, "  ✗ {name}{tag}");
            if !v.note.is_empty() {
                let _ = writeln!(text, "      {}", v.note);
            }
        }
    }

    let _ = writeln!(text);
    let _ = writeln!(
        text,
        "══════ 关卡 1+2：通过 {pass} / 不通过 {} （共 {}）══════",
        verdicts.len() - pass,
        verdicts.len()
    );

    if !pr.unmatched_products.is_empty() {
        let _ = writeln!(
            text,
            "另有 {} 个产物文件未能配对（未验证）：",
            pr.unmatched_products.len()
        );
        for n in pr.unmatched_products.iter().take(10) {
            let _ = writeln!(text, "  · {}", short_path(n, dir_b));
        }
    }
    if !pr.unmatched_originals.is_empty() {
        let _ = writeln!(
            text,
            "另有 {} 个原件没有对应产物（未验证）：",
            pr.unmatched_originals.len()
        );
        for n in pr.unmatched_originals.iter().take(10) {
            let _ = writeln!(text, "  · {}", short_path(n, dir_a));
        }
    }
    for m in pr.ambiguous.iter().take(10) {
        let _ = writeln!(text, "  ! {m}");
    }

    Ok(ToolOutput {
        text,
        ok: pass == verdicts.len(),
    })
}

/// 把绝对路径缩回成相对某个根目录的显示名。
fn short_path(p: &Path, root: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本文件头的表格说六个工具都在这儿 —— 少一个，界面就会退回"没有入口"
    /// 那半边（命令行有、界面没有），正是这条改动要消掉的不对称。
    ///
    /// 用纯文本清单钉住：加/删工具时这里必须跟着改，改的时候会想起来
    /// 「界面那一页也得加一档」。
    #[test]
    fn 六个工具都在本模块里() {
        let names = [
            "inspect", "passthrough", "diff", "dump", "probe", "verify",
        ];
        // 编译期就够了：这里只是把名字写出来，函数签名被人改名时这条会先红
        assert_eq!(names.len(), 6);
        let _: fn(&Path) -> Result<ToolOutput> = inspect;
        let _: fn(&Path, &Path) -> Result<ToolOutput> = diff;
        let _: fn(&Path, &Path, bool, bool) -> Result<ToolOutput> = verify;
    }

    /// inspect 的文本必须**说到三件事**：文件、part 总数、文本容器数。
    /// 少任何一件，拿这段输出截图给别人看时就得再补一句解释。
    ///
    /// 用不存在的文件：错误要带出路径（`package::inspect` 的 context），
    /// 别让界面显示一句"打开失败"。
    #[test]
    fn inspect_报错要带出路径() {
        let e = format!(
            "{:#}",
            inspect(Path::new("D:/test/不存在的文件.docx")).unwrap_err()
        );
        assert!(e.contains("不存在的文件.docx"), "错误里要有路径：{e}");
    }

    /// `verify` 的单文件模式与批量模式共用入口：A/B 都是文件时走关卡 1+2，
    /// 传目录 + `batch=false` 必须报错而不是静默返回空报告。
    #[test]
    fn verify_单文件模式遇到目录要报错() {
        let e = verify(Path::new("D:/test"), Path::new("D:/test"), false, false)
            .err()
            .expect("目录不是有效输入，应当报错");
        let msg = format!("{e:#}");
        assert!(!msg.is_empty(), "要给出原因");
    }
}
