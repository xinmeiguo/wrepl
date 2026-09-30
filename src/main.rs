//! wrepl —— Word 批量替换工具（命令行前端）
//!
//! 设计原则见 `04-重写实现/批量替换工具-设计规格-v0.2.md`。
//! 一句话：只改该改的字节，其余 part 按原始压缩字节整块搬运。
//!
//! 实质逻辑全在 `wrepl` 库里，这里只负责收集参数、调度、打印。

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use wrepl::cli::{Cli, Cmd, RuleSource, RulesCmd, Targets};
use wrepl::docx::package::{self, PartInfo, PartKind};
use wrepl::docx::scan::{self, ScanStats};
use wrepl::rules::{self, Rule};
use wrepl::{pipeline, probe, report, verify};

fn main() {
    if let Err(e) = run() {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Inspect { file } => cmd_inspect(&file),
        Cmd::Passthrough { input, output } => cmd_passthrough(&input, &output),
        Cmd::Diff { a, b } => cmd_diff(&a, &b),
        Cmd::Dump {
            file,
            part,
            grep,
            limit,
            special,
        } => cmd_dump(&file, part.as_deref(), grep.as_deref(), limit, special),
        Cmd::Scan { targets, rules } => cmd_scan(&targets, &rules),
        Cmd::Apply {
            targets,
            rules,
            out,
            in_place,
            backup,
            report,
            dry_run,
            rename_files,
            verify_after,
            longest_first,
        } => cmd_apply(            &targets,
            &rules,
            out.as_deref(),
            in_place,
            backup,
            report.as_deref(),
            dry_run,
            rename_files,
            verify_after,
            longest_first,
        ),
        Cmd::Rules { sub } => cmd_rules(&sub),
        Cmd::Probe {
            dir,
            recursive,
            out,
        } => cmd_probe(&dir, recursive, out.as_deref()),
        Cmd::Verify {
            a,
            b,
            level1_only,
            batch,
        } => cmd_verify(&a, &b, level1_only, batch),
    }
}

// ─────────────────────────────── 规则装载 ───────────────────────────────

fn load_rules(rs: &RuleSource) -> Result<(Vec<Rule>, Option<String>)> {
    let mut defaults = Rule::new(0, "", "");
    defaults.scope = rules::Scope::parse(&rs.scope)?;
    defaults.case_sensitive = rs.case_sensitive;
    defaults.whole_word = rs.whole_word;
    defaults.kana_sensitive = rs.kana_sensitive;

    let mut out: Vec<Rule> = Vec::new();
    let mut id = 1u32;
    // 规则表实际用了哪张工作表（报告里要写清楚，多表工作簿尤其重要）
    let mut used_book: Option<String> = None;

    for a in &rs.rule {
        out.push(rules::from_cli_arg(a, id, &defaults)?);
        id += 1;
    }
    if let Some(f) = &rs.rules_file {
        let more = rules::from_text_file(f, &defaults, id)?;
        id += more.len() as u32;
        out.extend(more);
    }
    if let Some(b) = &rs.rules_book {
        // 表名不固定：从第一张工作表起往后找，取第一张读得出条款的（见 rules::from_xlsx_book）
        let (more, warns, sheet) =
            rules::from_xlsx_book(b, &rules::XlsxLayout::all(), &defaults, id)?;
        for w in &warns {
            eprintln!("⚠ {w}");
        }
        println!("Excel 规则表：{}〔工作表「{sheet}」〕读出 {} 条", b.display(), more.len());
        // 这是最后一批：之后不再需要 id，不必累加
        out.extend(more);
        used_book = Some(sheet);
    }

    if out.is_empty() {
        bail!("没有可用规则：请用 --rule \"查找=>替换\"、--rules-file 或 --rules-book 指定");
    }
    Ok((out, used_book))
}

fn print_rules(rules: &[Rule], chain: bool, longest_first: bool) {
    println!(
        "═══ 规则 {} 条　执行模式：{} ═══",
        rules.len(),
        if chain {
            "链式（前一条输出即后一条输入）".to_string()
        } else if longest_first {
            "各自独立基于原文，区间重叠按「最长匹配优先」裁决".to_string()
        } else {
            "各自独立基于原文，区间重叠报冲突（两条都不改）".to_string()
        }
    );
    for r in rules {
        let flag = if r.enabled { " " } else { "（已禁用）" };
        println!("  {}{}", r.summary(), flag);
    }
    println!();
}

fn cmd_scan(t: &Targets, rs: &RuleSource) -> Result<()> {
    let (rl, _) = load_rules(rs)?;
    print_rules(&rl, rs.chain, false);

    let opts = pipeline::Options {
        longest_first: false,
        recursive: t.recursive,
        exclude: t.exclude.clone(),
        chain: rs.chain,
        out_dir: None,
        in_place: false,
        backup: false,
        dry_run: true,
        rename_files: false,
        verify_after: false,
        threads: t.threads,
    };

    let (files, skipped) = pipeline::collect_targets(&t.paths, &opts)?;
    if files.is_empty() {
        println!("没有找到可处理的 .docx 文件。");
        print_skipped(&skipped);
        return Ok(());
    }
    println!("待扫描文件：{} 个\n", files.len());

    // 并行下多个文件同时打印会互相咬行：拿锁把"一个文件的整段输出"圈成一块。
    // 顺序按完成先后，不保证等于目录顺序——所以文件序号仍打出来。
    let out_lock = Mutex::new(());
    let res = pipeline::run(&t.paths, &rl, &opts, &|i, total, o| {
        let _guard = out_lock.lock().unwrap_or_else(|e| e.into_inner());
        println!("─── [{i}/{total}] {} ───", o.src.display());
        render_outcome(o);
    })?;

    println!("并行：{} 线程", res.threads);
    print_totals(&res.outcomes);
    print_skipped(&res.skipped);
    Ok(())
}

/// 执行替换。
///
/// 实质动作（找文件 / 改写 / 改名 / 验证）全在 [`pipeline`] 里，
/// 这里只做**参数映射**和**结果打印**——GUI 走的是同一条路。
#[allow(clippy::too_many_arguments)]
fn cmd_apply(
    t: &Targets,
    rs: &RuleSource,
    out: Option<&Path>,
    in_place: bool,
    backup: bool,
    report_path: Option<&Path>,
    dry: bool,
    rename_files: bool,
    verify_after: bool,
    longest_first: bool,
) -> Result<()> {
    let (rl, book_sheet) = load_rules(rs)?;
    print_rules(&rl, rs.chain, longest_first);

    // 链式模式是"前一条的输出当下一条的输入"，规则先后作用，本来就不存在重叠，
    // 所以「最长匹配优先」在链式下无事可做。同时给两个是语义矛盾，明确拒绝而非静默忽略。
    if longest_first && rs.chain {
        bail!(
            "--longest-first 与 --chain 不能同时使用：\
             链式模式下规则是先后作用的，不存在区间重叠，也就无从裁决。请二选一。"
        );
    }

    // 没给 --out 也没给 --in-place：**默认就地替换源文件**。
    // 这是刻意的默认：拿到一批要归一编号/客户名的交付包时，
    // 通常就是要把手上这批文件改掉，再输出一份副本反而还得手工搬回去。
    // 备份默认**不留**（源目录干净）；要安全垫就加 --backup，正本始终在原文件上改写。
    let in_place = in_place || out.is_none();
    if in_place && !dry {
        println!(
            "⚠ 就地替换源文件（{}）；要输出副本请加 --out <目录>\n",
            if backup {
                "每个文件先写一份 .docx.bak 备份"
            } else {
                "直接覆盖原文件、不留备份，需要时加 --backup"
            }
        );
    }

    let opts = pipeline::Options {
        recursive: t.recursive,
        exclude: t.exclude.clone(),
        chain: rs.chain,
        out_dir: out.map(Path::to_path_buf),
        in_place,
        backup,
        dry_run: dry,
        rename_files,
        verify_after,
        threads: t.threads,
        longest_first,
    };

    let out_desc = if dry {
        "（dry-run，不落盘）".to_string()
    } else if in_place {
        if backup {
            "就地替换源文件（保留 .bak 备份）".to_string()
        } else {
            "就地替换源文件（不留备份）".to_string()
        }
    } else {
        out.map(|d| d.display().to_string()).unwrap_or_default()
    };

    let announced = AtomicBool::new(false); // 并行下只许印一次
    let out_lock = Mutex::new(());
    let res = pipeline::run(&t.paths, &rl, &opts, &|i, total, o| {
        let _guard = out_lock.lock().unwrap_or_else(|e| e.into_inner());
        if !announced.swap(true, Ordering::Relaxed) {
            println!("待处理文件：{total} 个　输出：{out_desc}\n");
        }
        println!("─── [{i}/{total}] {} ───", o.src.display());
        render_outcome(o);
    })?;

    if res.outcomes.is_empty() {
        println!("没有找到可处理的 .docx 文件。");
    }
    println!("并行：{} 线程", res.threads);
    print_totals(&res.outcomes);
    print_skipped(&res.skipped);

    // ── 改名与验证的摘要（动作已在库里执行完，这里只报数）──
    if rename_files && !dry {
        if !rl.iter().any(|r| r.enabled && r.scope.filename) {
            println!("提示：没有任何规则勾选「文件名」作用域，文件名不会变化。");
        }
        let renamed = res.name_rows.iter().filter(|n| n.changed).count();
        if renamed > 0 {
            println!("文件名同步改名：{renamed} 个");
        }
        // 目标名已被占用时是**加序号而不是覆盖**（保护已有文件）。
        // 不说一声的话，用户只会看到产物目录里多出 `xxx (2).docx` 这类文件而不知为何。
        let occupied = res
            .name_rows
            .iter()
            .filter(|n| n.note.contains("已被占用"))
            .count();
        if occupied > 0 {
            println!(
                "注意：{occupied} 个目标文件名在输出目录里已存在，已自动加序号（不覆盖已有文件）"
            );
        }
    } else if rename_files && dry {
        println!("（dry-run：未执行文件名同步改名，故「文件名对照」表为空）");
    }

    if verify_after && !dry {
        let bad = res.verify_bad();
        println!(
            "\n自动验证：{} 个文件　通过 {} / 不通过 {}",
            res.verify_rows.len(),
            res.verify_rows.len() - bad,
            bad
        );
        for v in res.verify_rows.iter().filter(|v| !v.ok()) {
            println!("   ✗ {}：{}", v.file, v.note);
        }
    }

    let Some(rp) = report_path else {
        return Ok(());
    };

    // ─────────────────────────── 报告 ───────────────────────────
    let mut file_rows: Vec<report::FileRow> = res
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
            sha_before: short(&o.sha_before),
            sha_after: short(&o.sha_after),
            parts_changed: o.parts.join(" "),
            note: o.note.clone(),
        })
        .collect();

    // 被跳过的文件也写进报告——QA 归档必须能看出"哪些文件没处理、为什么没处理"
    for s in &res.skipped {
        let md = std::fs::metadata(&s.path).ok();
        file_rows.push(report::FileRow {
            file: s
                .path
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_string(),
            path: s.path.display().to_string(),
            size_kb: md.as_ref().map(|m| m.len() as f64 / 1024.0).unwrap_or(0.0),
            mtime: md
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| pipeline::format_epoch(d.as_secs()))
                .unwrap_or_default(),
            rule_count: rl.len(),
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
            let find = rl
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

    // ── 正式报告：封面元信息 + 规则快照，便于 QA 归档自证 ──
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let operator = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "(未知)".to_string());

    let mut rule_source = String::new();
    if let Some(f) = &rs.rules_file {
        rule_source = format!("文本规则文件：{}", f.display());
    }
    if let Some(b) = &rs.rules_book {
        if !rule_source.is_empty() {
            rule_source.push_str("　＋　");
        }
        // 写明实际采用的工作表：多表工作簿里"用错了表"是最难查的一类问题
        match &book_sheet {
            Some(s) => rule_source.push_str(&format!("Excel 规则库：{}〔工作表「{s}」〕", b.display())),
            None => rule_source.push_str(&format!("Excel 规则库：{}", b.display())),
        }
    }
    if !rs.rule.is_empty() {
        if !rule_source.is_empty() {
            rule_source.push_str("　＋　");
        }
        rule_source.push_str(&format!("命令行逐条（--rule {} 条）", rs.rule.len()));
    }
    if rs.chain {
        rule_source.push_str("　［链式模式 --chain］");
    }
    if longest_first {
        rule_source.push_str("　［区间重叠裁决：最长匹配优先 --longest-first］");
    }

    let total_hits = res.total_hits();
    let total_applied = res.total_applied();
    let total_conflicts = res.total_conflicts();
    let failed = res.failed();
    let verify_bad = res.verify_bad();
    let name_warn = res.name_rows.iter().filter(|n| !n.note.is_empty()).count();

    let verdict = if failed > 0 {
        format!("不合格：{failed} 个文件处理失败，需查因后重跑")
    } else if verify_bad > 0 {
        format!("不合格：{verify_bad} 个文件未通过执行后自动验证（见「验证结论」表）")
    } else if total_conflicts > 0 {
        format!("有条件通过：{total_conflicts} 处区间冲突已跳过，须人工复核")
    } else if name_warn > 0 {
        format!("有条件通过：文件名改名有 {name_warn} 条需人工确认（见「文件名对照」表）")
    } else {
        let extra = if res.verify_rows.is_empty() {
            String::new()
        } else {
            format!("；执行后验证 {} 个文件全部通过", res.verify_rows.len())
        };
        format!("合格：命中 {total_hits} 处、替换 {total_applied} 处，无冲突{extra}")
    };

    let meta = report::ReportMeta {
        tool: "wrepl".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        generated_at: pipeline::format_epoch(now),
        operator,
        input: t
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("　；　"),
        output: out_desc.clone(),
        rule_source,
        mode: {
            let base = if dry {
                "试运行（dry-run，未写入任何文件）"
            } else if in_place {
                if backup {
                    "就地覆盖（原文件保留 .bak 备份）"
                } else {
                    "就地覆盖（原文件上直接覆盖，不留备份）"
                }
            } else {
                "写出到新目录（原文件不动）"
            };
            let mut m = base.to_string();
            if rename_files {
                m.push_str("　＋　文件名同步改名");
            }
            if verify_after && !dry {
                m.push_str("　＋　执行后自动验证");
            }
            m
        },
        verdict,
    };

    let rule_rows: Vec<report::RuleRow> = rl
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
        rp,
        &meta,
        &file_rows,
        &hit_rows,
        &rule_rows,
        &res.name_rows,
        &res.verify_rows,
    )?;
    println!("\n报告已写出：{}", rp.display());

    Ok(())
}

fn short(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        s.chars().take(16).collect()
    }
}

fn render_outcome(o: &pipeline::Outcome) {
    println!(
        "  状态：{}　命中 {}（替换 {} / 冲突 {}）{}",
        o.status,
        o.hits.len(),
        o.applied,
        o.conflicts,
        if o.parts.is_empty() {
            String::new()
        } else {
            format!("　改动 part：{}", o.parts.join(", "))
        }
    );
    if !o.note.is_empty() {
        println!("  备注：{}", o.note);
    }
    if let Some(d) = &o.dst {
        if o.status == "OK" {
            println!("  输出：{}", d.display());
        }
    }

    let cap = 60usize;
    for h in o.hits.iter().take(cap) {
        let mark = if h.applied { "✔" } else { "✗" };
        println!(
            "    {mark} 规则#{} [{}] 第{}段  「{}」→「{}」{}",
            h.rule_id,
            h.slot.label(),
            h.para,
            truncate(&h.matched, 40),
            truncate(&h.replaced_by, 24),
            if h.applied {
                format!("（{}）", h.strategy)
            } else {
                format!("　{}", h.reason)
            }
        );
    }
    if o.hits.len() > cap {
        println!("    …另有 {} 条命中未显示", o.hits.len() - cap);
    }
    println!();
}

fn truncate(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= n {
        s.to_string()
    } else {
        format!("{}…", c[..n].iter().collect::<String>())
    }
}

fn print_totals(outcomes: &[pipeline::Outcome]) {
    let mut by: HashMap<&str, usize> = HashMap::new();
    let mut hits = 0usize;
    let mut applied = 0usize;
    let mut conflicts = 0usize;
    for o in outcomes {
        *by.entry(o.status).or_insert(0) += 1;
        hits += o.hits.len();
        applied += o.applied;
        conflicts += o.conflicts;
    }
    println!("────────────────────── 汇总 ──────────────────────");
    println!(
        "文件 {} ｜ 命中 {} ｜ 已替换 {} ｜ 冲突 {}",
        outcomes.len(),
        hits,
        applied,
        conflicts
    );
    let mut keys: Vec<&&str> = by.keys().collect();
    keys.sort();
    let parts: Vec<String> = keys
        .iter()
        .map(|k| format!("{k} {}", by[**k]))
        .collect();
    if !parts.is_empty() {
        println!("状态：{}", parts.join(" ｜ "));
    }
    println!();
}

fn print_skipped(skipped: &[pipeline::Skipped]) {
    if skipped.is_empty() {
        return;
    }
    println!("跳过 {} 个文件：", skipped.len());
    for s in skipped {
        println!("  · {}　→ {}", s.path.display(), s.reason);
    }
    println!();
}

// ─────────────────────────────── 规则管理 ───────────────────────────────

fn cmd_rules(sub: &RulesCmd) -> Result<()> {
    match sub {
        RulesCmd::Template { out, rule } => {
            match out {
                Some(p) => {
                    let is_xlsx = p
                        .extension()
                        .and_then(|s| s.to_str())
                        .map(|s| s.eq_ignore_ascii_case("xlsx"))
                        .unwrap_or(false);
                    if is_xlsx {
                        // 列布局默认全开。第 1 列 = 查找内容，第 2 列 = 替换为，
                        // 后面是可选列，**没有序号列**。
                        let layout = rules::XlsxLayout::all();
                        let headers = layout.headers();
                        let rules: Vec<Rule> = if rule.is_empty() {
                            rules::template_rules()
                        } else {
                            let defaults = Rule::new(0, "", "");
                            rule.iter()
                                .enumerate()
                                .map(|(i, a)| rules::from_cli_arg(a, i as u32 + 1, &defaults))
                                .collect::<Result<Vec<_>>>()?
                        };
                        let rows: Vec<Vec<report::Cell>> = rules
                            .iter()
                            .map(|r| {
                                rules::xlsx_row(&layout, r)
                                    .iter()
                                    .map(|c| report::Cell::text(c))
                                    .collect()
                            })
                            .collect();
                        report::write_single_sheet(p, "规则", &headers, rows)?;
                        println!("已生成 Excel 规则模板：{}", p.display());
                    } else {
                        let defaults = Rule::new(0, "", "");
                        let rules: Vec<Rule> = rule
                            .iter()
                            .enumerate()
                            .map(|(i, a)| rules::from_cli_arg(a, i as u32 + 1, &defaults))
                            .collect::<Result<Vec<_>>>()?;
                        std::fs::write(p, rules::dump_to_string(&rules))
                            .with_context(|| format!("写入失败：{}", p.display()))?;
                        println!("已生成文本规则模板：{}", p.display());
                    }
                }
                None => {
                    println!("# 每行格式（制表符分隔）：查找内容\t替换为\t作用域\t选项\t备注");
                    println!("# 作用域：正文,页眉页脚,文本框,脚注,批注（或 全部）");
                    println!("# 选项：case(区分大小写), whole(全字匹配), kana(区分全半角), off(禁用)");
                    println!("# 也可以直接写成：查找=>替换");
                    println!("# 以 # 开头的行和空行会被忽略");
                }
            }
            Ok(())
        }
        RulesCmd::Dump { rules: rs, out } => {
            let (rl, _) = load_rules(rs)?;
            let text = rules::dump_to_string(&rl);
            match out {
                Some(p) => {
                    std::fs::write(p, &text)
                        .with_context(|| format!("写入失败：{}", p.display()))?;
                    println!("已导出 {} 条规则到 {}", rl.len(), p.display());
                }
                None => print!("{text}"),
            }
            Ok(())
        }
        RulesCmd::Check { rules: rs } => {
            let (rl, _) = load_rules(rs)?;
            println!("✓ 规则解析通过，共 {} 条：", rl.len());
            for r in &rl {
                println!(
                    "  {}{}{}",
                    r.summary(),
                    if r.note.is_empty() {
                        String::new()
                    } else {
                        format!("　// {}", r.note)
                    },
                    if r.origin.is_empty() {
                        String::new()
                    } else {
                        format!("　[{}]", r.origin)
                    }
                );
            }
            Ok(())
        }
    }
}

// ─────────────────────────────── 验证 ───────────────────────────────

fn cmd_verify(a: &Path, b: &Path, level1_only: bool, batch: bool) -> Result<()> {
    if batch {
        return cmd_verify_batch(a, b);
    }
    println!("验证：");
    println!("  A（处理前） {}", a.display());
    println!("  B（处理后） {}", b.display());
    println!();

    println!("### 关卡 1　part 级字节比对");
    let l1 = verify::level1(a, b)?;
    println!(
        "  part 总数 {}　内容一致 {}　有差异 {}",
        l1.parts.len(),
        l1.parts.iter().filter(|p| p.same).count(),
        l1.changed().len()
    );
    for p in l1.changed() {
        println!(
            "  · [{}] {}　{} B → {} B",
            p.kind.label(),
            p.name,
            p.size_a,
            p.size_b
        );
    }
    if !l1.only_a.is_empty() {
        println!("  ⚠ 仅 A 有：{}", l1.only_a.join(", "));
    }
    if !l1.only_b.is_empty() {
        println!("  ⚠ 仅 B 有：{}", l1.only_b.join(", "));
    }
    let forbidden = l1.forbidden();
    if forbidden.is_empty() {
        println!("  ✓ 非文本容器 part（styles / settings / media …）零改动");
    } else {
        println!("  ✗ 以下**不应改动**的 part 发生了变化：");
        for p in forbidden {
            println!("      {}", p.name);
        }
    }
    println!(
        "  关卡 1 结论：{}",
        if l1.pass() { "通过" } else { "不通过" }
    );
    println!();

    if level1_only {
        if !l1.pass() {
            std::process::exit(2);
        }
        return Ok(());
    }

    println!("### 关卡 2　XML 语义骨架比对（忽略文本承载元素的内容）");
    println!("  判定准则：除「run 内空元素被删除」外，骨架必须逐一完全相同。");
    let l2 = verify::level2(a, b)?;
    for p in &l2.parts {
        println!(
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
        println!(
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
            println!("      删除的元素：{}", list.join("　"));
        }
        for b in p.bad.iter().take(8) {
            println!("      ✗ {b}");
        }
        if p.bad.len() > 8 {
            println!("      …另有 {} 条异常", p.bad.len() - 8);
        }
    }
    println!(
        "  关卡 2 结论：{}",
        if l2.pass() { "通过" } else { "不通过" }
    );
    println!();

    let ok = l1.pass() && l2.pass();
    println!(
        "{}",
        if ok {
            "══════ 总判定：通过（格式标记层分毫未动）══════"
        } else {
            "══════ 总判定：不通过 ══════"
        }
    );
    if !ok {
        std::process::exit(2);
    }
    Ok(())
}

// ───────────────────────────── 侦察 probe ─────────────────────────────

/// 扫目录，列出候选项目编号与客户名，替代手写侦察脚本。
///
/// 生成的「建议规则文件」**故意把替换列留空、并把规则标成 `off`（禁用）**：
/// 空替换的语义是"删除"，若直接执行会把查找内容整段删掉。
/// 填好替换列、去掉 `off` 才可执行——这个门槛是刻意留的。
fn cmd_probe(dir: &Path, recursive: bool, out: Option<&Path>) -> Result<()> {
    if !dir.is_dir() {
        bail!("不是目录：{}", dir.display());
    }
    let rep = probe::probe_dir(dir, recursive)?;

    println!("侦察目录：{}", dir.display());
    println!(
        "扫描 .docx {} 个{}",
        rep.files_scanned,
        if recursive { "（已递归子目录）" } else { "" }
    );
    if rep.files_scanned == 0 {
        println!("\n该目录下没找到 .docx。");
        print_probe_skipped(&rep);
        return Ok(());
    }
    println!();

    let section = |title: &str, list: &[probe::Candidate], hint: &str| {
        if list.is_empty() {
            return;
        }
        println!("### {title}（{hint}）");
        for c in list {
            println!(
                "  {:>5} 处　{:>3} 个文件　{}",
                c.count, c.files, c.text
            );
        }
        println!();
    };

    section("候选项目编号（正文）", &rep.codes_in_text, "替换前先确认要改成哪个");
    section("候选项目编号（文件名）", &rep.codes_in_names, "文件名里出现，可配合 --rename-files");
    section("候选客户名 · 中文", &rep.names_cn, "改成新客户名");
    section("候选客户名 · 英文", &rep.names_en, "注意与中文成对改，别只改一半");

    print_probe_skipped(&rep);

    let sug = rep.suggested();
    if sug.is_empty() {
        println!("没有扫出可用的候选串（可能这批文件里没有项目编号/客户名）。");
        return Ok(());
    }

    match out {
        Some(p) => {
            let mut text = String::new();
            text.push_str("# wrepl 侦察建议规则 —— 由 `wrepl probe` 生成\n");
            text.push_str("#\n");
            text.push_str("# ⚠ 替换列目前是空的，空替换的语义是「删除」。\n");
            text.push_str("#   所有规则都已标为 off（禁用），直接执行不会有任何改动。\n");
            text.push_str("#   请：① 在替换列填上目标文本　② 去掉选项列的 off　再执行。\n");
            text.push_str("#\n");
            text.push_str("# 格式：查找内容\t替换为\t作用域\t选项\t备注\n");
            for (t, kind) in &sug {
                text.push_str(&format!("{t}\t\t全部\toff\t{kind}\n"));
            }
            std::fs::write(p, &text).with_context(|| format!("写入失败：{}", p.display()))?;
            println!("已写出建议规则 {} 条：{}", sug.len(), p.display());
            println!("（替换列留空 + 全部 off，填好再跑）");
        }
        None => {
            println!("建议规则 {} 条（用 --out <文件> 可导出成规则文件）：", sug.len());
            for (t, kind) in &sug {
                println!("  {t}\t→（待填）\t{kind}");
            }
        }
    }
    Ok(())
}

fn print_probe_skipped(rep: &probe::ProbeReport) {
    if rep.skipped.is_empty() {
        return;
    }
    println!("### 未纳入扫描的文件（{} 个）", rep.skipped.len());
    for (p, why) in &rep.skipped {
        println!(
            "  {}　{}",
            p.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
            why
        );
    }
    println!();
}

// ───────────────────────────── 批量验证 ─────────────────────────────

/// `verify --batch`：A、B 两个目录，成对逐个验证。
///
/// 配对是两级策略：
/// 1. **相对路径同名**优先；
/// 2. 剩下的按**结构指纹**配 —— 同一模板只改文字时骨架不变，
///    所以即使执行时开了「文件名同步改名」，也照样能配上。
///    只在两个方向都唯一时才配对，绝不猜；歧义项单独列出。
fn cmd_verify_batch(dir_a: &Path, dir_b: &Path) -> Result<()> {
    if !dir_a.is_dir() || !dir_b.is_dir() {
        bail!(
            "--batch 模式下 A、B 必须都是目录（当前：{} / {}）",
            dir_a.display(),
            dir_b.display()
        );
    }

    let pr = verify::pair_dirs(dir_a, dir_b)?;

    if pr.pairs.is_empty() {
        println!("没能在两个目录间配对到任何 .docx。");
        for n in pr.unmatched_products.iter().take(20) {
            println!("  · 产物未能配对：{}", short_path(n, dir_b));
        }
        for m in pr.ambiguous.iter().take(10) {
            println!("  ! {m}");
        }
        return Ok(());
    }

    println!("批量验证：{} 组", pr.pairs.len());
    println!("  A（处理前） {}", dir_a.display());
    println!("  B（处理后） {}", dir_b.display());
    println!(
        "  配对方式：同名 {} ｜ 结构指纹（已改名）{}",
        pr.by_name, pr.by_signature
    );
    println!();

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
            println!("  ✓ {name}{tag}");
        } else {
            println!("  ✗ {name}{tag}");
            if !v.note.is_empty() {
                println!("      {}", v.note);
            }
        }
    }

    println!();
    println!(
        "══════ 关卡 1+2：通过 {pass} / 不通过 {} （共 {}）══════",
        verdicts.len() - pass,
        verdicts.len()
    );

    if !pr.unmatched_products.is_empty() {
        println!(
            "另有 {} 个产物文件未能配对（未验证）：",
            pr.unmatched_products.len()
        );
        for n in pr.unmatched_products.iter().take(10) {
            println!("  · {}", short_path(n, dir_b));
        }
    }
    if !pr.unmatched_originals.is_empty() {
        println!(
            "另有 {} 个原件没有对应产物（未验证）：",
            pr.unmatched_originals.len()
        );
        for n in pr.unmatched_originals.iter().take(10) {
            println!("  · {}", short_path(n, dir_a));
        }
    }
    for m in pr.ambiguous.iter().take(10) {
        println!("  ! {m}");
    }

    if pass != verdicts.len() {
        std::process::exit(2);
    }
    Ok(())
}

/// 把绝对路径缩回成相对某个根目录的显示名。
fn short_path(p: &Path, root: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

// ─────────────────────────────── 早期子命令 ───────────────────────────────

fn cmd_inspect(file: &Path) -> Result<()> {
    let parts = package::inspect(file)?;

    println!("文件：{}", file.display());
    println!("part 总数：{}", parts.len());
    println!();

    let text_parts: Vec<&PartInfo> = parts.iter().filter(|p| p.kind.is_text_bearing()).collect();
    println!("文本容器 part（会被替换引擎扫描）：{}", text_parts.len());
    for p in &text_parts {
        println!(
            "  [{}] {:<28} {:>9} B  {}",
            p.kind.label(),
            p.name,
            p.uncompressed_size,
            &p.sha256[..16]
        );
    }
    println!();

    let other: Vec<&PartInfo> = parts.iter().filter(|p| !p.kind.is_text_bearing()).collect();
    println!("其它 part（一律不动）：{}", other.len());
    for p in other {
        println!(
            "  {:<40} {:>9} B  {:>7} B压缩  {}  {}",
            p.name,
            p.uncompressed_size,
            p.compressed_size,
            p.method,
            &p.sha256[..16]
        );
    }
    Ok(())
}

fn cmd_passthrough(input: &Path, output: &Path) -> Result<()> {
    println!("零改动透传：");
    println!("  输入 {}", input.display());
    println!("  输出 {}", output.display());
    println!();

    let before = package::inspect(input).context("读取输入指纹失败")?;
    package::passthrough(input, output)?;
    let after = package::inspect(output).context("读取输出指纹失败")?;

    if before.len() != after.len() {
        println!("✗ part 数量不一致：{} → {}", before.len(), after.len());
        std::process::exit(2);
    }

    let mut bad = 0usize;
    for (b, a) in before.iter().zip(after.iter()) {
        if b.name != a.name || b.sha256 != a.sha256 {
            bad += 1;
            println!("✗ 差异：{}", b.name);
            if b.sha256 != a.sha256 {
                println!("    前 {}", &b.sha256[..24]);
                println!("    后 {}", &a.sha256[..24]);
            }
        }
        if b.method != a.method {
            println!("⚠ 压缩方式变化：{} {} → {}", b.name, b.method, a.method);
        }
    }

    println!();
    if bad == 0 {
        println!(
            "✓ 全部 {} 个 part 内容 SHA256 完全一致 —— 零改动透传验收通过",
            before.len()
        );
        Ok(())
    } else {
        println!("✗ 共 {} 个 part 发生变化 —— 验收失败", bad);
        std::process::exit(2);
    }
}

fn cmd_diff(a: &Path, b: &Path) -> Result<()> {
    let diffs = package::diff_parts(a, b)?;
    if diffs.is_empty() {
        println!("✓ 两个文件全部 part 内容一致");
    } else {
        println!("发现 {} 处 part 差异：", diffs.len());
        for (name, sa, sb) in diffs {
            println!("  {name}");
            println!("    左 {}", &sa[..sa.len().min(24)]);
            println!("    右 {}", &sb[..sb.len().min(24)]);
        }
    }
    Ok(())
}

fn cmd_dump(
    file: &Path,
    part: Option<&str>,
    grep: Option<&str>,
    limit: usize,
    special: bool,
) -> Result<()> {
    let targets: Vec<(String, PartKind)> = match part {
        Some(p) => vec![(p.to_string(), PartKind::from_name(p))],
        None => package::text_parts(file)?,
    };

    println!("文件：{}", file.display());
    println!("扫描 part：{} 个", targets.len());
    println!();

    let mut total = ScanStats::default();

    for (name, kind) in &targets {
        let bytes = package::read_part(file, name)?;
        let xml = std::str::from_utf8(&bytes)
            .with_context(|| format!("{name} 不是 UTF-8 文本，无法做偏移扫描"))?;
        let (paras, stats) = scan::scan_part(name, xml)?;
        total.merge(&stats);

        println!("══════════════════════════════════════════════════════════════");
        println!(
            "[{}] {}   段落 {} / run {} / 可见字符 {}",
            kind.label(),
            name,
            stats.paragraphs,
            stats.runs,
            stats.visible_chars
        );
        println!(
            "    文本节点 {}（含 preserve {}）  跨 run 段落 {}  空元素 {}  delText {}  instrText {}  文本框内段落 {}",
            stats.text_nodes,
            stats.preserve_space_nodes,
            stats.split_paragraphs,
            stats.empty_elem_nodes,
            stats.del_text_nodes,
            stats.instr_text_nodes,
            stats.textbox_paragraphs
        );
        println!("══════════════════════════════════════════════════════════════");

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
            render_para(p, special);
            shown += 1;
        }
        if limit > 0 && matched > shown {
            println!();
            println!("  …还有 {} 段未显示（--limit 0 可全部显示）", matched - shown);
        }
        println!();
    }

    println!("────────────────────── 汇总 ──────────────────────");
    println!(
        "段落 {} ｜ run {} ｜ 可见字符 {} ｜ 文本节点 {}",
        total.paragraphs, total.runs, total.visible_chars, total.text_nodes
    );
    println!(
        "跨 run 段落 {} ｜ 空元素节点 {} ｜ delText {} ｜ instrText {} ｜ 文本框内段落 {} ｜ preserve 节点 {}",
        total.split_paragraphs,
        total.empty_elem_nodes,
        total.del_text_nodes,
        total.instr_text_nodes,
        total.textbox_paragraphs,
        total.preserve_space_nodes
    );
    if total.split_paragraphs > 0 {
        println!();
        println!(
            "⚠ {} 个段落的可见文本被切分在多个 <w:t> 上——这些段落若按单个 <w:t> 查找会静默漏掉，",
            total.split_paragraphs
        );
        println!("  正是本模块存在的理由（上面报告里的「字符 → 字节」映射即为其解决方案）。");
    }
    Ok(())
}

fn render_para(p: &scan::Para, special: bool) {
    println!();
    println!(
        "── #{}  字节[{}..{})  run {}  可见 {} 字符{}",
        p.index,
        p.elem_start,
        p.elem_end,
        p.run_count,
        p.visible.chars().count(),
        if p.in_textbox { "  [文本框内]" } else { "" }
    );
    if !p.visible.is_empty() {
        println!("     可见: {}", scan::display_visible(&p.visible));
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
        println!(
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
            Some(sp) => println!(
                "     ↳ 跨 run：可见文本跨 {} 个 <w:t>，最小外包字节区间 {}..{}（{} 字节）",
                p.visible_text_nodes(),
                sp.start,
                sp.end,
                sp.end - sp.start
            ),
            None => println!(
                "     ↳ 跨 run：区间首尾落在空元素上，需显式处理（空元素无法用删字节表达）"
            ),
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
        println!("{s}");
    }
}
