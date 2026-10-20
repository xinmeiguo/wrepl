// ★ 走 Windows 图形子系统（PE Subsystem = 2）：双击**不弹黑色控制台窗口**。
//   不写这一行时，bin 默认是「控制台子系统」（Subsystem = 3），双击会先开一个黑框，
//   里面还可能打印 eframe/建窗路径上的 `thread 'main' has overflowed its stack`
//   —— 那个提示既吓人、又与本工具无关（30 行最小 egui 程序同样会打）。
//
//   **debug 与 release 一视同仁**（原先只在 release 生效、debug 特意保留控制台看
//   stderr）。两种构建双击起来长得一样，代价与补偿都已核过：
//     · 输出重定向到文件/管道照旧 —— 回归脚本 `--selftest > log 2>&1` 不受影响；
//     · 在 cmd / PowerShell 里手动跑，父进程的控制台句柄本来就会继承，输出照旧能看到；
//     · 只有「双击」这一种启动方式会失去可见的 stderr —— 而查问题本来就该看
//       `%LOCALAPPDATA%\wrepl\wrepl-gui.log`（`diag::init` 第一件事就是装它）。
#![windows_subsystem = "windows"]

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
mod diag;
mod ds;
mod picker;
mod state;
mod wordtool;

use eframe::egui;

fn main() -> eframe::Result<()> {
    // 第一件事就是装日志与 panic 钩子：后面任何一步出问题都要有记录。
    // （图形子系统没有控制台，否则用户只能看到窗口凭空消失，见 `diag`。）
    diag::init("wrepl-gui");

    let args: Vec<String> = std::env::args().collect();

    // 无窗口自检：不开窗，真跑字体加载 + 布局 + 一次完整批量。用于回归。
    if args.iter().any(|a| a == "--selftest") {
        let code = app::selftest(&args);
        diag::log(format!("自检结束，退出码 {code}"));
        std::process::exit(code);
    }

    let preset = app::Preset::from_args(&args);

    // 尺寸**回到 0.2.2 的原值**：1360×920 那次放大是跟着字号一起做的，
    // 实测偏大（1920×1080 上占掉大半屏），字号保持放大就够看了。
    //
    // 下限 940×640 按 **1366×768 的笔记本**定：640 加标题栏(~31) 与任务栏(~40)
    // 约 711 < 768，在那种机器上不会被屏幕裁掉；再往上取就会。
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("wrepl —— Word 批量替换")
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([940.0, 640.0]),
        ..Default::default()
    };

    diag::log("开始建窗（eframe::run_native）");
    // ★ 窗口建不出来时**必须出声**。
    //
    //   图形子系统没有 stderr：`eframe::run_native` 返回 Err（显卡驱动只给
    //   OpenGL 1.1、远程桌面、虚拟机没有 3D 加速时就是这样）、或者 `main`
    //   返回 Err 之后，进程会**安安静静地退出** —— 用户看到的就是"双击没反应"。
    //   这个函数把 Err 接住，写日志、给命令行留一行 stderr、再弹一个说人话的对话框。
    let r = run_and_report(preset, native);
    // 走到这里说明主循环退出了 —— 这一条能把「正常关窗」和「被外部杀掉」
    // 区分开：日志里没有它就说明进程是被硬干掉的，不是自己退的。
    diag::log(format!(
        "主循环结束（run_native 是否正常返回 = {}）—— 进程即将退出",
        r.is_ok()
    ));
    r
}

/// 跑窗口，并且**保证任何失败都看得见**。
///
/// 返回 `eframe::Result<()>` 让 `main` 保持原来的退出语义（失败 = 非零退出码）。
fn run_and_report(
    preset: Option<app::Preset>,
    native: eframe::NativeOptions,
) -> eframe::Result<()> {
    let ran = eframe::run_native(
        "wrepl",
        native,
        Box::new(move |cc| {
            // 设计系统：字体表（egui 默认 + Phosphor + 中文）+ 九档字号层级 + 主题。
            // 顺序钉死在 `ds::install` 里（install_fonts → 自拼字体表 → set_fonts），
            // 不要再往这里插 `set_visuals` —— 那会把设计系统的主题整个盖掉。
            ds::install(&cc.egui_ctx);
            ds::apply(&cc.egui_ctx, true);
            Ok(Box::new(match &preset {
                Some(p) => app::App::with_preset(p),
                // ★ 只有正常开窗才读写 `state.json`。
                //   `--preset` / `--selftest` 是脚本化的一次性跑动，若也去读，
                //   存档里的规则表会跟命令行给的规则文件打架；若也去写，
                //   一次自动化跑动就会把用户手填的规则冲掉。
                None => app::App::with_saved_state(),
            }))
        }),
    );

    let Err(e) = &ran else { return ran };

    // 先试着认出**已经能确证**的原因。认出来了，弹窗就不必再让人从三条
    // "可能"里猜（对方机器上那位同事就是被并列的三条卡住，点了五次确定）。
    let 已知 = 是缺_opengl(e).then_some(diag::已知原因::缺OpenGL);

    // 三路同时告知，哪条路通就走哪条：
    //   ① 日志（唯一一份完整的现场）
    //   ② stderr（从 cmd / PowerShell 启动时父控制台会继承，能直接看到）
    //   ③ 对话框（双击启动时**唯一**看得见的那条）
    let detail = format!("{e}");
    diag::log(format!("!!! 建窗失败 !!!\n  {detail}\n  {e:?}"));
    if 已知.is_some() {
        // 替读日志的人把结论写出来 —— 不写的话，得先知道 egui_glow 内部长什么样
        // 才读得懂上面那句报错。顺带记下"该装驱动"，省一轮来回问。
        diag::log(
            "  ↑ 结论：这台机器的显卡驱动没有提供 OpenGL 2.0 以上（拿到的是 1.1）。\
             请装显卡驱动；没驱动可装就用命令行版 wrepl.exe。",
        );
    }
    eprintln!("wrepl 图形界面没能打开窗口：{detail}");
    eprintln!("（同一个包里的 wrepl.exe 命令行版做的是同一件事，且不需要显卡）");
    diag::startup_failure_box(&detail, 已知);
    ran
}

/// 这次建窗失败，是不是「驱动没提供 OpenGL 2.0+」。
///
/// 判据在 `egui_glow/src/painter.rs:161`：
///
/// ```ignore
/// if gl.version().major < 2 {
///     // this checks on desktop that we are not using opengl 1.1 microsoft sw rendering context.
///     return Err(PainterError("egui_glow requires opengl 2.0+. ".to_owned()));
/// }
/// ```
///
/// 上游把注释直接写成「opengl 1.1 microsoft sw rendering context」—— 正是
/// 「机器里插着显卡，但驱动没装 / 没生效，系统拿『Microsoft 基本显示适配器』
/// 兜底」那一种。顺带：这道检查同时是**防 segfault** 的
/// （`SHADING_LANGUAGE_VERSION` 是 GL 2.0 才有的枚举），所以我们拿到的是一条
/// 干净的 `Err`，不是崩溃。
///
/// ★ **不能只看 `Error::OpenGL` 这个变体** —— 同一个变体也用来报「着色器编译
/// 失败」之类的毛病，一律说成"没装显卡驱动"会把人带沟里。所以连消息一起认。
/// （`PainterError` 的字段是私有的，只能走 `Display`；它的 `Display` 形如
/// `"OpenGL: <消息>"`，因此直接找消息里的特征串。）
fn 是缺_opengl(e: &eframe::Error) -> bool {
    match e {
        eframe::Error::OpenGL(p) => p.to_string().contains("requires opengl"),
        _ => false,
    }
}

// ─────────────────────────── 测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条**真实**的 egui_glow 建窗失败。
    ///
    /// 走 `eframe` 重导出的 `egui_glow`（`pub use {egui_glow, glow};`），
    /// 用它的 `From<String>` 构造。比"自己拼个字符串然后当参数传"强的地方在于：
    /// 万一上游改了变体的形状，这里**编译不过**，而不是悄悄退化成永远为假、
    /// 让测试继续"绿着"却什么都没验。
    fn 建窗失败(消息: &str) -> eframe::Error {
        eframe::Error::OpenGL(eframe::egui_glow::PainterError::from(消息.to_string()))
    }

    #[test]
    fn 缺opengl_认得出驱动只给_1_1_的那一条() {
        // ★ 消息原文抄自同事机器（yuhongzhen04914）2026-10-13 的真实日志。
        let e = 建窗失败("egui_glow requires opengl 2.0+. ");
        assert!(是缺_opengl(&e), "这条必须认出来，否则弹窗又退回让人自己猜");
    }

    #[test]
    fn 缺opengl_不把同变体的别的毛病算进去() {
        // 着色器编译失败走的是**同一个** `Error::OpenGL` 变体，但那是另一回事 ——
        // 说成「没装显卡驱动」会把人带到沟里去。这条测试盯的就是这个区分。
        let e = 建窗失败("Failed to compile shader:\n0:1(10): error: syntax error");
        assert!(!是缺_opengl(&e));
    }

    #[test]
    fn 缺opengl_不认别的变体() {
        // 拿一个跟图形无关的失败做对照，确认 `_ => false` 那条早退分支是对的。
        let e = eframe::Error::AppCreation(Box::new(std::io::Error::other("boom")));
        assert!(!是缺_opengl(&e), "非 OpenGL 变体不该被认成显卡问题");
    }
}
