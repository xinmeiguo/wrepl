// ★ 发布版走 Windows 图形子系统（PE Subsystem = 2）：双击**不再先弹一个黑色控制台窗口**。
//   不写这一行时，bin 默认是「控制台子系统」（Subsystem = 3），双击会先开一个黑框，
//   里面还可能打印 eframe/建窗路径上的 `thread 'main' has overflowed its stack`
//   —— 那个提示既吓人、又与本工具无关（30 行最小 egui 程序同样会打）。
//
//   为什么限定 release：调试构建保留控制台，方便本地看 stderr 与 panic。
//
//   影响面已经核过，只有「看得见」这一项变了：
//     · 输出重定向到文件/管道照旧 —— 回归脚本 `--selftest > log 2>&1` 不受影响；
//     · 在 cmd / PowerShell 里手动跑，父进程的控制台句柄本来就会继承，输出照旧能看到；
//     · 只有「双击」这一种启动方式会失去可见的 stderr。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! wrepl 图形界面入口。
//!
//! 只做三件事：读命令行 → 装中文字体 → 开窗。界面逻辑全在 [`app`] 里。
//!
//! ```text
//! wrepl-gui                                  正常开窗（空白表单）
//! wrepl-gui --preset <输入> [输出] [规则文件] [--rename] [--in-place]
//!                                            预填参数再开窗（省掉手点选目录）
//!                                            给了输出＝写副本；--in-place＝就地替换
//! wrepl-gui --selftest <输入> <输出> [规则文件]
//!                                            不开窗：加载字体 + 跑 3 帧布局 + 跑一次完整批量
//! ```
//!
//! **开窗默认是「就地替换源文件」**（在原文件上直接覆盖改写，默认**不留** `.bak`，
//! 要备份就勾「保留 .bak 备份」）；
//! 要留副本就在界面上勾「输出到子文件夹」。`--preset` / `--selftest` 给了输出目录时
//! 一律走写副本，这样自动化路径不可能因为默认值的变化去改源样本。

mod app;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // 无窗口自检：不开窗，真跑字体加载 + 布局 + 一次完整批量。用于回归。
    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(app::selftest(&args));
    }

    let preset = app::Preset::from_args(&args);

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("wrepl —— Word 批量替换")
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([940.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "wrepl",
        native,
        Box::new(move |cc| {
            app::install_fonts(&cc.egui_ctx);
            cc.egui_ctx.set_visuals(egui::Visuals::light());
            Ok(Box::new(match &preset {
                Some(p) => app::App::with_preset(p),
                None => app::App::new(),
            }))
        }),
    )
}
