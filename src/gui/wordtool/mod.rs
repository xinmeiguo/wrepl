//! 「页眉替换」与「打印与导出」。
//!
//! ## 两条路径
//!
//! **页眉替换**只有纯 Rust 一套引擎（[`wrepl::hdr`]）：按 `default` / `first` /
//! `even` **三种页眉部件逐个**处理，**不要求装 Word**，落盘可选"写副本（默认）"
//! 或"就地覆盖"。
//!
//! 2026-10-19 起，老的 **Word COM 实现已退役删除**（`services` 里那两条
//! 文本 / 图片替换，以及只服务于它的 `locator`）。原因：它只写 `default`
//! 一个部件、还要求目标机器装 Office，而纯 Rust 那套从 2026-10-11 起就是默认路径，
//! 已经在真实交付件上跑了半年多；两套并存唯一的用处是"哪条出问题切另一条"，
//! 留着反而让界面多一个必须解释的选择。
//!
//! **打印与导出**仍只有 COM 一条路 —— 打印与 PDF 排版本身就得 Word 出手，
//! 纯 Rust 做不到（见 [`engine`] 的引擎探测与 WPS 降级）。
//!
//! ★ 退役前留下的一条知识仍然成立：**「只写主页眉部件」≠「首页不变」** ——
//! 页面显示哪个部件由 `w:titlePg`（首页不同）/ `settings.xml` 的
//! `evenAndOddHeaders`（奇偶页不同）决定；**两个都没开时（这批模板的常态），
//! 首页 / 奇数页 / 偶数页统统显示主页眉**。
//!
//! ## 与来源的关系
//!
//! `com` / `printers` / `services` / `wd` / `word_session` 取自
//! `E:\test\word_header_tool_rust`（v0.2.0），**原样搬运**，只把内部引用由
//! `crate::` 改为 `super::`。逻辑上只删不改：退役页眉 COM 时移除了
//! `services` 的文本 / 图片两条服务与 `locator` 模块。
//!
//! `ui.rs` 改动较大：并入主界面的设计系统、选文档改走主界面那套自绘选择器、
//! 页眉替换保留落盘开关，并把界面日志镜像进诊断日志。

pub mod com;
pub mod engine;
pub mod printers;
pub mod services;
pub mod ui;
pub mod wd;
pub mod word_session;

pub use ui::{HeaderMode, PickDoc, Sink, WordTool};
