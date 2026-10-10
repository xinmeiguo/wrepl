# wrepl 性能分析与优化

> 结论先说：**在不改动任何产物字节、不削弱任何验证关卡的前提下，
> 「处理一批文件」的净耗时降到原来的 1/1.5 ～ 1/2.0**（8.8 MB 单文件场景到 1/3.9）。
> 仓库自带的完整回归（26 个真实交付包）**61/61 全绿**，与历史基线产物 26/26 SHA256 逐字节相同。
>
> 另外盘了一遍**打包与系统依赖**（第 5 节），并处理了"别人下载后双击没反应"：
> 成因有两条是代码层面的（窗口建不出来时**完全静默**、主线程栈预留没钉住 ——
> 它同时决定"能不能开"和"开多快"），都已修掉；
> 第三条（SmartScreen / 管控套件）只能靠文档说清。
>
> 本文记录**怎么测的、测到了什么、改了什么、还剩什么**。
> 所有数字都是本机实测，测量方法写在文末，可原样复跑。

---

## 0. 一句话诊断

这个工具慢，**不是慢在算法上，是慢在"重复劳动"和"没必要的系统调用"上**：

- 同一份 docx 在一次执行里被**打开 9 次、解压 4 遍**（改造前）；
- 一个谁都不用的**整文件 SHA256**（8.8 MB ≈ 40 ms，本机 CPU 还没有 SHA 指令加速）每次都算两遍；
- **验证关卡对每个 part 都解压再算内容 SHA256** —— 包括 9 MB 的图片，
  而它想问的其实只是"这个 part 被改过没有"；
- 关卡 2 的骨架比对**每个 XML 元素分配一个 `String`**，几万个元素就是几万次堆分配；
- 规则折叠按「规则 × 段落」重复做，10 条规则就把同一段文字折 10 遍；
- 写完产物后**为了补 3 个字节的 zip 记账字段，重新打开刚落盘的文件**——
  而 Windows 上这一步经常被杀软实时防护拒掉，落到 15/30/60 ms 的退避重试上。

这些都跟"替换逻辑对不对"无关，纯粹是可以白拿掉的开销。

而"别人下载后双击没反应"是**另一类**问题：界面版跑在 Windows 图形子系统上，
**没有 stderr**，任何启动期失败都是静默的。见第 5 节。


---

## 1. 改造前的开销拆解

一次 `wrepl apply <目录> --out <目录> --verify-after --report`
对**每个文件**要做的事（`--out` 写副本路径）：

| # | 动作 | 打开文件 | 解压 | 整文件读 | SHA256 |
|---|---|---|---|---|---|
| 1 | `read_text_parts` 取文本 part | 1 | 文本 part | — | — |
| 2 | `file_sha(src)` | 1 | — | 全文件 | ✔ |
| 3 | `write_with_replacements` | 1 (src) + 1 (create dst) | 仅改过的 part 重压 | — | — |
| 4 | `restore_external_attrs` 补 zip 记账字段 | src 1 + dst 1 + dst 写 1 | — | — | — |
| 5 | `file_sha(dst)` | 1 | — | 全文件 | ✔ |
| 6 | `verify_one` = `snapshot(src)` + `snapshot(dst)` | 2 | **全包** | — | ✔✔ |
| 7 | `residue(dst)` 残留自检 | 1 | xml/rels + 重扫文本 part | — | — |
| | **合计** | **9 次** | **4 遍** | **2 遍** | **4 遍** |

而 `snapshot` 里两个关卡的实际开销并不均等——以一份 278 KB 的
双向交付文档（`word/document.xml` 解压后 1.23 MB）实测：

| 步骤 | 耗时 | 说明 |
|---|---|---|
| `package::inspect`（全包解压 + 逐 part SHA256） | 9.1 ms | 解压本身很便宜 |
| `verify::snapshot` | **35.0 ms** | 其中**骨架解析约 25 ms**——贵的不是解压，是每个 token 一次 `String` 分配 |
| `verify::verify_one` | **72.9 ms** | = 两次 snapshot |
| `verify::residue` | **29.7 ms** | 第三次开包 + 第三次解析 XML + 每条规则重折一遍全文 |
| `write_with_replacements` | 24.6 ms | 含"重新打开刚落盘文件"的退避重试 |

**本机 CPU 没有 SHA-NI。** `--version` 之外最"纯粹"的一次测量：

```
Sha256::digest(8.8 MB 已在内存里的缓冲)   33–37 ms   →  约 240 MB/s
CPU: Intel64 Family 6 Model 165 Stepping 5   （Comet Lake，无 SHA 扩展）
```

`sha2 0.11` 的 SHA-NI 运行时探测在别的机器上是 5–10 倍，**在本机一点用都没有**。
所以"整文件 SHA256 每个文件算两遍"在本机是实打实的 80 ms/文件。

---

## 2. 改了什么

### 2.1 整文件 SHA256 改成按需（`pipeline::Options::file_sha`）

`Outcome::sha_before` / `sha_after` 只有两个消费者：`--report` 的「文件清单」工作表、
以及 `--mirror` 用来自证"未改动文件是原样复制的"。CLI 不带 `--report` 时**没有任何人读它**，
却照样每个文件整读两遍、算两次 SHA256。

现在由调用方声明要不要：CLI 在 `--report` 或 `--mirror` 时自动打开
（`mirror` 那一支在 `pipeline::run` 里还会再强制打开一次，不指望调用方记得设）；
界面版**恒开**——它每次执行完都会写报告，报告的「文件清单」要填这两列。

> 这一项对 8.8 MB 的文件值 **80 ms**，对小文件几乎为零。

### 2.2 关卡 2 骨架：不再每个 token 分配一次 `String`

`Skeleton` 从 `Vec<String>` 改成「**一个字节缓冲 + 一张 `u32` 起点表**」。
`word/document.xml` 有几万个元素与文本事件，原先每个 token 一次 `format!`
外加一次 `Vec` 推入；现在推一个 token 只是一次 `extend_from_slice`。
元素栈也从 `Vec<String>` 改成 `Vec<bool>`（它只用来回答"父元素是不是 `w:r`"）。
**token 的内容与顺序一字未改**，`compare_skeleton` 的判据也一字未改。

`Event::Text` / `Event::CData` / `Event::GeneralRef` 同样不再无条件 `into_owned()`——
元素之间的空白各是一个文本事件，没有挂着的文本元素时一个字节都不必复制。

| | 改造前 | 改造后 | |
|---|---|---|---|
| `verify::snapshot` | 35.0 ms | **19.8 ms** | 1.77× |
| `verify::verify_one`（= 两次 snapshot） | 72.9 ms | **37.6 ms** | 1.94× |

### 2.3 段落扫描：去掉逐元素的 `String` 分配

`docx::scan::Ctx` 的元素栈同样从 `Vec<(String, bool)>` 改成 1 字节的 `ElemId` 枚举
（只保留 `Run` / `Txbx` / `Wml` / `Other` 四种判定用得上的身份）。
`decode_with_offsets` 原先返回 `Vec<(char, usize, usize)>`（每字符 24 字节的中间缓冲），
再由 `finalize_para` 逐项搬进 `visible` + `map`；现在**直接写进这两个容器**，中间缓冲整个消失。

顺带修掉一个**算法性**问题：`Para::node_covered_range` 原先每次都把**整段**的
字符→字节映射表走一遍（只是把区间外的项 `continue` 掉），实际只需要看 `map[a..b]` 那一段。

| | 改造前 | 改造后 | |
|---|---|---|---|
| `scan_part` 合计 | 14.2 ms | **12.2 ms** | 1.16× |
| `plan_part` 合计 | 17.4 ms | **13.1 ms** | 1.33× |

### 2.4 折叠只做一次（`engine::collect` / `verify::residue` / `count_in_package`）

匹配靠"把待搜文本与查找串各自折叠成同一个可比形式"。折叠结果**只取决于折叠设置**
（区分大小写 / 区分全半角），**与是哪条规则无关**。原先是「每条规则 × 每一段」各折一遍。

现在按折叠设置分组：同一组里的规则共用一份折叠后的段落文本。
**命中顺序必须与"逐规则扫"完全一致**（它是报告的一部分），所以分组后按
`(规则序号, 段号, 起始位置)` 排一次序，把顺序精确还原。

| | 改造前 | 改造后 | |
|---|---|---|---|
| `verify::residue`（278 KB 文档，4 条规则） | 29.7 ms | **18.4 ms** | 1.61× |
| `verify::residue`（8.8 MB 文档） | 21.6 ms | **10.1 ms** | 2.14× |

规则表越长，这一项的差距越明显——10 条规则时是 10 倍的白干。

### 2.5 写副本：补 zip 记账字段不再重开刚落盘的文件 ★

这是**单项收益最大**的一处，也是最不直观的一处。

zip 规范里每个条目有"version made by / 内部属性 / 外部属性"三个声明字段，
`zip` crate 写新条目时会无条件改掉它们。为了做到"文件级 SHA 与原件一致"，
`restore_external_attrs` 会在**写完之后**再把中央目录那几十字节改回去——
而它是通过**重新 `File::open(dst)` 再写**做到的。

本机（装了企业管控 / 实时防护）实测：`File::open` 一个**刚被自己创建**的文件并请求写权限，
会被拒；于是落到 `write_at` 的 `15 / 30 / 60 / 120 / 240 / 480 ms` 退避重试上。

```
改造前：write_with_replacements 连续三次 =  24.8 ms ／ 58.7 ms ／ 23.1 ms   （抖动 2.5 倍）
改造后：write_with_replacements 连续三次 =   6.7 ms ／  8.0 ms ／  7.3 ms   （抖动消失）
```

修法：产物以 **`read(true).write(true)`** 打开，写完之后**用同一个句柄**就地回填记账字段
（`restore_external_attrs_on`），连"再开一次文件"这个动作都不必发生。
**产物字节一个都没有变**——回归里 26/26 SHA256 与历史基线完全相同。

### 2.6 关卡 1 改成比**压缩字节**，不再解压每个 part ★

这是收益最大的第二处，也是唯一动了验证内核的一处。**判据没有变弱，反而更强了。**

原实现把**每个 part 都解压**、再对解压内容算 SHA256。对图片这种 part，
解压后往往比压缩后大一个数量级，本机又没有 SHA 指令加速：

```
8.8 MB 文件：snapshot 44 ms  ≈  解压 9.4 MB（约 7 ms） + SHA256 9.4 MB（约 37 ms）
```

而关卡 1 想问的其实只是"这个 part 有没有被改动"。现在分两档判：

| part 类型 | 判据 | 为什么 |
|---|---|---|
| **文本容器**（`document.xml` / 页眉页脚 / 脚注 / 批注） | 解压内容 SHA256 | 关卡 2 本来就要解压它们做骨架 —— 这个指纹是**白得的** |
| **其余 part**（styles / settings / media / rels …） | **压缩字节逐字节比** | 同一条 deflate 流必然解出同一段内容，所以"压缩字节完全相同"蕴含"内容完全相同"，而且它比 SHA256 **更强**（连"重新压缩过但内容没变"都排除了）。关键在**不必解压** |
| 压缩字节对不上 / 压缩方式不同 / 取不到数据起点 | **退回解压比内容 SHA256** | 与改造前一模一样的判据，只是落到少数派分支上 |

**就地替换是这一改动的唯一难点，也是它差点做不成的原因。**
就地替换下"原件"在改写那一刻就没了：如果 `before` 快照只记"压缩字节在文件里的位置"，
事后再去读那个区间，读到的是**改写后的文件** —— 等于拿文件跟它自己比，
关卡 1 必然全过。那不是"验证通过"，是**根本没有验证**。

所以 `PkgSnap` 现在带一个 `RawSource`：

- `RawSource::File(路径)` —— 写副本模式与 `wrepl verify` 命令（原件一直在盘上）；
- `RawSource::Mem(Arc<Vec<u8>>)` —— **就地替换**：`pipeline::process_one` 在改写前
  把原件整份读进内存（这份字节本来就是 `rewrite_in_place` 要用的，**没多读一次盘**），
  据此采快照，再把同一份字节交给新的 `rewrite_in_place_with`。

逐字节比对是**分块流式**做的（128 KB 一块，缓冲在 part 之间复用），
不把整份文件读进内存；`Mem` 一侧直接切片，`File` 一侧只开一个句柄。

新增 5 个单元测试钉住这条底线，最关键的一个是
`in_memory_before_snapshot_still_detects_a_changed_media_part`：
左侧取自"改写前"的内存字节、磁盘上已是改写后的内容，媒体 part 被换掉 ——
**关卡 1 必须报出来**。把 `RawSource::Mem` 去掉，这个测试立刻变红。

| | 改造前 | 改造后 | |
|---|---|---|---|
| `snapshot`（278 KB 文本型） | 19.8 ms | **15.9 ms** | 1.24× |
| `snapshot`（8.8 MB 媒体型） | 44.3 ms | **4.0 ms** | **11.1×** |
| `verify_one`（8.8 MB） | 90.2 ms | **15.0 ms** | **6.0×** |

### 2.7 其它

- **`PartPlan::new_bytes` 改成 `Option<Vec<u8>>`**：没命中的 part 原先要
  `xml.to_vec()` 拷一份（1.23 MB）再整段比一遍，只为得出"没变"。现在没有编辑就没有新的字节流。
- **`canonicalize` 每个文件只算一次**：收集目标时「是否落在输出目录内」与"同一个文件被给了两次"
  两道判断各调一次 `Path::canonicalize`（本机 0.27 ms/次的真实系统调用）。
- **`[profile.release]`**：原先**完全没有这一段**，等于 `codegen-units = 16`、无 LTO。
  现在 `lto = "fat"` + `codegen-units = 1`。热点全在依赖里（quick-xml 的解析循环、
  flate2 的 inflate、sha2 的压缩函数、zip 的搬运），跨 crate 内联是唯一能把它们缝进主流程的手段。
  **刻意不开 `panic = "abort"`、也不开 `strip`**——界面版靠 panic 钩子把出错位置与调用栈
  写进 `%LOCALAPPDATA%\wrepl\wrepl-gui.log`，`strip` 会删掉符号表、`panic = "abort"`
  会让线程里的 panic 直接带走整个进程。这两项省下的体积换不来"崩溃了查不出原因"。
- **主线程栈 1 MB → 16 MB**：见第 5 节，那是"双击没反应"那条线的改动。

---

## 3. 实测结果

进程启动本底（`wrepl --version`，含 clap 解析与进程创建）**63 ms**。
下表两侧**都含**这 63 ms，所以"加速比"列被稀释了；最后一列是扣掉本底后的**净耗时**比。

### 全语料 26 个真实交付包（11.2 MB）

| 场景 | 基线 | 优化后 | 加速 | 净耗时比 |
|---|---|---|---|---|
| `scan`（只扫不落盘） | 172 ms | **137 ms** | 1.25× | **1.49×** |
| `apply --dry-run` | 152 ms | **120 ms** | 1.27× | **1.58×** |
| `apply --out` | 284 ms | **211 ms** | 1.35× | **1.51×** |
| `apply --out --verify-after` | 569 ms | **312 ms** | 1.82× | **2.04×** |
| `apply --out --verify --report` | 600 ms | **396 ms** | 1.52× | **1.62×** |
| `apply --out --mirror --verify --report` | 598 ms | **368 ms** | 1.62× | **1.75×** |

### 单个 8.8 MB 文件（含大图，媒体型）

| 场景 | 基线 | 优化后 | 加速 | 净耗时比 |
|---|---|---|---|---|
| `scan` | 118 ms | **72 ms** | 1.63× | **7.57×** |
| `apply --out` | 173 ms | **97 ms** | 1.78× | **3.38×** |
| `apply --out --verify-after` | 299 ms | **124 ms** | 2.40× | **3.90×** |
| `apply --out --verify --report` | 303 ms | **201 ms** | 1.51× | **1.75×** |

> 这份文件 9.1 MB 是图片。它在**每一列**都快了 1.75～7.6 倍 —— 包括
> 上一轮唯一没怎么动的 `--verify --report`（那时只有 1.06×）：
> 2.2 节之后 `snapshot` 不再解压图片，报告那一列剩下的就只有它自己必须算的
> 整文件 SHA256。

### 25 个小文件（~2.7 MB，文本型，最贴近真实交付包）

| 场景 | 基线 | 优化后 | 加速 | 净耗时比 |
|---|---|---|---|---|
| `apply --out --verify-after` | 529 ms | **315 ms** | 1.68× | **1.85×** |
| `apply --out --verify --report` | 534 ms | **334 ms** | 1.60× | **1.74×** |

### 单步剖析（278 KB 文档，各取 5 次最小值）

| 步骤 | 基线 | 优化后 | 加速 |
|---|---|---|---|
| `package::read_text_parts` | 2.1 ms | 2.2 ms | — |
| `scan_part` 合计 | 14.2 ms | 12.2 ms | 1.16× |
| `plan_part` 合计 | 17.4 ms | 13.1 ms | 1.33× |
| `verify::snapshot` | 35.0 ms | **15.9 ms** | 2.20× |
| `verify::verify_one` | 72.9 ms | **34.8 ms** | 2.09× |
| `verify::residue` | 29.7 ms | **16.5 ms** | 1.80× |
| `write_with_replacements` | 24.6 ms | **6.7 ms** | **3.67×** |
| `rewrite_in_place` | 17.7 ms | 20.1 ms | 0.88×（噪声，见下） |

### 单步剖析（8.8 MB 媒体型文档，各取 3 次最小值）

| 步骤 | 基线 | 优化后 | 加速 |
|---|---|---|---|
| `verify::snapshot` | 44.3 ms | **4.0 ms** | **11.1×** |
| `verify::verify_one` | 90.2 ms | **15.0 ms** | **6.0×** |
| `verify::residue` | 10.1 ms | **8.1 ms** | 1.25× |
| `write_with_replacements` | 29.4 ms | **24.8 ms** | 1.19× |
| `pipeline::file_sha`（报告才要，每个文件两遍） | 39.1 ms | 39.8 ms | —（按需才算，见 2.1） |

> `rewrite_in_place` 那一格是**测量噪声**，不是回归：它内部同样走一次 `write_at`
> 重开写句柄，会随机撞上退避重试。CLI 层面的就地替换实测没有变慢。
> 顺带说明：**`write_at` 的退避档位（15→480 ms）偏保守**，落空一次就吃掉几十毫秒；
> 想再稳一点可以改成 2/4/8/16/32/64 ms（累计 126 ms，仍覆盖"独占通常几十毫秒"这一前提）。

---

## 4. 还没做的（以及为什么）

### 4.1 `residue` 是同一份包的**第三遍**解压 + 第二遍 XML 解析

`residue` 现在自己开一次包、解压 `.xml`/`.rels`、对文本 part 再跑一遍 `scan_part`，
只为拿到"可见文本"。而 `snapshot(dst)` 刚刚已经解压过同一批 part（只是它要的是骨架、
不是可见文本）。让两者共用一次解压是可行的，但 `scan_part`（可见文本 + 字符映射）
与 `skeleton`（骨架 token）是两套状态机，合并或共享缓存都得动接口。

估计收益：278 KB 文档约 4 ms/文件，8.8 MB 文档约 3 ms/文件（现在它已经是小头了）。

### 4.2 界面版报告强制整文件 SHA256

界面版每次执行完都会写报告，报告的「文件清单」要填处理前后的整文件 SHA256，
所以 `file_sha` 在界面里恒开。上面「26 个包 `--verify-after` 2.04× 对
`--verify+report` 1.62×」的差距（247 → 331 ms）就是它 —— 26 个文件、11.2 MB，
整读两遍再算两次 SHA256。

想要更快的界面路径，可以给"要 SHA 的报告列"一个复选框（默认勾上），
或者把这两列标成"可选"。**这是产品决定，不该由性能顺手改掉。**

### 4.3 界面启动：读 18.8 MB 的 `msyh.ttc`

`install_fonts` 会 `std::fs::read("C:/Windows/Fonts/msyh.ttc")`——本机这个文件
**18.8 MB**，且是 `.ttc` 字体集合（`ab_glyph` 只会用索引 0 那一张字面）。
读盘 + `FontData::from_owned` + egui 建字体图集都压在第一帧上。

可选做法：把解析结果缓存到 `%LOCALAPPDATA%\wrepl\`；或改用体积小得多的
`Deng.ttf` / `simhei.ttf`（只有黑体一族，缺字时回落系统默认）。
**这一项我没有动** —— 它直接改变界面字体观感，属于产品决定。

### 4.4 换成 wgpu(DX12) 渲染后端，救"没有 OpenGL 2.1"的机器 —— 实测不可行

界面版用 `glow`（OpenGL）后端。显卡驱动只给 OpenGL 1.1 时（远程桌面、虚拟机、
只装了「Microsoft 基本显示适配器」）窗口建不出来。`wgpu` 后端走 DX12，
没有硬件适配器时能落到 WARP（软件光栅化），理论上**在这些机器上反而能跑起来**。

**实测结论：这条路在本项目的约束下走不通。**

- `eframe` 的 `wgpu` feature 需要 `pollster` 等**本地 registry 里没有**的 crate ——
  在 `--offline` 下直接报 `failed to download pollster v0.3.0`。
  本项目「构建一律加 `--offline`」是硬约束（见 `bench/README.md` 与
  `regress.sh` 文末的本机环境备忘），加它等于破坏这条约束。
- 依赖树会大一圈（wgpu + naga + wgpu-core/hal + d3d12 …），
  与 `Cargo.toml` 里"glow 而非 wgpu —— 依赖树更小、离线可编"的取舍直接冲突。
- **本机验证不了**：这台机器有 OpenGL，回退分支永远走不到。

所以这一轮改成**把失败讲清楚**（见 5.2）：窗口建不出来时弹一个说人话的对话框，
写明原因、日志路径，并指向命令行版 —— 命令行版做的是同一件事，且完全不需要显卡。
要不要为少数机器付出上面的代价，属于产品取舍，留给维护者决定。

### 4.5 顺带确认过、**不需要**动的

- **deflate 后端已经是快的那个**。`zip 8.6` 的 `deflate` feature 本身就映射到
  `deflate-flate2-zlib-rs` → `flate2/zlib-rs`（zlib-ng 的纯 Rust 移植），
  而 flate2 的后端优先级里 `zlib-rs` 高于 `miniz_oxide`。**不必再换。**
- **`sha2` 已经开了 SHA-NI 运行时探测**，只是本机 CPU（Comet Lake）没有这个扩展。
  换机器就是 5～10 倍，代码不用改。
- **并行策略没问题**：原子取号的动态负载均衡对"100 KB 与 5 MB 混在一起"的交付包
  正是对的；`parallel_map` 结果按输入顺序归位，报告行序稳定。
- **`write_at` 的退避档位（15→480 ms）偏保守**，落空一次就吃掉几十毫秒。
  想再稳一点可以改成 2/4/8/16/32/64 ms（累计 126 ms，仍覆盖"独占通常几十毫秒"这一前提）。
  本次没动它 —— 换档位只影响延迟分布，不影响正确性，且现在的值有实测依据。

---

## 5. 打包与系统依赖盘点（「下载后双击没反应」）

这一节回答"发出去的包到底依赖什么"以及"为什么有人打不开"。

### 5.1 产物到底依赖什么 —— 实测清单

`wrepl-gui.exe`（本机 GNU 构建）导入 **24 个 DLL**，`wrepl.exe` 13 个。
逐个核对过来源，**全部是 Windows 自带**：

| 分类 | DLL | 说明 |
|---|---|---|
| 内核 / 基础 | `KERNEL32` `kernel32` `ntdll` `advapi32` `bcryptprimitives` | 进程、注册表、随机数 |
| 窗口 / 图形 | `user32` `gdi32` `dwmapi` `imm32` `uxtheme` `opengl32` | 只有界面版要 |
| Shell / COM | `ole32` `shell32` `shlwapi` `mpr` | `mpr` 用于映射网盘路径 |
| CRT | `api-ms-win-crt-*`（10 个） | UCRT，Windows 10 起系统自带 |
| 其它 | `api-ms-win-core-synch-l1-2-0` | OS API set |

**没有** `VCRUNTIME140.dll` / `MSVCP140.dll`（CI 用 `+crt-static` 静态链接），
**也没有** `libgcc_s_seh-1.dll` / `libwinpthread-1.dll`（本机这条 GNU 工具链
已经把 libgcc/pthread 静态链进去了 —— 与 `rust-toolchain.toml` 里那句
"本机 GNU 工具链是另一套依赖"**不一致，实测不成立**，顺带纠正）。

另外核对了 PE 头，发现三件**之前没人管**的事：

| 项 | 改造前 | 本次结论 | 影响 |
|---|---|---|---|
| 主线程栈预留 | 2.0 MB（GNU 默认）/ 1.0 MB（MSVC 默认） | **显式钉死 1.0 MB** | 见下 5.3：调大栈**既打不开、又慢**；而"不碰"会随工具链给 1 或 2 MB |
| 子系统 | `wrepl.exe` = CONSOLE(3)、`wrepl-gui.exe` = GUI(2) | 同左（**加进 CI 断言**） | 错了就会双击弹黑框 / 一闪而过 |
| 资源目录 | **完全没有** —— 无 manifest、无版本信息、无图标 | 未改（见 5.4） | 文件属性里一片空白，杀软/SmartScreen 眼里就是个匿名二进制 |

### 5.2 为什么有人"双击没反应" —— 三条真实成因

图形界面版是 **GUI 子系统**（PE Subsystem=2），**没有 stderr**。任何启动期失败
都是静默的 —— 这就是"双击没反应"的**结构性原因**。

**① 窗口建不出来（最可能）。** `eframe` 用 `glow` 后端，需要显卡驱动提供
**OpenGL 2.1 以上**。`opengl32.dll` 虽然是系统自带，但它只是个"转发层"，
真正的能力来自驱动。下面这些机器上只给 OpenGL 1.1：

- 远程桌面（RDP）会话；
- 虚拟机 / 云主机没开 3D 加速；
- 只装了「Microsoft 基本显示适配器」而没装显卡厂商驱动。

这时 `eframe::run_native` **返回 `Err`**（`Error::NoGlutinConfigs` /
`Error::Glutin` / `Error::OpenGL`），`main` 返回错误，进程安静退出 ——
**改造前连一句提示都没有**。

**② 主线程栈踩穿 —— 这一条最后**被证伪了**，见 5.3 的「栈的教训」。**
当时的顾虑是：Windows 给主线程的栈默认只预留 1 MB，界面版要在上面跑 winit 事件循环
+ eframe 整棵布局 + 显卡驱动的着色器编译，怕踩穿。**实测恰恰相反**：栈越大越糟，
大到 8 MB 就直接建不出窗口；而且**启动耗时随栈预留单调增长**（1 MB 2.2 s、
2 MB 5.5 s、4 MB 19.9 s）。真正要防的不是"栈太小"，而是"栈不是 1 MB"。
顺带纠正 README 里那句"`thread 'main' has overflowed its stack` 打印后进程继续
正常运行" —— Rust 的栈溢出处理是打印后 `abort()`，不可能"继续正常运行"；那句已删。

**③ 进程压根没被启动**（代码管不了，只能靠文档）：SmartScreen 拦未签名 exe、
文件带"来自 Internet"标记（MOTW）、企业管控/杀软静默拉黑、直接在压缩包里双击。

### 5.3 改了什么

| 改动 | 位置 | 解决哪一条 |
|---|---|---|
| 窗口建不出来时**弹对话框**说清原因 + 日志路径 + 指向命令行版；同时写日志、`eprintln!` | `gui/main.rs` + `diag::startup_failure_box` | ① ③ —— 把"没反应"变成"有话说" |
| CI 里**盘点依赖**：导入表 / 架构 / 子系统 / 栈预留（**必须恰好 1 MB**，见下），白名单外拒绝发布 | `check_pe_imports.py` | 全部（防退化） |
| CI 里**真跑一次**产物（`wrepl --version` + `wrepl-gui --selftest`） | `release.yml` | 全部（"编译过了但起不来"） |
| README 增「双击没反应？先看这一条」+ 系统要求 + 依赖清单 | `README.md` | ③ |
| ★ 本机新增 `bench/win_probe.py`：**枚举进程的可见顶层窗口**判定"窗口到底出没出来" | `bench/` | 全程（唯一能自动判定这一点的手段） |

#### 5.3.1 栈的教训：**"把主线程栈调大"在这里是反效果**

本次曾经按 ② 的推理把 `SizeOfStackReserve` 从 1 MB 提到 16 MB（一个 `build.rs`
加两条 `cargo:rustc-link-arg-bins`）。**结果界面版彻底打不开** —— 正是它想修的
那个症状：

```
!!! 建窗失败 !!!
  Found no glutin configs matching the template: ConfigTemplate { … } Error: not found
  NoGlutinConfigs(ConfigTemplate { … }, Error { …, kind: NotFound })
```

进程不立刻退出：winit 的辅助窗口（`Winit Thread Event Target`）先建出来，
主窗口始终没有，干等约 **38 s** 后以退出码 **1** 退出。

**单变量实验**（同一份二进制，只改 PE 头里那 2 个字节的栈字段）：

| 产物 | 栈 | 主窗口 |
|---|---|---|
| GNU release（本机 6.7 MB） | 16 MB | **没有**（38 s 后退出码 1） |
| 同一份，只把栈改回 | 1 MB | **4.0 s 出现**（`wrepl —— Word 批量替换` 1196×859） |
| CI 的 MSVC 产物（7.2 MB） | 1 MB | **2.1 s 出现**（用户手上这份是好的） |
| 同一份，只把栈改成 | 16 MB | **没有**（40 s 内始终无窗口） |

⇒ **与编译器无关、与 LTO 无关，就是栈字段本身。**
（`DllCharacteristics` 由 `0x8160` 变 `0x0160` 是 GNU/MSVC 的既有差异 ——
0.2.2 时期的 GNU 产物同样是 `0x0160` —— 不是原因。）

##### 后半段：同一个字段还决定**启动快慢**

后来用户报"程序打开变慢"，用同一套单变量手法把曲线量细，才发现 8 MB 只是这条
**连续代价曲线**的悬崖端点：

| 栈预留 | 1.00 MB | 1.25 MB | 1.50 MB | 2.00 MB | 3 MB | 4 MB | ≥ 8 MB |
|---|---|---|---|---|---|---|---|
| 进程起 → 主窗口 | **2.2 s** | 3.1 s | 3.5 s | 5.5 s | 10.9 s | 19.9 s | **建不出来** |

两个方向都复现：MSVC 产物（1 MB，2.1 s）→ 2 MB 掉到 **5.5 s**；
GNU 产物（2 MB，5.5 s）→ 1 MB 回到 **2.2 s**。

代价**全在建窗那一段**（glutin 枚举 GL 配置）：bootstrap（进 `main` → 准备建窗）
只 **0.05~0.10 s**，两条曲线在这里一模一样。`SizeOfStackCommit` 恒为 4 KB，
所以不是"保留了多少内存"的代价，就是预留区大小本身。**具体机制未查明**
（要 ETW/剖析器），但规律被实验钉死，两个工具链一致。

**这才是"上一版变慢"的真正原因**：`mingw` 默认给 **2 MB**、`msvc` 默认给 **1 MB**
—— 用户拿到的一直是 CI 的 MSVC 产物（1 MB，2.1 s），而本地自编的是 GNU 产物
（2 MB，5.5 s），**同一版本号、一快一慢两份**。

**处置**：`build.rs` **显式把栈钉死 1 MB**（`/STACK:1048576` /
`-Wl,--stack,1048576`），CI 断言从"栈 ≤ 4 MB"改成"栈**恰好** 1 MB"。
上一轮曾把断言写成"栈必须 ≥ 8 MB" —— 方向是反的，会**拒绝修复、放行坏产物**。

**注意这里有个容易接着搞错的地方**：结论**不是**"别碰栈参数、用编译器默认值"
（那是上一轮的结论，已作废）—— 因为两个工具链的默认值不同（2 MB vs 1 MB），
"不碰"就必然给出"同版本一快一慢"。**必须显式钉死**。

**教训（可复用）**：① 看到"听起来完全合理的编译期加固"时，先用**单变量实验**
（只改一个字段、其余字节不动）证明它，别靠推理；② 判定 GUI"能不能开"要看
**窗口是否真的存在**，不能看进程活没活 —— **建不出窗口时进程同样是活的**；
③ 一个"能不能用"的开关（栈大小）往往同时也是**性能参数** —— 只验"能/不能"
会把中间的连续代价整段漏掉。

> **为什么不用"另起一个大栈线程"那个常见绕法**：Windows 上 winit 的事件循环
> **必须在主线程建**（`winit/src/platform_impl/windows/event_loop.rs:189`
> 有一处 `thread_id != main_thread_id()` 的 panic），要搬线程得多开一个
> `EventLoopBuilderExtWindows::any_thread` —— 而 winit 文档明确说那条路
> **不推荐**跨平台使用。
>
> 改 PE 头（`/STACK`）是 Windows GUI 程序的标准做法，**运行时行为一个字节都不改**；
> 本次的问题是**改的方向搞反了**（往大改），不是"不该改"。

### 5.4 没做、建议维护者定夺的

- **没有版本信息 / 图标 / manifest**。产物资源目录是空的：文件属性里看不到
  版本与公司名，资源管理器里是默认空白图标。对"未签名 + 无版本信息的匿名 exe"，
  SmartScreen 与企业管理软件的处置**明显更严**。补上需要一个 `.rc` +
  资源编译器（`winres` 在本地 registry 里有，`windres` 本机也有），
  但**图标是设计产物**，得由维护者给 —— 所以这一轮没动。
- **不做数字签名**。这是 SmartScreen 警告的根治办法，但需要代码签名证书
  （要花钱），不是代码层面能解决的。
- **不加 wgpu 后端**：见 4.4，实测 `--offline` 下装不上。

### 5.5 发版前自查

```powershell
# 依赖 / 架构 / 子系统 / 主线程栈 —— 一次看完
python .github/scripts\check_pe_imports.py target\release\wrepl.exe target\release\wrepl-gui.exe

# 真跑一次（命令行版 + 界面版无窗口自检）
.\target\release\wrepl.exe --version
New-Item -ItemType Directory -Force -Path smoke\in, smoke\out | Out-Null
.\target\release\wrepl-gui.exe --selftest smoke\in smoke\out

# ★ 界面版**必须**再加这一步：看窗口到底有没有真的建出来
python bench\win_probe.py target\release\wrepl-gui.exe 40
```

> 注意 `wrepl-gui.exe` 是图形子系统进程：用 `> 文件` 直接重定向时 PowerShell
> **不会等它**，退出码取不到；走管道（`| Out-String`）才会等。
> 记在 `release.yml` 的注释里了。

★ 最后那一步不能省：`--selftest` 按设计**不开窗**，静态检查也只看 PE 字段，
两者都**验不了"窗口能不能建出来"**（那依赖显卡驱动，CI runner 上还没有 GPU）。
而"建不出窗口时进程同样是活的"，所以光看"进程没崩"也没用 ——
必须问 Windows 那个进程有没有**可见的顶层窗口**，这正是 `bench/win_probe.py` 干的事。

---

## 6. 怎么验证"一点都没改坏"

四道独立的关，全部通过：

### ① 逐字节对照（本次新增的回归工具）

`bench/compare.ps1` 把**改动前的原始 exe**（`bench/baseline/wrepl-baseline.exe`，
从 `git archive HEAD` 单独编出来）与新版跑同一套参数，逐文件比 SHA256：

```
产物逐字节：写副本 / 完整镜像+改名 / 链式 / 最长匹配优先 / 单线程 /
            预演 / 4 种就地替换 —— 全部 OK（含 .xlsx 报告逐单元格内容）
stdout 一致性：scan / dump / inspect / probe / rules dump / rules check /
            apply --dry-run / verify --batch / verify 成对 —— 全部 OK
```

报告 `.xlsx` 里有「报告生成时间」和 `wrepl v<版本>`，天然每次不同，所以单独用
`bench/xlsx_diff.py` 逐 sheet 逐单元格比对（这两处抹掉）。

### ② 仓库自带的完整回归


```
bash regress.sh（26 个真实交付包，含 26/26 SHA256 硬指标、选项矩阵、GUI 自检）
→  汇总：通过 61 ／ 失败 0
```

其中与本改动最相关的几项：

- 单元测试 **34/34**
- 零改动透传：**SHA 一字不差**
- 端到端 26 个真实包 + 改名后批量验证 **26/26 通过**（同名 5 + 结构指纹 21）
- **与历史基线产物 26/26 SHA256 完全相同**（内核零漂移）
- GUI 产物与 CLI 产物 **26/26 SHA256 完全相同**
- 并行 1 / 8 / 自动：**26/26 逐字节相同**
- 就地替换与写副本的产出**逐字节相同**

### ③ 单元测试

`cargo test --release` → **34 passed / 0 failed**。
其中 5 个是本次为"关卡 1 比压缩字节"新加的，见 2.6。

### ④ CI 里新增的两道防线

发版流程现在会先**盘点依赖**（导入表 / 架构 / 子系统 / 主线程栈预留，白名单外拒绝发布），
再**真跑一次产物**（`wrepl.exe --version` + `wrepl-gui.exe --selftest`）。
这两步挡的都是"开发机上永远看不出来、用户机器上一试就废"的问题，见第 5 节。

> 一个踩到的坑记在这里：新加的那 5 个单元测试一开始用 `std::env::temp_dir()` 落临时文件，
> 在本机**沙箱里过、脱离沙箱跑回归时挂**（`os error 5`，C 盘禁止非系统进程写入）。
> 现在改成写进 `target/<profile>/` 下 —— 测试二进制自己就住在那儿，一定可写，
> 测试结果也就不再随"怎么跑的"漂移。

---

## 7. 测量方法（可复跑）

```powershell
# 单步剖析（不需要起进程，直接调库）
cargo build --release --offline --example profile
.\target\release\examples\profile.exe <某个.docx> bench\rules.txt

# 逐字节对照：必须先有「改动前的 exe」
#   1) 把 HEAD 的源码摊到 bench\baseline-src（仓库外亦可）
#      git archive HEAD | tar -x -C bench\baseline-src
#   2) 在那里 cargo build --release，把 wrepl.exe 放到 bench\baseline\wrepl-baseline.exe
pwsh -NoProfile -File bench\compare.ps1

# 端到端 A/B（交错跑、各取 6 次最小值、并扣掉进程启动本底）
pwsh -NoProfile -File bench\final.ps1
```

> `bench/` 里的脚本都依赖 `E:\test\docx` 这份真实语料（26 个交付包）。
> `bench\small`（<1 MB 的那 25 个）会按需自动生成。
> `bench\baseline\wrepl-baseline.exe` 是**改动前**的二进制快照，
> 6.3 MB，删了 `compare.ps1` 就跑不了，需要按上面第 1、2 步重新编一份。

几点测量上的注意，避免误读：

- **`--version` 要 63–93 ms**，其中真正属于程序的只有亚毫秒级（clap 解析 + 初始化）。
  其余全是 `Process.Start` 的开销（本机装了企业管控，进程创建本身就要记账）。
  所以"绝对耗时"里有一大块与程序无关，看**净耗时比**更接近真实观感。
- **单文件剖析要用「多次取最小」**。本机 `write_at` 会随机撞上杀软独占 →
  退避重试，同一段代码能测出 23 ms 和 59 ms 两个数。取最小才看得到代码本身。
- 剖析里 `rewrite_in_place` 那一格的 17.7 → 20.1 ms 与 CLI 层面的
  313 → 301 ms 相互矛盾，原因就是前者是**单次**测量撞上了重试。
