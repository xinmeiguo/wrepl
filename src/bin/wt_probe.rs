//! 脱离界面的 Word COM 探针（诊断用，不属于产品功能）。
//!
//! ## 为什么要有它
//!
//! 「页眉替换」页的 COM 路在界面上表现为"点了执行、报成功、文件却没变"。
//! 界面只把日志留在自己的 `self.log` 里，关窗即失，查不到 COM 那层到底报了什么。
//! 这个探针把同一批子模块（`com` / `wd` / `locator` / `services` / `word_session`）
//! 直接搬进一个**没有 egui 依赖**的 bin 里跑，于是：
//!   · 每一步 COM 调用的返回值都能打到 stdout；
//!   · 可以逐步复刻 `locator::for_each_first_cell` 的取法，看它断在哪一层；
//!   · 再真跑一次 `services::*_run`，看整条链的 (ok, fail) 与日志。
//!
//! `#[path]` 直接 include 源码，不复制一份 —— 探针与产品走的是**同一份代码**，
//! 探针里能复现的，界面上必然同样发生。
//!
//! ## 用法
//!
//! ```text
//! wt_probe <text|image> <docx>...            # 就地改（请指向副本！）
//! wt_probe read  <docx>...                   # 只读：逐步打印定位链，不改文件
//! wt_probe pdf  <输出目录|-> <页码范围> <docx>...
//!                                            # 跑一遍导出 PDF（`-` = 各文档旁的 pdf/，
//!                                            #   页码留空引号 `""` = 全部）
//! wt_probe pdfa <输出目录|-> <页码范围> <docx>...   # 同上，但按 PDF/A 归档
//! wt_probe perf <输出根目录> <docx>...       # 三档计时：现状 / 调优 / 调优+PDF/A
//! ```

// 探针把 `wordtool` 的六个子模块**整套** `#[path]` 搬进来（只为让探针与产品
// 走同一份代码），但每次只走其中一两条路径 —— 剩下的函数 / 结构"没被用到"
// 是**设计如此**，不是死代码。产品 bin（`wrepl-gui`）里这些代码都在用，所以
// 不会报；不声明这一行，探针 bin 会独占 8 条 `never used` 警告，
// 把整个构建输出弄脏（`cargo build --features gui` 的警告数从 7 变 15）。
#![allow(dead_code)]

#[path = "../gui/wordtool/com.rs"]
mod com;
#[path = "../gui/wordtool/locator.rs"]
mod locator;
#[path = "../gui/wordtool/printers.rs"]
mod printers;
#[path = "../gui/wordtool/services.rs"]
mod services;
#[path = "../gui/wordtool/wd.rs"]
mod wd;
#[path = "../gui/wordtool/word_session.rs"]
mod word_session;

use com::{DispResult, res_disp, res_i32, var_bool, var_bstr, var_i4};

/// 一次「打开 → 导出 → 关闭」的分段耗时（毫秒）。
struct Row {
    name: String,
    open_ms: u128,
    export_ms: u128,
    close_ms: u128,
}

fn ms(t: std::time::Instant) -> u128 {
    t.elapsed().as_millis()
}

/// 调优档要额外拨的几个开关。**只拨"不影响排版"的那几个**：
/// 拼写 / 语法检查是纯后台开销，与分页、字体、图片尺寸都无关，
/// 所以不可能改变 PDF 的任何一页；`DisplayRecentFiles` 只是不写 MRU。
///
/// 特意**不碰** `Options.Pagination` —— 关掉它 Word 就不再算分页，
/// 页码与页数会直接错，那是"快得离谱但导出是错的"。
unsafe fn tune(word: &com::Disp) -> DispResult<()> {
    unsafe {
        word.put("DisplayRecentFiles", var_bool(false))?;
        let opts = res_disp(word.get("Options")?)?;
        // 逐个 best-effort：老版本 Word 上某些成员可能不存在，不该因此中断测量
        let _ = opts.put("CheckSpellingAsYouType", var_bool(false));
        let _ = opts.put("CheckGrammarAsYouType", var_bool(false));
        let _ = opts.put("CheckGrammarWithSpelling", var_bool(false));
        Ok(())
    }
}

/// 跑一整批，记每个文档的三段耗时。
///
/// `tuned=false` 用与产品**完全一致**的 `Documents.Open(路径)`（位置参数缺省）；
/// `tuned=true` 只读打开、不写 MRU、不弹转换/编码对话框，并先 [`tune`] 一遍。
unsafe fn perf_pass(
    files: &[String],
    dir: &std::path::Path,
    tuned: bool,
    pdf_a: bool,
) -> DispResult<Vec<Row>> {
    unsafe {
        std::fs::create_dir_all(dir).ok();
        let mut session = word_session::WordSession::create()?;
        if tuned {
            tune(&session.word)?;
        }
        let docs = res_disp(session.word.get("Documents")?)?;
        let mut rows = Vec::new();
        for f in files {
            let name = f.rsplit(['\\', '/']).next().unwrap_or(f).to_string();
            let t0 = std::time::Instant::now();
            let opened = if tuned {
                // Open(FileName, ConfirmConversions, ReadOnly, AddToRecentFiles)
                docs.call(
                    "Open",
                    vec![var_bstr(f), var_bool(false), var_bool(true), var_bool(false)],
                )
            } else {
                docs.call("Open", vec![var_bstr(f)])
            };
            let doc = match opened.and_then(|v| res_disp(v)) {
                Ok(d) => d,
                Err(e) => {
                    println!("  !! 打开失败 {name}：{e}");
                    continue;
                }
            };
            let t1 = std::time::Instant::now();

            // 与 services::export_one 同一组参数（探针只为计时，不复用那个私有函数）
            let stem = std::path::Path::new(f)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "document".to_string());
            let out = dir.join(format!("{stem}.pdf"));
            let exported = doc.call_named(
                "ExportAsFixedFormat",
                vec![
                    ("OutputFileName", var_bstr(&out.to_string_lossy())),
                    ("ExportFormat", var_i4(wd::EXPORT_FORMAT_PDF)),
                    ("OpenAfterExport", var_bool(false)),
                    ("OptimizeFor", var_i4(wd::EXPORT_OPTIMIZE_FOR_PRINT)),
                    ("UseISO19005_1", var_bool(pdf_a)),
                ],
            );
            let t2 = std::time::Instant::now();
            if let Err(e) = exported {
                println!("  !! 导出失败 {name}：{e}");
            }
            let _ = doc.callv("Close", vec![var_bool(false)]);
            let t3 = std::time::Instant::now();

            rows.push(Row {
                name,
                open_ms: (t1 - t0).as_millis(),
                export_ms: (t2 - t1).as_millis(),
                close_ms: (t3 - t2).as_millis(),
            });
        }
        session.cleanup();
        Ok(rows)
    }
}


/// COM 的布尔（VT_BOOL）读成 i32 后是 0 / -1。
fn yn(v: i32) -> &'static str {
    if v == 0 { "否" } else { "是" }
}

/// 逐步复刻 `locator::for_each_first_cell` 的取法，每一步都打印真实结果。
///
/// 只读：打开后不保存、直接关。
fn probe_read(path: &str) -> DispResult<()> {
    unsafe {
        println!("---- 逐步探针：{path}");
        let mut session = word_session::WordSession::create()?;
        println!("[1] WordSession::create            OK");

        let docs = res_disp(session.word.get("Documents")?)?;
        println!("[2] word.Documents                 OK");

        let doc = res_disp(docs.call("Open", vec![var_bstr(path)])?)?;
        println!("[3] Documents.Open                 OK");

        let sections = res_disp(doc.get("Sections")?)?;
        let n = res_i32(sections.get("Count")?)?;
        println!("[4] doc.Sections / Count           OK  Count={n}");

        // 两个开关决定"页面实际显示哪个页眉"。
        // 都关着 = 首页、奇数页、偶数页统统显示"主页眉"。
        // 这和"页眉部件里有没有东西"是两回事，混起来就会得出
        // "COM 只改主页眉 ⇒ 首页不变"这种听起来对、实际误导的结论。
        {
            let ps = res_disp(doc.get("PageSetup")?)?;
            let dfp = res_i32(ps.get("DifferentFirstPageHeaderFooter")?)?;
            let oae = res_i32(ps.get("OddAndEvenPagesHeaderFooter")?)?;
            println!(
                "[5] PageSetup: 首页不同={} 奇偶页不同={}   =>{}",
                yn(dfp),
                yn(oae),
                if dfp == 0 && oae == 0 {
                    "所有页面都显示【主页眉】"
                } else {
                    "首页/偶数页会各自取自己的页眉"
                }
            );
        }

        for i in 1..=n {
            let section = res_disp(sections.call("Item", vec![var_i4(i)])?)?;
            let headers = res_disp(section.get("Headers")?)?;
            println!("  -- 第 {i} 节 --");

            for k in 1..=3 {
                let header = res_disp(headers.call("Item", vec![var_i4(k)])?)?;
                let range = res_disp(header.get("Range")?)?;
                let tables = res_disp(range.get("Tables")?)?;
                let cnt = res_i32(tables.get("Count")?)?;
                let htext = com::res_string(range.get("Text")?)?;
                let one = htext.split_whitespace().collect::<Vec<_>>().join(" ");
                println!("     Headers({k}) 表格={cnt}  文本={one:?}");
                if cnt == 0 {
                    continue;
                }
                let tbl = res_disp(tables.call("Item", vec![var_i4(1)])?)?;
                let rows = res_disp(tbl.get("Rows")?)?;
                let cols = res_disp(tbl.get("Columns")?)?;
                let nr = res_i32(rows.get("Count")?)?;
                let nc = res_i32(cols.get("Count")?)?;
                println!("     Tables(1).Rows.Count={nr} Columns.Count={nc}");
                if nr < 1 || nc < 1 {
                    println!("     !! 行或列为空，locator 会 continue");
                    continue;
                }
                let cell = res_disp(tbl.call("Cell", vec![var_i4(1), var_i4(1)])?)?;
                let crange = res_disp(cell.get("Range")?)?;
                let ctext = com::res_string(crange.get("Text")?)?;
                let one = ctext.split_whitespace().collect::<Vec<_>>().join(" ");
                println!("     Cell(1,1) = {one:?}");
            }
        }

        let _ = doc.callv("Close", vec![com::var_bool(false)]);
        session.cleanup();
        drop(session);
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("用法：wt_probe <read|text|image|pdf|pdfa> <docx>...");
        std::process::exit(2);
    }
    let mode = args[0].clone();
    let docs: Vec<String> = args[1..].to_vec();

    // 探针自己也走界面那条线：独立线程 + STA 初始化。
    // 特意保持与 `ui.rs::start` 相同的顺序（先 CoInitializeEx，再干活）。
    let handle = std::thread::spawn(move || {
        let init = match com::ComInit::sta() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("!! ComInit::sta() 失败：{e}");
                return 1;
            }
        };
        println!("[0] ComInit::sta()                 OK");

        if mode == "read" {
            for d in &docs {
                if let Err(e) = probe_read(d) {
                    eprintln!("!! 探针出错 {d}：{e}");
                }
            }
            return 0;
        }

        // ---------- 导出 PDF（2026-10-19 新增）----------
        //
        // 为什么探针要有这一档：「导出 PDF」整条链只有 Word COM 一条路，
        // 而界面上点一下要走确认框、后台线程、channel 回传 —— 出问题时分不清是
        // COM 参数不对、还是界面那条线断了。这里直接调同一个
        // `services::export_pdf_run`，**走的是同一份源码**（`#[path]` 引入），
        // 所以探针能跑通，界面上就一定跑得通；探针报什么错，界面里也是那个错。
        if mode == "pdf" || mode == "pdfa" {
            let out = docs.first().cloned().unwrap_or_default();
            let range = docs.get(1).cloned().unwrap_or_default();
            let files: Vec<String> = docs.iter().skip(2).cloned().collect();
            if files.is_empty() {
                eprintln!("用法：wt_probe pdf <输出目录|-> <页码范围> <docx>...");
                return 2;
            }
            let dir = if out.is_empty() || out == "-" {
                None
            } else {
                Some(std::path::PathBuf::from(&out))
            };
            let pages = match services::parse_pdf_pages(&range) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("!! 页码范围读不出：{e}");
                    return 2;
                }
            };
            println!(
                "== 导出 {}｜{} 个文档｜{}｜{}",
                if mode == "pdfa" { "PDF/A" } else { "PDF" },
                files.len(),
                pages.describe(),
                match &dir {
                    Some(d) => d.display().to_string(),
                    None => "每份文档旁的 pdf/".to_string(),
                }
            );
            let mut log = |s: String| println!("  {s}");
            let mut prog = |cur: usize, total: usize| println!("[进度 {cur}/{total}]");
            let (ok, fail) = services::export_pdf_run(
                &files,
                dir.as_deref(),
                pages,
                mode == "pdfa",
                &mut log,
                &mut prog,
            );
            println!("== 结果：成功 {ok} / 失败 {fail}");
            drop(init);
            return if fail == 0 && ok > 0 { 0 } else { 1 };
        }

        // ---------- 性能探针（2026-10-19 新增）----------
        //
        // 回答两个问题：① 换成只读打开 / 关拼写语法，到底能省多少？
        // ② PDF/A 比普通 PDF 贵多少？三档跑同一批文档，各自重开 Word（含冷启动）。
        if mode == "perf" {
            let root = docs.first().cloned().unwrap_or_default();
            let files: Vec<String> = docs.iter().skip(1).cloned().collect();
            if files.is_empty() {
                eprintln!("用法：wt_probe perf <输出根目录> <docx>...");
                return 2;
            }
            let root = std::path::PathBuf::from(root);
            let passes: [(&str, &str, bool, bool); 3] = [
                ("A 现状（Open(路径) 全部缺省）", "a", false, false),
                ("B 调优（只读 + 不记 MRU + 关拼写语法）", "b", true, false),
                ("C 调优 + PDF/A", "c", true, true),
            ];
            let mut totals: Vec<(String, u128, u128)> = Vec::new();
            for (label, sub, tuned, pdf_a) in passes {
                println!("\n════ {label} ════");
                let t_all = std::time::Instant::now();
                let rows = match unsafe { perf_pass(&files, &root.join(sub), tuned, pdf_a) } {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("!! {label} 跑不动：{e}");
                        continue;
                    }
                };
                let mut open_sum = 0u128;
                let mut export_sum = 0u128;
                for r in &rows {
                    println!(
                        "  {:<44} 打开 {:>6} ms   导出 {:>7} ms   关闭 {:>4} ms",
                        r.name, r.open_ms, r.export_ms, r.close_ms
                    );
                    open_sum += r.open_ms;
                    export_sum += r.export_ms;
                }
                println!(
                    "  ── 合计 {} 份：打开 {} ms｜导出 {} ms｜整档墙钟 {} ms",
                    rows.len(),
                    open_sum,
                    export_sum,
                    ms(t_all)
                );
                totals.push((label.to_string(), open_sum, export_sum));
            }
            if totals.len() >= 2 {
                let (a_open, a_exp) = (totals[0].1, totals[0].2);
                let (b_open, b_exp) = (totals[1].1, totals[1].2);
                let pct = |a: u128, b: u128| {
                    if a == 0 {
                        "-".to_string()
                    } else {
                        format!("{:+.0}%", (b as f64 - a as f64) * 100.0 / a as f64)
                    }
                };
                println!(
                    "\n== A → B：打开 {} → {} ms（{}）｜导出 {} → {} ms（{}）",
                    a_open,
                    b_open,
                    pct(a_open, b_open),
                    a_exp,
                    b_exp,
                    pct(a_exp, b_exp)
                );
                if totals.len() >= 3 {
                    let c_exp = totals[2].2;
                    println!(
                        "== PDF/A 的代价（对比 B）：导出 {} → {} ms（{}）",
                        b_exp,
                        c_exp,
                        pct(b_exp, c_exp)
                    );
                }
            }
            drop(init);
            return 0;
        }

        let mut log = |s: String| println!("  {s}");
        let mut prog = |cur: usize, total: usize| println!("[进度 {cur}/{total}]");
        let (ok, fail) = if mode == "image" {
            let img = docs.last().cloned().unwrap_or_default();
            let list: Vec<String> = docs[..docs.len() - 1].to_vec();
            services::image_replace_run(&list, &img, true, &mut log, &mut prog)
        } else {
            services::text_replace_run(&docs, "PROBE EN", "探针中文", &mut log, &mut prog)
        };
        println!("== 结果：成功 {ok} / 失败 {fail}");
        drop(init);
        0
    });

    let code = handle.join().unwrap_or(3);
    std::process::exit(code);
}
