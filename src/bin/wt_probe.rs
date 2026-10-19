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

use com::{DispResult, res_disp, res_i32, var_bstr, var_i4};

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
