# wrepl

> Word 批量替换工具（.docx）——**外科手术式改 XML，不重建文档**。
> 只替换文字，段落样式、编号、表格、页眉页脚、修订标记一个字都不动。
>
> A batch find-and-replace tool for `.docx` that edits the OOXML inside the
> package instead of regenerating the document, so all original formatting —
> styles, numbering, tables, headers, revision marks — survives untouched.

不依赖 Word / Office / LibreOffice，纯 Rust，命令行与图形界面共用同一套内核。

---

## 它解决什么问题

手上有一批交付文档（FAT/IQ/OQ/PQ/SAT/MCL/URS…），要把里面的**项目编号**和
**客户名称**统一换成新的，比如：

```
2026-001CE  →  2026-002CE
某某制药有限公司  →  某某生物
Xxx Pharmaceutical Co., Ltd.  →  Xxx Biological
```

用 Word 的"查找替换"逐份打开改、或者用脚本把 docx 拆成 XML 改完再拼回去，
都会碰到同一类麻烦：

- **格式会被动**：换掉整段 `w:r`，字体的加粗、页眉的域、表格里的编号会漂；
- **改动面不可控**：正则一搜一替，容易连带改掉不该碰的地方；
- **改完不知道有没有改坏**：没有可复算的凭据，只能靠肉眼看。

`wrepl` 的做法是把 docx 当 zip 打开，**只在文本承载元素的字符数据上做区间替换**，
其余 part 的字节原样搬过去；改完立刻用两关 + 残留自检来证明"格式确实没被动过"。

---

## 核心保证

| 保证 | 手段 |
|---|---|
| 未改动的 part 字节完全不变 | 逐 part SHA256 比对 |
| XML 骨架（`pPr`/`rPr`/`styles`/表格/分节符）逐 token 相同 | 归一成骨架后比对 |
| `<w:delText>`（修订删除）、`<w:instrText>`（域代码）不被改动 | 数量必须相等，变了即判不合格 |
| 同一批文件无论用几线程、走 GUI 还是 CLI，产物逐字节相同 | 回归里钉死（26/26 SHA256） |
| 规则命中区间重叠时不猜 | 默认两条都不落笔并如实报冲突 |

---

## 构建

```bash
# 只要命令行（不拉 GUI 依赖，编得快）
cargo build --release --offline

# 带图形界面（eframe + glow 后端）
cargo build --release --offline --features gui
```

产物：

```
target/release/wrepl.exe        # 命令行
target/release/wrepl-gui.exe    # 图形界面（需 --features gui）
```

**Windows 上如果没装 Visual Studio / MSVC Build Tools**，默认的 `msvc` 工具链找不到
真正的链接器，会退而调用 PATH 里的 `link.exe`（Git Bash 里那是 BusyBox 的 `link`，
语义是"建硬链接"），缺参数时它去读 stdin，**进程会永久挂起**。此时改用 GNU 工具链：

```bash
cargo +stable-x86_64-pc-windows-gnu build --release --offline --features gui
```

或者建个 `rust-toolchain.toml`：

```toml
[toolchain]
channel = "stable-x86_64-pc-windows-gnu"
```

> 本仓库**故意不收录** `rust-toolchain.toml` 与 `.cargo/config.toml`——它们是本机
> 环境产物（前者锁 Windows GNU 工具链，后者把 `target-dir` 指到别的盘），
> 锁进仓库会让 Linux/macOS 上 `rustup` 直接装不上。

---

## 用法

### 命令行

```bash
# 先空跑一遍看命中（不落盘）
wrepl apply ./交付包 --rule "2026-001CE=>2026-002CE" --dry-run

# 就地替换源文件（默认行为），顺带改文件名，并做执行后验证
wrepl apply ./交付包 --rules-file 规则.txt --rename-files --verify-after

# 源文件不动，产物写到 ./out
wrepl apply ./交付包 --rule "2026-001CE=>2026-002CE" --out ./out

# 就地替换但留一份 .docx.bak（默认不留）
wrepl apply ./交付包 --rule "2026-001CE=>2026-002CE" --backup
```

常用子命令：

| 子命令 | 用途 |
|---|---|
| `apply` | 执行替换并落盘 |
| `scan` | 只扫描不落盘，列出每条规则的命中条数 |
| `probe` | 侦察目录，自动列出候选的项目编号与客户名，并可生成建议规则文件 |
| `verify` | 关卡 1+2 比对；`--batch` 时给两个目录，改过名的按 XML 结构指纹配对 |
| `inspect` / `diff` / `dump` | 摸 part 指纹 / 比对 part 差异 / 导出"段落→run 切分"报告 |
| `rules` | `template` 生成规则模板、`dump` 导出、`check` 只校验 |

### 规则从哪来

三种来源，可混用：

```bash
--rule "查找=>替换"           # 逐条给，可重复
--rules-file 规则.txt          # 制表符分隔：查找 | 替换 | 作用域 | 选项 | 备注
--rules-book 规则.xlsx         # Excel 规则表
```

**Excel 规则表**有两点和一般工具不一样：

- **表名随意**，不必叫「规则」。按工作表顺序找**第一张读得出条款的表**：
  先找首行能认出列名（"查找内容 / 替换为"）的表，都没有才退回找第一张
  像两列规则表的表（并且要求至少有一行第 2 列有内容，免得把"填写说明"页读成规则）。
  整本都读不出时**明确报错**并逐张列出原因，不会静默当"零规则"跑完。
- **没有序号列**：第 1 列固定是「查找内容」，第 2 列固定是「替换为」。
  再往后的列全部可选，且**按标题名认、列序随意**：
  `区分大小写 / 全字匹配 / 使用通配符 / 区分全半角 / 作用域 / 启用 / 备注`。

### 落盘位置

| 场景 | 行为 |
|---|---|
| `apply` 不给 `--out` | **就地替换源文件**（原文件上直接覆盖） |
| 加 `--backup` | 就地替换时另留一份 `.docx.bak`（只首次生成，始终是最初那版） |
| 给了 `--out DIR` | 写副本，源文件不动（`--backup` 与 `--out` 互斥，明确拒绝） |
| `--dry-run` | 只走完整执行路径，不落盘 |

就地替换不建临时文件、不做 rename，是在原文件上直接改写。代价是失去原子性——
写失败时先把内存里的原件写回去再报错，保底是（勾了才有的）`.bak`。

### 规则打架了怎么办

默认模式是**所有规则基于原文同时扫描**，命中区间重叠时**两条都不落笔**并如实报冲突
（"不猜"是刻意的：哪条该赢属于业务判断，工具不替用户决定）。三种应对：

```bash
--chain           # 链式：前一条的输出作为后一条的输入（顺序明确，不再有重叠）
--longest-first   # 重叠时只让「查找内容更长」的那条落笔，被挤掉的如实记进报告
```

也可以先把规则表收拾干净：把「查找内容 == 替换为」的空转规则标成 `启用=否`
——空转规则不只是没作用，它还会把真正要跑的规则**顶成冲突**。

---

## 执行后验证

CLI 加 `--verify-after`、GUI 执行时**恒开**。逐文件做三件事，全部只读：

1. **残留自检** —— 拿「查找内容」回头扫整包（含 `docProps`、图表、`[Content_Types].xml`），
   看还有没有没替换干净的旧串；
2. **关卡 1** —— 除改动过的 part 外逐个比 SHA256；
3. **关卡 2** —— 把 XML 归一成"骨架"逐 token 比对，确保 `pPr`/`rPr`/`styles`/表格/分节符未被动过。

> **基准必须在覆盖源文件之前扣下来。** 就地替换下"磁盘上的原件"会被当场覆盖掉，
> 所以实现是：写盘**前**采一份基准（每 part SHA256 + 每文本 part 骨架 token），
> 写盘**后**立刻比完即弃。报告「验证结论」的**备注**列会写明
> `改动 part：word/footer2.xml` —— **冒号后面是空的就说明没真比过**。

---

## 回归

```bash
bash regress.sh                       # 默认用 debug 产物
bash regress.sh path/to/wrepl.exe path/to/wrepl-gui.exe
```

15 组 44 项，每一组都对照**外部可复算**的量（SHA256 / 命中数 / 退出码），
不依赖工具自己的说法；任一项失败即非零退出。

回归跑的是**真实交付包**，里面的项目编号与客户名属于商业信息，不适合随源码公开，
可脚本又必须靠这些字面量去 `cp` 文件、`grep` 期望值。所以：

- `regress.sh` 里的语料字面量**全是变量**，默认值是脱敏占位；
- 本机真实值放在同目录的 `regress-samples.local.sh`（`.gitignore` 已排除）；
- 换语料 / 换机器只改那个文件，`regress.sh` 一个字都不用动；
- 没那个文件也能跑 —— 语法没毛病，但会整片报红，这是设计如此。

要拿自己的语料跑，照 `regress-samples.example.sh` 改：

```bash
cp regress-samples.example.sh regress-samples.local.sh
# 把 CORPUS 指到你的 docx 目录，再把 P_OLD / P_NEW / C_OLD / C_NEW
# 换成语料里真实存在的串
```

另外 `cargo test --offline --lib` 有 26 项单元测试，不需要任何外部语料，随时可跑。

---

## 目录结构

```
src/
  main.rs         CLI 入口
  cli.rs          命令行参数定义（只有参数，没有逻辑）
  lib.rs          库门面
  engine.rs       搜索与落笔规划（区间重叠裁决也在这）
  pipeline.rs     目录遍历 / 并行 / 落盘 / 规则装载
  rules.rs        规则模型、规则文件与 Excel 规则表解析
  report.rs       运行日志与 .xlsx 替换报告
  verify.rs       执行后验证（关卡 1+2、残留自检、结构指纹配对）
  naming.rs       文件名同步改名（含重名消解）
  probe.rs        目录侦察：候选项目编号与客户名
  docx/
    package.rs    zip 容器读写、part 分类（就地覆盖改写在这）
    scan.rs       XML 解析：段落 → run 切分、文本承载元素
    rewrite.rs    XML 就地重写（只动字符数据区间）
  gui/            图形界面（eframe/egui，--features gui 才编）
```

---

## 已知限制

- **只支持 `.docx`**。`.doc`（老的二进制格式）、`wps` 一律**明确拒绝**，不做降级尝试。
- **不做像素级比对，也不调用 Office**。格式保全靠解析原始字节来证明，
  能力边界是"结构性未改动"，不是"渲染出来一模一样"。
- 使用**通配符**的规则会被**挡在表外并记日志**，绝不降级当普通文本跑。
- 残留自检的扫描范围**大于**可改范围：只替换 6 类文本 part 的 `<w:t>`，
  但会扫一切 `.xml`/`.rels`。所以当文字也出现在文档属性或图表里时，
  会出现"没改但报了残留"——这是设计如此，看报告备注里的
  `（可见文本 N）` 才是真漏改。
- GUI 启动时可能往 stderr 打一行 `thread 'main' has overflowed its stack`：
  **打印后进程继续正常运行、退出码 0**，是 eframe/OpenGL 建窗口路径上的既有现象，
  双击运行看不到 stderr，不影响任何功能。

---

## 许可

[MIT](LICENSE) © 2026 XINWEI
