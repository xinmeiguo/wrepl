//! 把 Windows 产物的**主线程栈预留钉死在 1 MB**（`SizeOfStackReserve`）。
//!
//! 为什么是"钉死"，而不是"调大" —— 这是 2026-10-09 用单变量实验测出来的一条硬规律。
//!
//! 起因：本机自编（GNU/mingw）的界面版双击后要 **5.5 s** 才出窗口，而 CI 出的
//! MSVC 产物只要 **2.1 s**，用户直接感觉"程序打开变慢"。逐项排除后剩下的变量是
//! PE 头里的栈预留：**mingw 默认 2 MB、MSVC 默认 1 MB**，别的都一样。
//!
//! 单变量实验（同一份二进制，只改 PE 头里那 1~2 个字节的栈字段，其余一个字节不碰，
//! 两个工具链两个方向都做）：
//!
//! | 栈预留 | 进程起 → 主窗口出现 |
//! |---|---|
//! | 1.00 MB | **2.2 s** |
//! | 1.25 MB | 3.1 s |
//! | 1.50 MB | 3.5 s |
//! | 2.00 MB | 5.5 s ← mingw 默认 |
//! | 3.00 MB | 10.9 s |
//! | 4.00 MB | 19.9 s |
//! | 8.00 MB | **窗口永远建不出来**（进程退出码 1） |
//! | 16 MB | **同上**（`NoGlutinConfigs(… kind: NotFound)`，干等约 38 s） |
//!
//! 把 MSVC 产物（1 MB，2.1 s）的栈改成 2 MB，它同样掉到 5.5 s；
//! 把 GNU 产物（2 MB，5.5 s）改回 1 MB，它同样回到 2.2 s。
//! ⇒ 与工具链、与 LTO、与本项目代码**都无关**，就是这一个字段。
//!
//! 把启动耗时切成两段后看得更清楚：进程起 → `eframe::run_native` **只要 0.05~0.10 s**，
//! 慢的全部在建窗（glutin 枚举 GL 配置）那一段。栈预留是"预留＝只占地址空间、
//! 用多少提交多少"（commit 始终 4 KB），可代价仍然随预留区大小单调上升 ——
//! 说明本机有某层东西（显卡驱动 / 这台机器上的安全管控）在按**预留区大小**付出代价。
//! 具体机制没有查明（要 ETW/剖析器），但规律已经被实验钉死，且工具链无关。
//!
//! 所以：**默认值不可依赖**。MSVC 默认恰好是 1 MB，mingw 默认是 2 MB —— 只靠默认，
//! 两个工具链就会给出一个快、一个慢的两份"同一版本"产物。这里显式写死 1 MB，
//! 并在 CI 里断言成品就是这个值（`.github/scripts/check_pe_imports.py`）。
//!
//! 另注：**不要把栈调大**。调大不是"更保险"，而是直接打坏窗口（≥8 MB 永远建不出来）。
//! 早先 v0.2.3 开发期曾把栈提到 16 MB，结果"双击没反应"，正是这条规律的另一端。
//!
//! 另外还做一件事：把**熊猫图标与版本信息**编进 Windows 产物的资源节
//! （`embed_resources`）。资源只挂在 Windows 目标上，别的平台一行都不多加。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    pin_stack();

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resources();
    }
}

/// 主线程栈预留钉死在 1 MB —— 理由见文件头那八行实验表。
fn pin_stack() {
    // 目标环境由 cargo 直接给出：msvc / gnu（本机只可能这两个）。
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let arg = if env == "msvc" {
        // link.exe 的写法
        "/STACK:1048576".to_string()
    } else {
        // GNU 工具链走 gcc 驱动，参数透传给 ld
        "-Wl,--stack,1048576".to_string()
    };
    println!("cargo:rustc-link-arg-bins={arg}");
}

/// 图标 + VERSIONINFO。
///
/// 为什么要自己写而不是拉 `embed-resource` / `winres`：本项目是**离线构建**
/// （`--offline`，依赖只有 vendor 里那些），为 30 行逻辑新增一个 build 依赖
/// 不划算。两条工具链各走各的原生工具：
///
/// | 目标 | 编译器 | 产物 |
/// |---|---|---|
/// | GNU（本机） | `windres`（mingw binutils，PATH 里有；可用 `WINDRES` 覆盖） | COFF 对象 |
/// | MSVC（CI） | `rc.exe`（跟着 Windows SDK 装） | `.res` |
///
/// VERSIONINFO 的版本号从 `CARGO_PKG_VERSION` **现取现拼**，不写在 .rc 文件里
/// —— 否则每发一版都要多改一处版本号，迟早会漏（`CHANGELOG.md` 那边的
/// 「四处版本号」清单已经够长了）。
///
/// .rc 里的字符串**刻意全用 ASCII**：`windres` 和 `rc.exe` 对 UTF-8 的处理
/// 不一致（前者要 `--codepage=65001`、后者要 `#pragma code_page`），中文
/// 一旦有一边解码歪了，就是一堆乱码出现在文件属性里 —— 而这块地方没人测得到。
fn embed_resources() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico: PathBuf = manifest.join("assets").join("wrepl.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    // 缺图标就直接失败：静默跳过等于「这版发布包悄悄没图标」，而图标正是
    // 这一版发版的目的 —— 宁可 CI 红，也不要发出去一个没图的 exe。
    assert!(
        ico.is_file(),
        "缺少图标 {}：用 `python assets/make_icon.py` 重新生成",
        ico.display()
    );

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // 把图标复制进 OUT_DIR，.rc 里就只写**裸文件名**、编译器的工作目录也设成那里。
    // 这不是多此一举：`rc.exe` 与 `windres` 对「rc 里写绝对路径」的接受程度不一样
    // （一个正斜杠能过、一个不保证），而带空格的 `C:\Program Files (x86)` 更是
    // 只在一条工具链上验证过。改成同目录裸名，两条路走的是同一份写法。
    let ico_local = out.join("wrepl.ico");
    std::fs::copy(&ico, &ico_local).expect("复制图标到 OUT_DIR 失败");

    let rc_path = out.join("wrepl.rc");
    std::fs::write(&rc_path, rc_text()).expect("写 .rc 失败");

    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env == "msvc" {
        let rc = find_rc_exe().expect(
            "找不到 Windows 资源编译器 rc.exe。它在 Windows SDK 里：\
             装了 Visual Studio 就有；也可用环境变量 RC 指定完整路径",
        );
        let res = out.join("wrepl.res");
        let ok = Command::new(&rc)
            .current_dir(&out)
            .arg("/nologo")
            .arg(format!("/fo{}", res.display()))
            .arg(&rc_path)
            .status()
            .unwrap_or_else(|e| panic!("无法执行 {}: {e}", rc.display()));
        assert!(ok.success(), "rc.exe 编译资源失败");
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        let windres = std::env::var("WINDRES").unwrap_or_else(|_| "windres".to_string());
        let obj = out.join("wrepl_res.o");
        let ok = Command::new(&windres)
            .current_dir(&out)
            .arg("--input-format=rc")
            .arg("--output-format=coff")
            .arg("-i")
            .arg(&rc_path)
            .arg("-o")
            .arg(&obj)
            .status()
            .unwrap_or_else(|e| {
                panic!(
                    "无法执行 {windres}（GNU 工具链需要 mingw 的 binutils 在 PATH 里，\
                     或用环境变量 WINDRES 指定完整路径）: {e}"
                )
            });
        assert!(ok.success(), "windres 编译资源失败");
        println!("cargo:rustc-link-arg-bins={}", obj.display());
    }
}

/// 拼出 .rc 的内容。版本号四段（`0.8.3` → `0,8,3,0`），字符串版本照抄。
///
/// 图标写裸文件名：编译器的工作目录就是 OUT_DIR，`wrepl.ico` 已复制在那儿。
fn rc_text() -> String {
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut parts = ver.split(['.', '-', '+']);
    // 版本号凑成四段：`0.8.3` → `0,8,3,0`（VERSIONINFO 的 FILEVERSION 必须四段）
    let n: Vec<u32> = (0..4)
        .map(|_| parts.next().and_then(|s| s.parse().ok()).unwrap_or(0))
        .collect();
    let quad = format!("{},{},{},{}", n[0], n[1], n[2], n[3]);

    format!(
        "// 由 build.rs 生成，不要手改。\r\n\
         \r\n\
         // 资源 ID 用 1：Windows 取最小 ID 的图标当 exe 的默认图标。\r\n\
         1 ICON \"wrepl.ico\"\r\n\
         \r\n\
         1 VERSIONINFO\r\n\
         FILEVERSION {quad}\r\n\
         PRODUCTVERSION {quad}\r\n\
         FILEFLAGSMASK 0x3fL\r\n\
         FILEFLAGS 0x0L\r\n\
         FILEOS 0x40004L\r\n\
         FILETYPE 0x1L\r\n\
         FILESUBTYPE 0x0L\r\n\
         BEGIN\r\n\
         \x20   BLOCK \"StringFileInfo\"\r\n\
         \x20   BEGIN\r\n\
         \x20       BLOCK \"040904b0\"\r\n\
         \x20       BEGIN\r\n\
         \x20           VALUE \"CompanyName\", \"XINWEI\"\r\n\
         \x20           VALUE \"FileDescription\", \"wrepl - surgical .docx find & replace\"\r\n\
         \x20           VALUE \"FileVersion\", \"{ver}\"\r\n\
         \x20           VALUE \"InternalName\", \"wrepl\"\r\n\
         \x20           VALUE \"LegalCopyright\", \"Copyright (C) XINWEI. All rights reserved.\"\r\n\
         \x20           VALUE \"ProductName\", \"wrepl\"\r\n\
         \x20           VALUE \"ProductVersion\", \"{ver}\"\r\n\
         \x20       END\r\n\
         \x20   END\r\n\
         \x20   BLOCK \"VarFileInfo\"\r\n\
         \x20   BEGIN\r\n\
         \x20       VALUE \"Translation\", 0x409, 1200\r\n\
         \x20   END\r\n\
         END\r\n"
    )
}

/// 找 `rc.exe`：先看 `RC` 环境变量，再看 PATH，最后扫 Windows SDK / VS 的安装目录。
///
/// CI 上 rc.exe 通常**不在 PATH 里**（要先进 vcvars 才有），所以不能只试 PATH。
/// 扫目录时按版本号从大到小试，取最新的那套 SDK。
fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // PATH 里有就直接用（本机装过 VS 的机器常见）
    if let Ok(out) = Command::new("rc").arg("/?").output() {
        // rc.exe 对 /? 返回非 0，但能跑起来就说明存在
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        if text.contains("resource compiler") || out.status.code().is_some() {
            return Some(PathBuf::from("rc"));
        }
    }

    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();
    // ① Windows SDK：C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\rc.exe
    let kits = std::env::var("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Program Files (x86)"));
    let bin = kits.join("Windows Kits").join("10").join("bin");
    if let Ok(rd) = std::fs::read_dir(&bin) {
        for e in rd.flatten() {
            let ver = e.file_name().to_string_lossy().to_string();
            let exe = e.path().join("x64").join("rc.exe");
            if exe.is_file() {
                candidates.push((ver_key(&ver), exe));
            }
        }
    }
    // ② Visual Studio 自带的：...\VC\Tools\MSVC\<ver>\bin\Hostx64\x64\rc.exe
    for pf in ["ProgramFiles", "ProgramFiles(x86)"] {
        let root = std::env::var(pf)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(format!(r"C:\{pf}")));
        let vs = root.join("Microsoft Visual Studio");
        if let Ok(years) = std::fs::read_dir(&vs) {
            for y in years.flatten() {
                if let Ok(editions) = std::fs::read_dir(y.path()) {
                    for ed in editions.flatten() {
                        let tools = ed.path().join("VC").join("Tools").join("MSVC");
                        if let Ok(vers) = std::fs::read_dir(&tools) {
                            for v in vers.flatten() {
                                let exe = v.path().join("bin").join("Hostx64").join("x64").join("rc.exe");
                                if exe.is_file() {
                                    let key = v.file_name().to_string_lossy().to_string();
                                    candidates.push((ver_key(&key), exe));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, p)| p)
}

/// 把 `10.0.22621.0` / `14.38.33130` 这类版本串折成可比较的整数。
fn ver_key(s: &str) -> u64 {
    let mut k = 0u64;
    for part in s.split('.').take(4) {
        let n: u64 = part.parse().unwrap_or(0);
        k = k * 1000 + n.min(999);
    }
    k
}
