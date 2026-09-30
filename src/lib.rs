//! # wrepl 内核
//!
//! 这个 crate 同时提供三种东西，**共用同一份实现**：
//!
//! 1. 库（本文件）——替换引擎、规则模型、验证、报告、侦察
//! 2. `wrepl` 命令行（`src/main.rs`）
//! 3. `wrepl-gui` 图形界面（`src/gui/main.rs`，需 `--features gui`）
//!
//! ## 为什么必须共用
//!
//! 界面和命令行只要各自实现一遍匹配或落盘逻辑，两边就会慢慢漂移，
//! 最后出现"命令行跑出来是这样、界面跑出来是那样"——这类问题几乎无法排查。
//! 所以**所有**实质逻辑都放在这里，两个前端只负责收集参数和展示结果。
//!
//! ## 模块地图
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`docx`] | ZIP 容器读写、part 扫描、字节级定点改写 |
//! | [`engine`] | 匹配引擎——`scan` 与 `apply` 共用，不允许"预览一套、执行一套" |
//! | [`rules`] | 规则模型与三种来源（命令行 / 文本文件 / Excel）的解析 |
//! | [`naming`] | 文件名同步改名（与 [`engine`] **同一套**匹配语义） |
//! | [`verify`] | 格式保全关卡 1+2、残留自检、批量验证 |
//! | [`report`] | 归档级 Excel 报告（封面 + 汇总 + 明细 + 文件清单 + 规则快照） |
//! | [`probe`] | 侦察：从目录里扫出候选项目编号与客户名 |
//! | [`cli`] | 命令行参数定义（GUI 不依赖它） |

pub mod cli;
pub mod docx;
pub mod engine;
pub mod naming;
pub mod pipeline;
pub mod probe;
pub mod report;
pub mod rules;
pub mod verify;
