//! 「页眉替换」与「批量打印」。
//!
//! ## 两条路径（2026-10-11 起）
//!
//! **页眉替换**现在有两套引擎，界面上可切换（[`ui::Engine`]）：
//!
//! | 引擎 | 依赖 | 写哪些页眉部件 | 落盘 |
//! |---|---|---|---|
//! | 纯 Rust（[`wrepl::hdr`]） | 无 | 主页眉 + 首页眉 + 偶页眉 | 写副本（默认）/ 就地覆盖 |
//! | Word COM（[`services`]） | 需装 Microsoft Word | **只写 default（主页眉）这一个部件** | 只能就地覆盖 |
//!
//! ★ **"只写主页眉部件"不等于"首页不变"** —— 这是两件事，2026-10-11 xin 实测纠正过：
//! 页面显示哪个部件由 `w:titlePg`（首页不同）/ `evenAndOddHeaders`（奇偶页不同）两个
//! 开关决定；**两个都没开时（这批模板的常态），首页 / 奇数页 / 偶数页统统显示主页眉**，
//! 所以首页与偶数页上看到的同样是替换后的内容。详见 [`ui::Engine`] 的文档。
//!
//! 两套都留着是刻意的：COM 那套从"页眉替换没生效"的报告里**拿不到真实报错**
//! （界面日志过去不落盘，现在由 [`ui::WordTool`] 的 `log` 统一镜像到 `wrepl-gui.log`），
//! 在根因没查清之前，把纯 Rust 定成唯一路径等于把"能不能修好"押在一个人身上。
//!
//! **批量打印**仍只有 COM 一条路 —— 打印本身就得 Word 出手，纯 Rust 做不到。
//!
//! ## 与来源的关系
//!
//! `com` / `locator` / `printers` / `services` / `wd` / `word_session` 六个模块
//! 取自 `E:\test\word_header_tool_rust`（v0.2.0），**原样搬运**，只把内部引用由
//! `crate::` 改为 `super::`（共 10 处），逻辑一行未改。
//!
//! `ui.rs` 改动较大：并入主界面的设计系统、选文档改走主界面那套自绘选择器、
//! 页眉替换加了引擎开关与落盘开关，并把界面日志镜像进诊断日志。

pub mod com;
pub mod locator;
pub mod printers;
pub mod services;
pub mod ui;
pub mod wd;
pub mod word_session;

pub use ui::{Engine, HeaderMode, PickDoc, WordTool};
