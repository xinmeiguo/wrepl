//! wrepl —— Word 批量替换工具（命令行前端）
//!
//! 设计原则见 `04-重写实现/批量替换工具-设计规格-v0.2.md`。
//! 一句话：只改该改的字节，其余 part 按原始压缩字节整块搬运。
//!
//! 实质逻辑全在 `wrepl` 库里，这里只负责收集参数、调度、打印。

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use wrepl::cli::{Cli, Cmd, RuleSource, RulesCmd, Targets};
use wrepl::docx::scan;
use wrepl::rules::{self, Rule};
use wrepl::{pipeline, report, tools};

fn main() {
    if let Err(e) = run() {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}

/// 零参数运行时的提示 —— 主要服务于「在资源管理器里双击了 exe」的人。
///
/// 为什么要有它：命令行版双击后的行为本来就是「打印用法 → 退出」，
/// 但那个控制台窗口会**一闪就关**，用户根本来不及看清任何字，
/// 只留下「黑框闪退 = 这软件坏了」的印象。
///
/// 这里换成一段人话，并（只在连着终端时）等一个回车。
/// 只在**零参数**这一种情况下触发 —— 脚本化调用一定带参数，不受影响。
fn show_double_click_help() -> ! {
    println!(
        "\
wrepl —— Word 批量替换工具（命令行版）

你刚才运行的是「命令行版」，它不会自己弹出界面；
黑框一闪而过是正常的，不是出错。

  ▸ 想用图形界面：双击同一文件夹里的  wrepl-gui.exe
  ▸ 想用命令行  ：在 cmd / PowerShell 里运行  wrepl.exe --help

详细说明见同目录的 README.md。"
    );

    // 双击时 stdin 连着刚分配的控制台 → 停住等用户读完；
    // 被重定向（脚本里调用）时 is_terminal() 为假 → 直接返回，不会挂住。
    if std::io::stdin().is_terminal() {
        println!("\n按回车键关闭本窗口…");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    std::process::exit(2);
}

fn run() -> Result<()> {
    // ★ 零参数：先给双击的人一段看得懂的提示，别让它一闪而过（见函数注释）。
    if std::env::args_os().len() == 1 {
        show_double_click_help();
    }

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
        Cmd::Scan {
            targets,
            rules,
            longest_first,
        } => cmd_scan(&targets, &rules, longest_first),
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
            mirror,
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
            mirror,
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
    // XML 非法控制字符检查（--rule / --rules-file 这两路；Excel 那路在
    // from_xlsx_rows 里已带警告）。这类字符：在「查找内容」里则永远不命中，
    // 在「替换为」里则写入时被丢弃（报告显示原文、产物实际不含）——
    // 必须在装载时说清楚，见 scan::illegal_xml_chars 的注释。
    for r in &out {
        for (what, text) in [("查找内容", &r.find), ("替换为", &r.replace)] {
            let bad = scan::illegal_xml_chars(text);
            if !bad.is_empty() {
                let names: Vec<String> =
                    bad.iter().map(|c| format!("U+{:04X}", *c as u32)).collect();
                eprintln!(
                    "⚠ 规则 #{}「{}」含 XML 非法控制字符（{}）：{}",
                    r.id,
                    what,
                    names.join("、"),
                    if what == "查找内容" {
                        "正文里不可能存在，这条规则将永远不命中"
                    } else {
                        "写入产物时会被丢弃，实际替换结果不含它"
                    }
                );
            }
        }
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

fn cmd_scan(t: &Targets, rs: &RuleSource, longest_first: bool) -> Result<()> {
    let (rl, _) = load_rules(rs)?;
    print_rules(&rl, rs.chain, longest_first);

    // 预演与执行（apply）必须用同一套裁决方式：预演硬编码 false 而 apply 可开
    // --longest-first 时，会出现"预览说两条冲突都不改、执行却真改了一条"——
    // 预览就失去了"所见即所得"的意义。所以这里透传，与 cmd_apply 保持一致。
    // 链式 + longest_first 的矛盾检查与 apply 相同（链式下重叠无从谈起）。
    if longest_first && rs.chain {
        bail!(
            "--longest-first 与 --chain 不能同时使用：\
             链式模式下规则是先后作用的，不存在区间重叠，也就无从裁决。请二选一。"
        );
    }

    let opts = pipeline::Options {
        longest_first,
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
        mirror: false,
        // scan 不落盘、不出报告，文件级 SHA256 没有任何消费者
        file_sha: false,
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
    }, &|_, _, _| {})?;

    println!("并行：{} 线程", res.threads);
    print_totals(&res.outcomes);
    print_skipped(&res.skipped);
    Ok(())
}

/// 执行替换。
///
/// 实质动作（找文件 / 改写 / 改名 / 验证）全在 [`pipeline`] 里，
/// 这里只做**参数映射**和**结果打印**——GUI 走的是同一条路。
/// 跑完但存在失败项时以**退出码 2** 结束，见 [`exit_for_failures`]。
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
    mirror: bool,
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
        mirror,
        // 文件级 SHA256 是**报告**与**完整镜像自证**才用得上的东西，
        // 为它每个文件要整读两遍、算两次 SHA256（8.8 MB 的文件约 40 ms/次）。
        // 不给 --report、也不开 --mirror 时没人消费这两个值，就不算。
        // （mirror 那一支在 pipeline::run 里还会再强制打开一次，双保险。）
        file_sha: report_path.is_some() || mirror,
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
    }, &|_, _, _| {})?;

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
        exit_for_failures(&res);
        return Ok(());
    };

    // ─────────────────────────── 报告 ───────────────────────────
    // 三张明细表的行由 `report` 统一摊平，与界面共用同一份实现。
    let rows = report::rows_from_run(&res, &rl);

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

    report::write_xlsx(
        rp,
        &meta,
        &rows.files,
        &rows.hits,
        &rows.rules,
        &res.name_rows,
        &res.verify_rows,
    )?;
    println!("\n报告已写出：{}", rp.display());

    exit_for_failures(&res);
    Ok(())
}

/// 一批跑完了，但有文件报错或验证不通过时，用**退出码 2** 结束。
///
/// 为什么要有它：`cmd_apply` 以前无论结果如何都返回 `Ok(())`，退出码恒为 0。
/// 于是 `wrepl apply ... && echo 成功了` 这种链式调用，哪怕有文件处理失败、
/// 有产物验证不合格，也照样打印"成功了"——自动化脚本据此判断会全盘误判。
///
/// 码值约定与 `wrepl verify` 一致：**2 = 跑完了但结论是失败**（1 留给"根本没跑起来"
/// 的硬错误，见 `main`）。这样调用方既能区分"命令用错了"与"跑完了但有问题"，
/// 又能用最简单的 `if ! wrepl apply ...; then` 拦住。
fn exit_for_failures(res: &pipeline::RunResult) {
    let failed = res.failed();
    let verify_bad = res.verify_bad();
    if failed > 0 || verify_bad > 0 {
        println!(
            "\n退出码 2：{failed} 个文件处理失败、{verify_bad} 个产物验证不通过"
        );
        std::process::exit(2);
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

// ─────────────────────── 诊断工具（实现见 `wrepl::tools`）───────────────────────
//
// 这六件事的报告文本**由库里生成**：图形界面的「诊断工具」页调的是同一份函数，
// 只是把文本显示在窗口里，而不是打到这里来。
//
// 以前它们整段写在本文件里，界面就永远不会有入口（同一个工具、两种能力，
// 见 `docs/ROADMAP.md` §7.1）。搬走之后 `cmd_*` 只剩"选路 + 打印 + 退出码"
// 三件事 —— 那本来就是命令行前端的全部职责。

/// 把工具的结果打到 stdout，并按结论决定退出码（2 = 不通过）。
fn emit(o: wrepl::tools::ToolOutput) -> Result<()> {
    use std::io::Write as _;
    print!("{}", o.text);
    // `process::exit` 不跑析构：先把缓冲刷掉，否则重定向到文件时尾部会丢行。
    let _ = std::io::stdout().flush();
    if !o.ok {
        std::process::exit(2);
    }
    Ok(())
}

fn cmd_inspect(file: &Path) -> Result<()> {
    emit(tools::inspect(file)?)
}

fn cmd_passthrough(input: &Path, output: &Path) -> Result<()> {
    emit(tools::passthrough(input, output)?)
}

fn cmd_diff(a: &Path, b: &Path) -> Result<()> {
    emit(tools::diff(a, b)?)
}

fn cmd_dump(
    file: &Path,
    part: Option<&str>,
    grep: Option<&str>,
    limit: usize,
    special: bool,
) -> Result<()> {
    emit(tools::dump(file, part, grep, limit, special)?)
}

fn cmd_probe(dir: &Path, recursive: bool, out: Option<&Path>) -> Result<()> {
    emit(tools::probe_dir_report(dir, recursive, out)?)
}

fn cmd_verify(a: &Path, b: &Path, level1_only: bool, batch: bool) -> Result<()> {
    emit(tools::verify(a, b, level1_only, batch)?)
}
