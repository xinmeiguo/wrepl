//! 「页眉替换」与「批量打印」——从 `word_header_tool_rust` 移植过来的 Word COM 部分。
//!
//! ## 与 wrepl 其余部分的边界
//!
//! wrepl 的正文替换是**纯 Rust 改 OOXML**，不依赖 Office；本模块走 **Word COM**，
//! 所以**需要目标机器装 Microsoft Word**（WPS 不认这套接口）。
//! 这条边界要一直摆在明面上：正文替换在什么机器上都能跑，页眉替换与打印需要 Word。
//!
//! ## 来源
//!
//! 代码取自 `E:\test\word_header_tool_rust`（v0.2.0），模块**原样搬运**，
//! 只把内部引用由 `crate::` 改为 `super::`（共 10 处），逻辑一行未改。

pub mod com;
pub mod locator;
pub mod printers;
pub mod services;
pub mod ui;
pub mod wd;
pub mod word_session;

pub use ui::{HeaderMode, PickDoc, WordTool};
