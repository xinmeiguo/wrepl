# 路线图

> **这个文件只记"还没做的"和"为什么先不做"。**
> 做完的进 [`CHANGELOG.md`](../CHANGELOG.md)；过程、约束与验证链进 [`CONTRIBUTING.md`](../CONTRIBUTING.md)。
>
> 最后更新：2026-10-13（0.4.2 已发版）

## 0. 顺序不是按"重要性"排的，是按**依赖**排的

下面几项之间有前后关系，跳着做会返工：

| # | 项 | 依赖 | 状态 |
|---|---|---|---|
| 1 | `egui_sauge` 上游 74 条 `falling back to f32` | 无 | ✅ 本机 fork 修掉（2026-10-12，见 §2.1.1） |
| 2 | 清 7 条自家弃用警告（egui 0.34 面板 API） | 无 | ✅ 2026-10-12，见 §1.1.1 |
| 3 | 拖放文件 / 文件夹进窗口 | 无 | ✅ 2026-10-12，见 §1.2.1 |
| 4 | 「最近使用」目录持久化 | 无 | ✅ 2026-10-12，见 §1.3.1 |
| 5 | 键盘快捷键 | **依赖 3、4** | ✅ 2026-10-12，见 CHANGELOG 0.4.2 |
| 6 | 更大项（PDF 导出 / 退役页眉 COM / 界面预演入口） | 各自独立 | 暂缓（见 §2） |

> **0.4.2 已发版（2026-10-13）**：上表 1–5 全部落地，正文见 [`CHANGELOG.md`](../CHANGELOG.md) 的 0.4.2 一节。
> 同期**补发了 `v0.4.1`**（提交 `b428b78`）—— 它 2026-10-12 只提交、没推送，
> 所以 `v0.4.0` 之后 Release 页上一直没有它；详见 CHANGELOG 0.4.1 的「发版记录」。

**为什么 5 依赖 3、4**：快捷键要绑的是"起手动作"（`Ctrl+O` 选目录、`F5` 扫描、
`Ctrl+Enter` 执行），而 3、4 改的正是"用户怎么把输入塞进来、下次从哪儿开始"。
起手方式没定型就先绑键，等于把要改的交互再焊一遍 —— 一改就是两处。

**为什么 1 和 2 放同一版**：都是"消警告"，一个在依赖侧、一个在自家代码侧，
合计就是"编译输出干净"。拆两个版本会让 `CONTRIBUTING.md` 里那条
「警告数 = 7」的判据在一个版本里先对后错，白改一遍文档。

---

## 1. 近期待办（做完就删）

### 1.1 清 7 条自家弃用警告

**在哪**：`src/gui/app.rs` 的 4 处、7 条 ——

| 行 | 告警 |
|---|---|
| 1227 | 弃用别名 `TopBottomPanel` + 弃用方法 `Panel::show` |
| 1257 / 1260 | 弃用别名 `SidePanel` + 弃用方法 `Panel::show` |
| 1291 | 弃用别名 `TopBottomPanel` + 弃用方法 `Panel::show` |
| 1418 | 弃用方法 `CentralPanel::show` |

**怎么改**：egui 0.34 起面板只有 `show_inside(ui, …)` 一条路。

```rust
egui::TopBottomPanel::top("title").show(ctx, |ui| { … })
//   → egui::Panel::top("title").show_inside(ui, |ui| { … })

egui::SidePanel::left("nav").exact_size(184.0).resizable(false).show(ctx, |ui| { … })
//   → egui::Panel::left("nav").exact_size(184.0).resizable(false).show_inside(ui, |ui| { … })

egui::TopBottomPanel::bottom("actions").show(ctx, |ui| { … })
//   → egui::Panel::bottom("actions").show_inside(ui, |ui| { … })

egui::CentralPanel::default().show(ctx, |ui| { … })
//   → egui::CentralPanel::default().show_inside(ui, |ui| { … })
```

`show_inside` 要的是 **`Ui`**，所以渲染入口也得从 `update(ctx)` 搬到
`ui(ui)`：`App::render` 的签名由 `&egui::Context` 改成 `&mut egui::Ui`，
内部 `let ctx = ui.ctx().clone();` 供主题 / 重绘请求 / 两个浮层继续用
（`Context` 内是 `Arc`，克隆很便宜）。**同时删掉 `update`** ——
eframe 0.34 是 `logic → update → ui` 三条都调（`epi_integration.rs:294/299`），
留着 `update` 就有两条非空通路，一帧画两遍。

**注意**：`app.rs` 里有 6 处 selftest 调用 `app.render(ui.ctx())`，要一并改成
`app.render(ui)`。`exact_size` **没有**被弃用（被弃用的是 `exact_width` /
`exact_height`），不用改。

**判据**：`wrepl-gui` 的警告汇总行**整条消失**（不是变少）。

#### 1.1.1 执行记录（2026-10-12，已验收）

| 项 | 实测 |
|---|---|
| 改动文件 | `src/gui/app.rs`（77 行）、`src/gui/ds.rs`（7 行），净 `+49 / -35` |
| 四个面板 | `Panel::top("title")` / `Panel::left("nav")`（`.exact_size(184.0).resizable(false)`）/ `Panel::bottom("actions")` / `CentralPanel::default()`，全部走 `show_inside(ui, …)` |
| 渲染入口 | `App::render` 签名 `&egui::Context` → `&mut egui::Ui`；`impl eframe::App` 里新增 `fn ui(&mut self, ui, _frame)` 转发到它，**`update` 已删除**（eframe 0.34 三条通路都调，留着会一帧画两遍） |
| 内部取 ctx | `let ctx = ui.ctx().clone();`（`Context` 内是 `Arc`，克隆很便宜）供 `ds::apply` / `start_run` / `confirm_inplace_dialog` / `handle_picker` 用 |
| 测试调用点 | 6 处 `app.render(ui.ctx())` → `app.render(ui)`（`replace_all`） |
| `exact_size` | **没有**被弃用（弃用的是 `exact_width` / `exact_height`），保留原样 |
| **判据达成** | `cargo check --features gui` 的 `wrepl-gui` 警告汇总行**整条消失**；`grep -c "wrepl.src.gui" <日志>` = **0**（自家代码零警告） |
| 其余验收 | release 构建 `EXIT=0`（8m52s）；回归 43 项 ✓ + 第 19 组 17/17；与历史基线产物 **26/26 SHA256 相同**；GUI↔CLI 产物 **26/26 相同**；单元测试 73 passed；界面实拍与改动前一致（四个面板渲染无变化） |

⚠ **egui_sauge 那 2 条 `os error 5` 不是代码警告** —— 是沙箱删除拦截层导致
`error deleting lock file for incremental compilation` / `error copying object file`，
与源码无关（见 `CONTRIBUTING.md` §2.1）。**统计警告数时要把这类行排除掉**，
否则会误判成"egui_sauge 又出警告了"。

#### 1.1.1 执行记录（2026-10-12，已验收）

四处面板按上面那条 `show_inside(ui, …)` 改完，另外两处签名跟着走：

| 项 | 实测 |
|---|---|
| 面板 | `Panel::top/left/bottom(...).show_inside(ui, …)` + `CentralPanel::default().show_inside(ui, …)`；`.exact_size(184.0).resizable(false)` **原样保留**（被弃用的是 `exact_width`/`exact_height`，不是 `exact_size`） |
| 签名 | `App::render(&mut self, ui: &mut egui::Ui)`；内部要 `Context` 的地方用 `ui.ctx()` |
| 生命周期 | `impl eframe::App` 里**删掉 `update`、换成 `ui`** —— eframe 0.34 的 `logic → update → ui` 三条通路都会调，留着 `update` 等于一帧画两遍 |
| 自家警告 | **0 条**（`generated N warnings` 汇总行不再出现） |
| 回归 | 43 项 ✓ + 第 19 组 **17/17**，0 失败 |
| 基线产物 | 与 `E:/test/docx-009CE` **26/26 SHA256 相同**；GUI 产物 vs CLI 产物也 26/26 |
| 实拍 | 四个面板渲染与改动前一致 |

> 顺带修掉一处过时注释：`app.rs` 里引述旧版 egui 文档的 `SidePanel` /
> `TopBottomPanel` 已改成当前 API 名，并注明"GUIDE 写的是 0.34 之前的名字"。

### 1.2 拖放文件 / 文件夹

现在把资源管理器里的文件拖进窗口**什么都不会发生** —— 全仓 `dropped_files`
零命中。

**落点**按当前页决定，并且**复用 `App::apply_picked` 这条唯一的漏斗**
（拖放只是"又一种选路径的壳"，与自绘面板、系统对话框并列）：

| 当前页 | 落点 | 收什么 |
|---|---|---|
| 批量替换 | 「输入」栏 | 目录与文件**混着收**（`input` 本来就支持，工具自己会遍历目录） |
| 页眉替换 | 该页「已选文档」 | 只收 `.doc` / `.docx`（与那两个入口的选择器白名单一致），目录**展开一层**（不递归） |
| 批量打印 | 同上 | 同上 |

按住 `Ctrl` 是**追加**到现有内容，否则替换 —— 与资源管理器多选的习惯一致，
也与 `picker.rs` 里那枚框选橡皮筋一致。

**还要有悬停提示**：「这个窗口能接拖放」在界面上没有任何静态线索，
拖过来的当口得说一句。用 `egui::Area`（`Order::Foreground` + `interactable(false)`）
而不是往页面里插一行 —— 拖拽中 `hovered_files` 每帧都变，插进页面会把内容顶得来回跳。

**实现要点**：`dropped_files` / `hovered_files` 是**每帧**的输入
（`RawInput` 里那两个字段），落到哪一帧就在那一帧取走，不能缓存。

#### 1.2.1 执行记录（2026-10-12，已验收）

| 项 | 实测 |
|---|---|
| 落点分派 | `handle_drop` 按 `self.page` 三分：批量替换 → `input` 栏（不过滤）；页眉 / 打印 → 各自那份「已选文档」（只收 `.doc`/`.docx`，目录**展开一层不递归**） |
| 复用漏斗 | 「批量替换」这一支直接写 `input`（该栏本来就吃目录+文件混合列表，工具自己会遍历目录）；另两支走 `WordTool::set_docs`。**没另开一条路** |
| 追加语义 | 按住 Ctrl（或 Cmd）= 追加，否则替换 —— 与资源管理器 Ctrl 多选、以及 `picker.rs` 那枚框选橡皮筋一致 |
| 悬停提示 | `drop_overlay` 用 `egui::Area`（`Order::Foreground` + `interactable(false)`），盖住 `content_rect` 面板区 + 品牌绿描边 + 「松手即可加入清单」 |
| 调用位置 | `render` 末尾，**排在 `handle_picker` 之后** —— 选择器 4a 版已改成 `egui::Window`（刻意无遮罩），但顺序仍按"最上层提示最后画"排 |
| **判据达成** | release 构建 **0 warning**；`--selftest` 6 条拖放断言全过；回归 55 项 ✓ + 第 19 组 **17/17**，0 失败；基线产物 SHA256 26/26 相同（内核未动） |
| 实拍 | 悬停遮罩 + 落地后「输入栏出现 `E:/test/docx`、执行按钮由灰变红」两张，见 `docs/` 外 `_shot-drop-*.png`（本机截图 artifact，不入库） |

**新增 selftest 断言**（`--selftest` 里那 6 条，都是静默退化型，坏了不报错）：

1. 拖拽悬停遮罩：布局 1 帧
2. 拖放到「输入」（目录 + 文件混收）
3. 第二次拖放（**无 Ctrl**）是**替换**而非追加
4. 按住 Ctrl 拖放是**追加**
5. 拖放到「页眉替换」—— `.txt` / `.xlsx` 被静默跳过，只留 `.docx`
6. 拖放到「批量打印」—— 收到自己那份，且**没有**串到页眉那份

**新增调试开关** `WREPL_DEBUG_DROP=hover|drop[;路径;路径]`（`src/gui/diag.rs`）——
本机合成鼠标进不来窗口，拖放是"按住不放"才有的状态，真实操作构造不出来。
`hover` 只画遮罩不落地；`drop;D:/a;D:/b` 直接走落地分派。

> ⚠ **写这块时踩的一个坑，值得记住**：`hover` 那一版改用 `ctx.layer_painter`
> 还是 `egui::Area`，屏幕上一个像素都没变，一度以为"绘制层被前面的浮层盖住了"，
> 改了三轮调用顺序与 `Order`（`Foreground` / `Tooltip`）全都不奏效。
> 后来逐像素比对才发现**遮罩一直是画出来的**（中心像素 `(165,165,165)` =
> 半透明黑罩在白底上的结果）—— 误判来自**图片查看工具在复用同一张图的缓存**，
> 每次改完重拍的文件名不同、但工具认的是像素内容。
> ⇒ **判据**：截图"没变化"时先 `sha256` 比像素，别急着改代码。

### 1.3 「最近使用」目录持久化

**现状**：`picker.rs:263` 明写"只存内存，不落盘"。关掉程序再开，
选择器忘了上次停在哪儿。

**做法**：写 `%LOCALAPPDATA%\wrepl\`（与 `diag.rs` 的日志同目录，
`diag.rs:36-45` 那个 `log_path()` 已经算好了基址）。存**一个**路径就够 ——
选择器是常驻单实例，四个模式共用同一份"上次停留"。

#### 1.3.1 执行记录（2026-10-12，已验收）

| 项 | 实测 |
|---|---|
| 落盘位置 | `%LOCALAPPDATA%\wrepl\last-dir.txt`，**一行一个路径**（不存那 5 条「最近使用」—— 那是会话内的导航痕迹，跨会话只需要"停在哪儿"） |
| 写盘时机 | **只在读目录成功时**（`push_recent`，由"读取完成"那条路径触发）。绘制路径上一行 IO 都没有 —— 这是模块头**约束 4** 的硬要求 |
| 读回时机 | `Picker::new()` 里一次，读出来同时给 `cwd` 与 `last_dir`（面板关着时两者不会差） |
| 存不进去 | `save_last_dir` 写失败**只记一行日志、不影响任何正事** —— 这是一份便利状态，不是配置 |
| 读不出来 | `load_last_dir` 拿不到、或那个目录**已经不在了**（盘拔了 / 被删了）→ 当没有，退回 home。**绝不因此让程序起不来** |
| 判据达成 | release 构建 0 warning；回归 50 项 ✓ + 第 19 组 **17/17**，0 失败；基线产物 SHA256 26/26 相同 |

**端到端实测**（真起进程、跨重启）：

1. 预置 `last-dir.txt` = `E:/test/docx` → 启动，选择器起点 `E:/test/docx` ✅
2. 跑完后文件内容仍是 `E:/test/docx`（读成功就刷新）✅
3. 改成一个**不存在的**盘符 `Z:/不存在的盘/某目录` → 启动**不崩**，
   起点退回 `C:\Users\xinwei01307` ✅（`is_dir()` 那一关把它挡住了）

⚠ **注意 `is_dir()` 那一关不是多余的**：`E:/test/regress` 在本机已不存在，
写进去时看着"保存成功"，读回时必须被过滤掉 —— 否则选择器会停在一个
点不回来的路径上，用户看到的是一个空列表且不知道为什么。

> 顺带发现（**不是这一步引入的**，是既有顺序）：`WREPL_DEBUG_PICKER` 触发的
> `open_picker` 跑在 `--preset` 应用**之前**，所以带 preset 启动时选择器起点
> 仍是 home 而不是 preset 里那个目录。属于诊断开关的既有行为，没动。

---

## 2. 拍板记录（做过决定，别再翻烧饼）

### 2.1 `egui_sauge` 的 74 条：选 **(c) 本地 fork 并记档**

| 选项 | 为什么不选 |
|---|---|
| (a) 不修，每次编译忍受 74 条 | ❌ 它们是 `future_incompatible`（lint `float_literal_f32_fallback`，`Stroke::new(1.0, …)` 的 `1.0` 被推断成 `f64`）。Rust 官方原文注明 **"will become a hard error in a future release"**（rust-lang/rust#154024）。**升一次工具链就编译失败**，而它是本机唯一副本 → 整个项目编不出来 |
| (b) 等上游修 | ❌ 上游 `macnjil4/egui_sauge` 最近一次提交就是 v2.0.0 的发布提交（`2b2feb16`，2026-04-26），`pushed_at` 停在 2026-04-26，**半年没有新提交**；crates.io 最新版也是 2.0.0。等不到 |
| **(c) 本地 fork 并记档** | ✅ 选它 |

**做法**：把修好的源码 **推到自家 fork 并推上 GitHub**，CI 改指自家仓库（见 §6.3）。
补丁 `docs/egui_sauge-fork.patch` 仍然保留 —— 它现在的用途是**记录 fork 相对上游的偏离**，
以及将来上游真修了 / 我们要合并上游新版本时用来重放。

#### 2.1.1 执行记录（2026-10-12，已验收）

| 项 | 实测 |
|---|---|
| 改动范围 | `egui_sauge/src/` **35 个文件 / 74 处**，全部是浮点字面量加后缀（`Stroke::new(1.0, …)` → `Stroke::new(1.0_f32, …)`） |
| 语义 | 零变化 —— 补丁里除 `_f32` 后缀外没有任何别的改动 |
| **机器码** | **同一路径下实测**：把副本原地反向打补丁回到上游原样、编译 LLVM IR，再正向打回来编译一次，两份 `.ll` **SHA256 完全相同**（`6a9df610…`）⇒ 打不打这个补丁，生成的机器码一个字节都不差 |
| 补丁 | `docs/egui_sauge-fork.patch`（711 行 / 35 个 hunk 组） |
| 可重放 | ✅ 已验证：拿上游 v2.0.0 原始 `src/` `patch -p1` 打上它 → 与本机副本 `diff -rq` **零差异** |
| `Cargo.toml` / `Cargo.lock` | 已还原成上游原样（修的时候临时摘过 dev 依赖、挪走过 lock，修完 `cp` 回去并 `diff` 确认一致）—— **fork 只动 `src/`** |
| 验收 | release 构建 `EXIT=0`；全量回归 18 组 + 第 19 组 17/17 全过；与历史基线产物 **26/26 SHA256 相同**；GUI 产物与 CLI 产物 **26/26 相同**；`--selftest` 通过 |
| **已推上 GitHub** | ✅ `xinmeiguo/egui_sauge`（fork，Apache-2.0 已保留），提交 `095ae21`；**CI 已改指该仓库**，见 §6.3 |

> **「两边不同」到底差在哪**：源码不同，**编译结果相同**（上表实测）。现在
> 本机 / CI 编的是**同一份源码**（同一个 SHA），差别只剩编译路径与工具链。
> 真正的守门仍然是本机 `regress.sh`；CI 证明的是「干净环境能从 GitHub 拉到这份源码并编过」。

**踩到的坑**（下次重跑别再撞）：① 在 wrepl 目录 `cargo fix -p egui_sauge` 会被拒
（工作区外的包不能再带 `--features`）；② egui_sauge 目录没有 `rust-toolchain.toml`，
不显式指定 GNU 工具链会退回 msvc，链接阶段 `link.exe` 失败；③ 该目录自带 `Cargo.lock`
含 wgpu 链（dev 依赖 `eframe` 默认 feature），离线解析不到，要临时挪走。
解法都写在 `CONTRIBUTING.md` §1.2。

#### 2.1.2 这 74 条到底是谁的（2026-10-12 查证）

**告警点是 egui_sauge 自己写的字面量，egui / epaint / eframe 自身 0 条。**
81 条定位拆开：74 条在 egui_sauge 的 35 个文件（`theme/apply.rs` 12 条最多），7 条在
wrepl `src/gui/app.rs`。

链路（最小复现已跑通，`d/test/_demo-f32/demo.rs`）：

| 写法 | 结果 |
|---|---|
| `Stroke::new(1.0, …)`（egui_sauge 的 74 处） | ⚠ 告警 |
| `Stroke::new(1.0_f32, …)`（补丁改成的） | ✅ 无告警 |
| 形参写成具体 `f32`，传 `1.0` | ✅ 无告警 |
| 形参写成 `impl Into<f32>`，传 `1.0`（**egui 就是这个签名**） | ⚠ 告警 |

egui 之所以用 `impl Into<f32>`（`epaint-0.34.3/src/stroke.rs:24`）是有意的 ——
让调用方既能传 `f32`、也能传别的可转类型，**不是 bug**。告警落在调用方，是因为 Rust
**要移除一条老机制**：类型推断过去靠「某个 trait 只有一个 impl」来引导
（如 `From<f32> for f32` 唯一，于是 `0.0` 能推成 `f32`）；**加了 `From<f16> for f32`
之后这个唯一性没了**，那条路迟早要封（rust-lang/rust#154024，2026-03-17 立项，
本机 rustc 1.97.1 已在报）。官方表态就是"别依赖它"，所以该改的是依赖方 ——
加 `_f32` 后缀是**面向未来的正确写法**，不是临时糊墙。

**上游为什么没修**（事实链，不是推测）：

- `macnjil4/egui_sauge`：**3 star、0 open issue**，最后一次提交 `2b2feb16`
  停在 **2026-04-26**（就是 v2.0.0 的发布提交），此后 **0 提交**。
- 他们 CI 是 `cargo clippy --all-targets --all-features -- -D warnings`（stable Linux）
  + stable/1.92 双工具链。**今天拿他们这条命令跑上游原样源码是红的** ——
  实测 74 条 warning 在 `-D warnings` 下全变 `error`；打上本补丁后退出码 0。
- 所以不是"不愿改"，是**还没被咬到**：他们发 2.0.0 那天的 stable 大概率还没这条
  警告（本机是 1.97.1 / 2026-07-14；1.96、1.97 的发布说明里都没提它），此后仓库就没人动。
  ⚠ 这一条是从"CI 带 `-D warnings` 却能正常发版"反推的，属**推断**，不是直证。
- 想省事的话：等他们发新版直接 `patch -p1` 重放；给他们提 PR 也行，但以目前
  3 star / 半年无提交的状态，大概率没人看。

**★ 网络不是问题**：`api.github.com` / `codeload` / `crates.io` 都在放行名单里
（只有 `git ls-remote github.com` 会 502）。已实测连通（HTTP 200，~0.6s）。
所以"同步一下"是能做的动作，只是**现在上游没有新东西可同步**。

### 2.2 更大项：暂缓

| 项 | 为什么缓 |
|---|---|
| 批量导出 PDF（`ExportAsFixedFormat`） | 交付常要 PDF，但打印页那套骨架得先稳；且它仍要装 Word |
| 退役页眉那半 COM | 纯 Rust 已是默认且覆盖三种页眉 + 文本/图片。但 `CHANGELOG` 写明"两套都留是刻意的"——**先拿一批真实文档把纯 Rust 跑稳再动** |
| 界面加「预演」入口 | `app.rs:303` 明写"界面上没有预演入口"，是**刻意的设计**；pipeline 的 dry 路径已经存在。要加得先想清楚"点完之后用户看到什么"，不然又是一个要改的交互 |

---

## 3. 每步的验收标准（**照抄，别凭印象**）

任何一步改动，都按这一套核；数字不对就是没做完：

```bash
cd /d/test/wrepl

# ① 类型检查：0 error，且「自家代码零警告」
cargo check --features gui --offline 2>&1 | tail -3

# ② 单元测试
cargo test --features gui --offline --bin wrepl-gui 2>&1 | tail -8

# ③ release 构建 + 自检（自检要求逐字一致）
cargo build --release --features gui --offline 2>&1 | tail -4
D:/test/cargo-target/release/wrepl-gui.exe --selftest <语料> <输出目录> <规则文件>

# ④ 全量回归：78 项全过 + 26/26 SHA256 与历史基线**完全相同**
bash regress.sh D:/test/cargo-target/release/wrepl.exe \
                D:/test/cargo-target/release/wrepl-gui.exe
```

**四条硬判据**（`CONTRIBUTING.md` 里那套，这里再抄一遍便于对照）：

1. **编译零错误**；
2. **自家代码零警告** —— 判据是 `wrepl-gui` 的 `generated N warnings` 汇总行
   **不存在**，以及日志里找不到指向 `wrepl\src` 的 `warning`。
   （`egui_sauge` 那份是依赖侧的另一笔账，见 §1.1 与 §2.1）
3. **`--selftest` 逐字一致**；
4. **产物 SHA256 26/26 与基线完全相同** —— 由 `regress.sh` 第 7 组算：
   与 `E:/test/docx-009CE`（历史基线产物目录）逐字节比对；第 9 组再比一遍
   GUI 产物 vs CLI 产物。
   （旧文档里提的 `_hash-_step4a-gui.txt` 已在 2026-10-11 的 D:/test 清理中删掉了，
   基线以 `E:/test/docx-009CE` 为准。）

**外加一条肉眼判据**：涉及界面的改动，要**实拍截图**并亲眼看——
改过布局就必须看图，别只看"编译过了"。

> ⚠ **截图对比的口径**：`_step4a-*.png` 里 x192–202 那条整高黑竖带**不是代码**，
> 是那一次截图的 artifact（用改动前后的 exe 现拍三页都没有）。**别把它当基线**，
> 要对比就"同一环境现拍"。

---

## 4. 遗留项（品味 / 尺寸问题，已决定先不动）

这些是**知道但先不改**的，列在这里免得每次评审重新翻一遍：

| 项 | 说明 |
|---|---|
| `ds::Checkbox` 字号写死 13px | 没走设计系统的字号层级，跟 `Style::text_styles` 对不上。改它要动 `egui_sauge`，且会牵动全部复选框的观感 |
| `ds::Card::show` 不自动撑宽 | 卡片的宽度得调用方自己给，漏给就窄一条。属上游部件的行为，改了是第二处 fork |
| `egui_sauge` 缺三个语义色 | 现在用 `pal.dir` / `ERR_C` 之类顶替，语义上不严格。等上游补（或并进 §2.1 那个补丁，但要单独立项） |
| 选择器底栏比设计稿高约 20px | 底栏那行要同时放「文件名」框 + 筛选说明 + 「显示隐藏项」 + 两个按钮，压不下去。踩过一次"收窄→否掉"，见 CHANGELOG 0.4.1 |
| 自绘图标保留原色 | 盘符 / 文件夹图标是自绘的、不跟随主题色（`RowIcon`）。跟随要重画那套图形 |
| `PERFORMANCE.md` §4 那三项 | `residue` 重复解压（4 ms/文件）、界面报告强制整文件 SHA256（247→331 ms）、启动读 18.8 MB 字体。后两项文件里就写着"属于产品决定" |

---

## 5. 工程 / 仓库小项（不急，但迟早要还）

| # | 项 |
|---|---|
| 5.1 | ~~仓库根混着 `regress-group19.py`(10 KB)、`run_regress.py`~~ **已处理（2026-10-13）**：`run_regress.py` 是本机专用包装（写死 Git Bash 绝对路径与 `D:/test` 工作目录），已移出仓库到 `D:/test/wrepl-local/` 并加 `.gitignore`。`regress-group19.py` **保留在仓库根** —— 它被 `regress.sh` 按 `$SRC/regress-group19.py` 引用（`SRC` 默认值就是仓库根），移走会让第 19 组直接报红 |
| 5.2 | **测试覆盖不均**：83 个 `#[test]` 集中在 `docx` / `hdr` / `naming` / `pipeline` / `probe` / `rules` / `verify` / `picker`；`app.rs`（3032 行）与 `wordtool/`（约 2100 行）**一个测试都没有** —— 恰恰是最近改动最多的地方 |
| 5.3 | 版本语义：0.4.1 带进了 `src/hdr.rs`（2591 行）整套引擎，严格讲够 minor。**不回改**，但以后新增整套引擎时按 minor 走。★ 原注写的"tag 已发"是**错的** —— `v0.4.1` 当时只提交未推送，实际到 2026-10-13 才与 `v0.4.2` 同期补发（见 CHANGELOG 0.4.1 的「发版记录」） |
| 5.4 | ~~`regress.sh` 的可移植性是半成品~~ **已处理（2026-10-13）**：① 第 4 组的 `CHSRC` 曾**写死**成 `D:/test/regress/CH-charref.docx` —— 全脚本唯一一处不可覆盖的输入，外人配齐语料也照红；现提成 `${WREPL_CHSRC:-}`。② `run_option_matrix.py`（第 3 组）只活在某个 WorkBuddy 会话目录里、不在仓库 —— 它其实**完全通用**（无客户数据、无本机路径），已收进仓库根。③ 新增 `skip()` 语义：可选夹具（`CORPUS_BASELINE` / `MATRIX` / `CHSRC`）缺了打印黄色 `−`、计入「跳过」、**不算失败**，让 clone 后的人能分清"我缺数据"和"代码坏了"。分界原则写进 `CONTRIBUTING` §3.1：**逻辑入库、夹具自备** |

---

## 6. 依赖复查：`egui_sauge` 的生命周期风险（2026-10-12）

用户问"既然它是个小项目，当初为什么推荐用它"——复查后把客观数据摆在这儿，
好让下一次评审不用重新查：

| 指标 | 实测值 |
|---|---|
| crates.io 总下载量 | **57** |
| GitHub stars / open issue | **3 / 0** |
| 仓库创建日 → 最后提交 | 2026-04-25 → **2026-04-26**（历史共 **12 个提交**，8+4 集中在两天） |
| 作者 | `lv5839` ×10、`macnjil4` ×2（单人） |
| 发布节奏 | 1.1.0 / 1.2.0 / 1.3.0 / 2.0.0 **全在同一天到次日**；crates.io 上实际只有 2.0.0 与 1.2.0 |
| 代码量 | `src/` 48 个 `.rs`、**8684 行** |
| 本项目引入时间 | 2026-10-11（v0.4.0，commit `a82b8b1`）—— 上游发布后约 5 个半月 |

**当时推荐它的理由（成立）**：一次性给到 sage 绿主题、九档字号层级、WCAG AA、
Phosphor 图标、中文字体拼装，外加 Card / InputField / SelectField / RadioGroup /
Dialog / Toast 等成套组件。不接就得自己写主题与组件，**功能匹配是真的**。

**当时没做的核实（疏漏，记在这儿）**：没查下载量、star、提交历史、作者结构。
0.4.0 定在 2026-10-11，那时上游已存在 5 个多月——查一眼就能看出它是"两天内生成、
发完版没再动"的项目，而不是一个社区库。**下面 §6.2 的三个选项就是为补这个洞。**

### 6.1 现在真正存在的风险（把假想风险剔掉）

| 风险 | 判断 |
|---|---|
| 上游不更新 → 我们被卡住 | ❌ **不成立**：源码已在本机 `../egui_sauge`，path 依赖，构建不依赖上游可用性 |
| Rust 升级 → 74 条变硬错误 | ❌ **已消除**：本机 fork 已修，LLVM IR 与上游原样逐字节相同 |
| ★ **CI 单点** | ✅ **真实存在**：`release.yml` 每次发版都 `actions/checkout macnjil4/egui_sauge@v2.0.0`。上游仓库改名 / 删除 / 转私有，**发版流水线当场断** |
| ★ **没有第二个人** | ✅ **真实存在**：fork 只过了本机一个人的回归；上游不会 review、不会修 bug |

### 6.2 三个选项（~~还没定~~ → 已拍板 (b)，执行记录见 §6.3）

| 选项 | 做法 | 代价 / 收益 |
|---|---|---|
| (a) 保持现状 | 继续用 patch + 文档记档 | 零成本，但 CI 单点一直在 |
| **(b) 收进自己名下** ✅ **已选** | 把这份源码 **push 到自家 GitHub 仓库**，CI 改指过去；顺手给上游提 PR | 一次性成本；消掉 CI 单点，8684 行彻底归我们所有，以后升级 egui 也不用等上游 |
| (c) 弃用、退回 egui 原生 | 自己写主题与组件 | 要重写 8684 行里我们用到的那部分 + 重调全部界面配色，**视觉会变**；收益只是"不依赖小库"。不建议 |

### 6.3 执行记录（2026-10-12，已验收）：fork 已推上 GitHub，CI 已改指

用户拍板 2026-10-12：**走 (b)**。三个子决策：仓库形态 = GitHub fork；版本号 = 保持 `2.0.0`；
CI 引用方式 = **钉 commit SHA**。

#### 做了什么

| 项 | 结果 |
|---|---|
| fork | `https://github.com/xinmeiguo/egui_sauge`（**真 fork**，parent = `macnjil4/egui_sauge`，Apache-2.0 与 LICENSE 完整保留） |
| 提交 | `095ae21` "Add explicit `f32` suffixes to float literals (74 sites)"，**+74 / -74，35 文件** |
| 历史 | 完整保留上游 **12 个提交**（`2b2feb16` v2.0.0 在倒数第二位），不是单提交重建 |
| tag | 新增 `wrepl-base-2026-10-12` → `095ae21`（上游 `v1.0.0`~`v2.0.0` 一个没动） |
| 版本号 | 保持 `2.0.0` 不动 ⇒ **`Cargo.toml` 与 `Cargo.lock` 零改动**，无版本冲突 |
| 仓库元数据 | Description + topics（`egui` / `design-system` / `rust` / `fork` / `wrepl`）已设 |
| CI | `release.yml` 的 `repository` → `xinmeiguo/egui_sauge`，`ref` → `095ae21e6e2357d4d392b1b33de98a7032af9212`（**只有这两个字段变**，junction 逻辑与 `Cargo.toml` 一字未动） |
| 给上游的 PR | ✅ **https://github.com/macnjil4/egui_sauge/pull/1** —— GitHub 显示 **+74/-74、35 文件**，与本地 diff 完全一致 |

#### 为什么钉 SHA 而不是 tag

tag 可以被 `git push --force` 移动。钉 SHA 意味着**任何人都无法在代码没变的情况下改变发版产物**。
代价是 fork 更新时要手动改 `release.yml` 那一行 —— 更新步骤见 `CONTRIBUTING.md` §1.2。

#### 验收（关键几条）

| 判据 | 结果 |
|---|---|
| **端到端复现 CI** | ✅ 从 GitHub **全新 clone** fork（checkout 到 `095ae21`）+ **全新 clone** wrepl，摆成同级目录 → `cargo check --features gui` **EXIT=0**，egui_sauge 警告 **0 条**（只剩第 2 步那 7 条自家弃用） |
| 远端 = 本机 | ✅ 两边 `HEAD^{tree}` **完全相同**（`fa8de614…`）⇒ 远端内容一字不差 |
| PR 差异干净 | ✅ 上游看到的 diff 是 **74 增 74 删**，不是整文件替换 |
| YAML 合法 | ✅ `yaml.safe_load` 解析通过，SHA 已用 API 验证存在 |

#### ★ 这次踩到的两个坑（下次改 fork 时必看）

1. **空仓库里直接 commit ⇒ PR 会变成全文重写**。当时在 `D:/test/egui_sauge` 里
   `git init` 后直接把上游全部文件 `add` + `commit`，`git diff --cached` 显示
   **63 files / 18283 insertions** —— 因为空仓库没有基线，每个文件都是「新增」。
   正解：先让本地仓库落在 `origin/main` 的历史上，用 plumbing 三连
   （`update-ref` + `symbolic-ref` + `read-tree`），只动 index 与 HEAD、不碰工作区。
   做完再 `git diff` 就是 **35 files / 74 增 74 删** 的真实差异。
   > ⚠ 我最初把这归因成 `core.autocrlf`，**是错的**，后经四组对照实验查清（见下）。

2. **`git push` 的写通道在本机常被代理封**（`CONNECT tunnel failed, 502`），而
   `git ls-remote` / `curl api.github.com` 的读通道正常 —— 两者表现不一致，别据此判断
   "仓库坏了"。**改用 REST API 建 tag**：
   `POST /repos/{o}/{r}/git/tags` → `POST /repos/{o}/{r}/git/refs`，绕开 git 写通道。

另外，PR 创建时 `head` 必须写**全限定名** `"xinmeiguo:main"`。只写 `"main"` 时 GitHub 会
按 fork 自身解析 head，等于「上游 main 减上游 main」，报
`No commits between main and main`。

#### ★★ `core.autocrlf` 的真实行为（与直觉相反，做过对照实验）

一开始以为「Windows 上 `autocrlf=true` 会把 LF 转成 CRLF，所以要关掉」——**这个推理是错的**。
实测四种组合（源文件 LF，走一次 add + checkout）：

| `.gitattributes` | `core.autocrlf` | 入库 blob 的 CR | 结论 |
|---|---|---|---|
| 无 | `true` | **0** | ✅ 规范化成 LF，PR 干净 |
| 无 | `false` | **89** | ❌ 不做转换，CRLF 直接入库 → PR 全文重写 |
| 有 | `true` | 0 | 同上 |
| 有 | `false` | 89 | 同上 |

⇒ **`autocrlf=true` 才是安全的那一方**（Windows 默认，也是本机全局值），**保持默认、别动它**。
⇒ `.gitattributes` 在这个场景下**不产生任何差异**（四种组合两两相同），
所以 fork 里**没有**加它 —— 加了也只是安慰剂，反而多一个与上游无关的偏离。


#### 现在的风险状态

| 风险 | 判断 |
|---|---|
| 上游停更 / 删库 / 转私有 → 发版断链 | ✅ **已消除**：CI 只认 `xinmeiguo/egui_sauge`，与上游可用性无关 |
| Rust 升级 → 74 条变硬错误 | ✅ **已消除**：本机和 CI 用同一份已修源码 |
| 上游发了新版 v2.1 我们要不要跟 | ⚠ **仍在**：这是主动决策问题，不是故障。真要跟时用 `docs/egui_sauge-fork.patch` 重放，或直接 rebase 后解决冲突 |
| 没有第二个人 review | ⚠ **仍在**：属团队规模问题，跟依赖无关 |

