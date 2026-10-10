//! 一次性性能剖析工具：把 `pipeline::process_one` 的一条路径拆成若干段分别计时。
//!
//! ```text
//! cargo run --release --example profile -- <某个 docx> [规则文件]
//! ```
//!
//! 纯本地诊断用，不参与发布产物。

use std::io::{Seek, Write};
use std::path::PathBuf;
use std::time::Instant;

use wrepl::docx::package;
use wrepl::engine;
use wrepl::rules::{self, Rule};
use wrepl::verify;

fn once<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    println!("  {label:<46} {:>8.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    r
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let src = PathBuf::from(&args[1]);

    let rl: Vec<Rule> = match args.get(2) {
        Some(p) => {
            let d = Rule::new(0, "", "");
            rules::from_text_file(std::path::Path::new(p), &d, 1)?
        }
        None => vec![],
    };

    println!(
        "文件：{}  ({:.1} KB)   规则 {} 条",
        src.display(),
        std::fs::metadata(&src)?.len() as f64 / 1024.0,
        rl.len()
    );

    let parts = once("package::inspect（全包解压+SHA256）", || {
        package::inspect(&src).unwrap()
    });
    println!(
        "  共 {} 个 part，解压后合计 {:.1} KB",
        parts.len(),
        parts.iter().map(|p| p.uncompressed_size).sum::<u64>() as f64 / 1024.0
    );
    for p in parts.iter().take(8) {
        println!(
            "    {:<42} {:>9} B 压缩 → {:>9} B 解压  {}",
            p.name, p.compressed_size, p.uncompressed_size, p.method
        );
    }

    let tp = once("package::read_text_parts", || {
        package::read_text_parts(&src).unwrap()
    });
    println!(
        "  文本 part {} 个，合计 {:.1} KB",
        tp.len(),
        tp.iter().map(|(_, _, b)| b.len()).sum::<usize>() as f64 / 1024.0
    );

    let mut total_plan = 0.0f64;
    let mut total_scan = 0.0f64;
    let mut n_hits = 0usize;
    let mut replaced = std::collections::HashMap::new();
    for (name, kind, bytes) in &tp {
        let t = Instant::now();
        let text = std::str::from_utf8(bytes).unwrap();
        let (paras, stats) = wrepl::docx::scan::scan_part(name, text).unwrap();
        let scan_ms = t.elapsed().as_secs_f64() * 1000.0;

        let t = Instant::now();
        let plan = engine::plan_part(name, *kind, bytes, &rl, false).unwrap();
        let plan_ms = t.elapsed().as_secs_f64() * 1000.0;

        total_scan += scan_ms;
        total_plan += plan_ms;
        n_hits += plan.hits.len();
        println!(
            "  {name:<30} scan {scan_ms:>7.1} ms  plan {plan_ms:>7.1} ms  \
             段 {} 可见字符 {} 命中 {}",
            stats.paragraphs,
            stats.visible_chars,
            plan.hits.len()
        );
        if let Some(b) = plan.new_bytes {
            replaced.insert(name.clone(), b);
        }
        let _ = paras;
    }
    println!("  {:<46} {:>8.1} ms", "└ scan_part 合计", total_scan);
    println!("  {:<46} {:>8.1} ms", "└ plan_part 合计", total_plan);
    println!("  命中总数 {n_hits}，改动 part {} 个", replaced.len());

    once("pipeline::file_sha(src)（全文件读+SHA256）", || {
        wrepl::pipeline::file_sha(&src).unwrap()
    });
    {
        use sha2::{Digest, Sha256};
        let buf = once("  └ std::fs::read(src)（纯读盘）", || std::fs::read(&src).unwrap());
        println!("    读到 {} 字节", buf.len());
        for i in 0..3 {
            let t = Instant::now();
            let d = Sha256::digest(&buf);
            println!(
                "  {:<46} {:>8.1} ms   ({:.0} MB/s)",
                format!("  └ Sha256::digest(cached) #{i}"),
                t.elapsed().as_secs_f64() * 1000.0,
                buf.len() as f64 / 1048576.0 / t.elapsed().as_secs_f64()
            );
            let _ = d;
        }
    }

    let snap = once("verify::snapshot(src)（全包解压+SHA256+骨架）", || {
        verify::snapshot(&src).unwrap()
    });
    println!(
        "  {} 个 part，{} 个文本 part 有骨架",
        snap.parts.len(),
        snap.skel.len()
    );

    let scratch = std::env::temp_dir().join("wrepl-profile");
    std::fs::create_dir_all(&scratch)?;
    for round in 0..3 {
        let dst = scratch.join(format!("out{round}.docx"));
        once(
            &format!("package::write_with_replacements #{round}"),
            || package::write_with_replacements(&src, &dst, &replaced).unwrap(),
        );
        once(&format!("  └ file_sha(dst) #{round}"), || {
            wrepl::pipeline::file_sha(&dst).unwrap()
        });
        once(&format!("  └ verify_one(src,dst) #{round}"), || {
            verify::verify_one(&src, &dst)
        });
        once(&format!("  └ level1(src,dst) #{round}"), || {
            verify::level1(&src, &dst).unwrap()
        });
        once(&format!("  └ residue(dst) #{round}"), || {
            verify::residue(&dst, &rl).unwrap()
        });
    }
    once("verify::snapshot(src) 再来一次（缓存后）", || {
        verify::snapshot(&src).unwrap()
    });

    let path = scratch.join("inplace.docx");
    std::fs::copy(&src, &path)?;
    once("package::rewrite_in_place（内存拼包+覆盖）", || {
        package::rewrite_in_place(&path, &replaced).unwrap()
    });

    // ── 文件系统原语单价 ──
    // 本机装了企业管控 / 实时防护，**每次 open 都是要记账的**。
    // 这几个数字决定了"少开一次文件"值多少钱。
    {
        let n = 200;
        let t = Instant::now();
        for _ in 0..n {
            let _f = std::fs::File::open(&src).unwrap();
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "File::open（已存在文件）",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );

        let t = Instant::now();
        for i in 0..n {
            let p = scratch.join(format!("create-{i}.tmp"));
            let _f = std::fs::File::create(&p).unwrap();
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "File::create（新建文件）",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );

        let t = Instant::now();
        for _ in 0..n {
            let _ = std::fs::metadata(&src).unwrap();
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "fs::metadata",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );

        let t = Instant::now();
        for _ in 0..n {
            let _ = src.canonicalize().unwrap();
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "Path::canonicalize",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );

        let t = Instant::now();
        for i in 0..n {
            let p = scratch.join(format!("create-{i}.tmp"));
            let _ = std::fs::remove_file(&p);
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "fs::remove_file",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );

        // 打开**已存在**文件写（复制一份到 scratch，别去动语料）
        let seed = scratch.join("seed.tmp");
        std::fs::write(&seed, b"hello").unwrap();
        let t = Instant::now();
        for _ in 0..n {
            let _f = std::fs::OpenOptions::new().write(true).open(&seed).unwrap();
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "OpenOptions::write(true).open（已存在）",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );

        // 打开**刚创建**的文件写 —— `write_at` 走的正是这一步
        let t = Instant::now();
        for i in 0..n {
            let p = scratch.join(format!("rw-{i}.tmp"));
            std::fs::write(&p, b"hello").unwrap();
            let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.write_all(b"hi").unwrap();
            let _ = std::fs::remove_file(&p);
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "新建 + 再 open(write) + 写 + 删（一轮）",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );

        // 同一个句柄：create(read+write) → 写 → seek0 → 再写
        let t = Instant::now();
        for i in 0..n {
            let p = scratch.join(format!("rw2-{i}.tmp"));
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&p)
                .unwrap();
            f.write_all(b"hello").unwrap();
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            f.write_all(b"hi").unwrap();
            let _ = std::fs::remove_file(&p);
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "同一个句柄（create rw + 写 + 回写）一轮",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );

        // 读完再打开写（`rewrite_in_place` 的老做法）
        let t = Instant::now();
        for i in 0..n {
            let p = scratch.join(format!("rw3-{i}.tmp"));
            std::fs::write(&p, b"hello").unwrap();
            let _ = std::fs::read(&p).unwrap();
            let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.write_all(b"hi").unwrap();
            let _ = std::fs::remove_file(&p);
        }
        println!(
            "  {:<46} {:>8.2} ms/次",
            "新建 + read + 再 open(write) + 写 + 删（一轮）",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }

    Ok(())
}
