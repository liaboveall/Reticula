/** Chinese texts of the front page. The glossary is in `zh-core.ts`. */
const zhLanding: Readonly<Record<string, string>> = {
  // Bars
  Sections: "章节",
  Overview: "概览",
  "Try it": "试用",
  Numbers: "数字",
  Renderers: "渲染器",
  Architecture: "架构",
  Checks: "验证",
  Limits: "限制",
  "Open the demo": "打开演示",
  "Open the simulator": "打开仿真器",
  "Open 10 million rows": "打开 1,000 万行",
  "View on GitHub": "在 GitHub 上查看",

  // Overview
  "One million rows in a browser tab, edited together, online or offline.":
    "百万行的表格，在浏览器标签页里打开，多人一起编辑，断网也能用。",
  "A local-first spreadsheet whose kernel (Rust in WebAssembly), grid (Canvas 2D and WebGPU) and sync protocol were written for it. Each number here says where it was measured.":
    "本地优先的电子表格，内核（Rust 编译为 WebAssembly）、网格（Canvas 2D 与 WebGPU）和同步协议都为它从头写成。每个数字都注明在哪里测得。",
  "No sign-in, and nothing is uploaded.": "无需登录，不上传任何内容。",
  "A formula being written. The ranges it refers to are outlined in their colors.":
    "正在输入的公式，它引用的区域以各自的颜色标出。",
  "The demo: 1,000,000 rows and 100,001 formulas from a seed. Its words are Chinese.":
    "演示：由种子生成的 1,000,000 行和 100,001 个公式。",

  // Try it
  "The end of the sample at 25% zoom.": "缩放到 25% 的示例末尾。",
  "A million rows": "百万行",
  "Change V1, and 100,001 formulas follow. Insert a row at the top, zoom out, scroll to the end. Go offline and reload.":
    "改动 V1，100,001 个公式随之更新。在最上面插入一行，缩小，滚动到末尾。断网后刷新。",
  "Two replicas that have diverged, side by side.": "并排的两个副本，尚未收敛。",
  "Sync by hand": "亲手同步",
  "Replicas and their server in one page. Hold a link back, edit on both sides, deliver the messages one by one. Or let five replicas run into faults from a seed.":
    "副本和服务器都在一个页面里。压住一条连接，两边同时编辑，逐条送达消息。也可以让五个副本按种子遭遇故障。",
  "A draft in the sheet, and the review of its changes.": "表中的草稿，以及对改动的审阅。",
  "Drafts to review": "待审阅的草稿",
  "The AI Analyst edits a branch, never the sheet. The review shows each change, each conflict, and references that would break. Ten recorded runs need no key: press AI in the editor.":
    "AI 分析师只改分支，不改主表。审阅列出每处改动、每个冲突和会失效的引用。十次录制的运行无需密钥：在编辑器里点 AI。",
  "Open the editor": "打开编辑器",
  "An earlier revision of a shared sheet.": "共享表格的一次较早的修订。",
  "Shared, with a history": "共享，并留有历史",
  "Sign in with GitHub and send a link. Offline edits merge later, and every revision can be opened again.":
    "用 GitHub 登录，发出链接。离线的编辑稍后合并，每次修订都能再打开。",
  "Share a workbook": "共享工作簿",
  "Performance dashboard": "性能看板",
  "Every archived run, every known difference from LibreOffice.":
    "每次存档的测量，与 LibreOffice 的每类差异。",
  "Open the dashboard": "打开看板",
  "Source code": "源代码",
  "Kernel, grid, server and tests in one repository.": "内核、网格、服务器和测试都在一个仓库里。",

  // Numbers
  "Medians unless noted, each with its commit and report. Everything in a browser ran on one fast laptop.":
    "未注明的是中位数，都附有提交和报告。浏览器里的数字在一台高性能笔记本上测得。",
  "not available": "暂无数据",
  "{s} s": "{s} 秒",
  "Commit {commit}": "提交 {commit}",
  "uncommitted changes": "含未提交的改动",
  headless: "无头模式",
  "machine not recorded": "未记录机器",
  "affinity {mask}": "亲和性 {mask}",
  report: "报告",
  "First visit to first cells": "首次访问到出现单元格",
  "{visits} visits, no cache, {mbps} Mbit/s, {rtt} ms round trip":
    "{visits} 次访问，无缓存，{mbps} Mbit/s，往返 {rtt} 毫秒",
  "Open 20 million cells and a million formulas": "打开 2000 万个单元格和 100 万个公式",
  "from local storage to the first page of cells": "从本地存储到第一页单元格",
  "Recalculate 100,000 formulas": "重算 10 万个公式",
  "after one edit; p95 {p95}": "一次编辑之后；p95 {p95}",
  "Insert a row above a million": "在百万行之上插入一行",
  "mean of 100": "100 次的均值",
  "Main thread per frame of scrolling": "滚动时主线程每帧用时",
  "p99 of {frames} frames, a million rows, Canvas 2D": "{frames} 帧的 p99，百万行，Canvas 2D",
  "Memory for 50 million cells": "5000 万个单元格占用的内存",
  "peak of WebAssembly; opened in {load}": "WebAssembly 峰值；打开用时 {load}",
  "Answers while 20 million cells are saved": "保存 2000 万个单元格期间的应答",
  "p95; the save takes {save}": "p95；保存用时 {save}",
  "Remote edit under 1,000 unsent ones": "有 1,000 处未发送编辑时收到远端编辑",
  "to the first frame, with 100,000 formulas": "到第一帧，有 10 万个公式",
  "Edit reaching everyone else, p95": "编辑到达其他所有人，p95",
  "{clients} clients, {rate} edits a second, {rtt} ms round trip added":
    "{clients} 个客户端，每秒 {rate} 处编辑，另加往返 {rtt} 毫秒",
  "Import {rows} rows of CSV": "导入 {rows} 行 CSV",
  "{text} in {columns} columns, into a table of {file}":
    "{text}，{columns} 列，得到 {file} 的数据表",
  "Sort {rows} rows by a number": "{rows} 行按数字排序",
  "by a text {text}; filter by a number {filter}": "按文本 {text}；按数字筛选 {filter}",
  "Main thread per frame, {rows} rows": "主线程每帧用时，{rows} 行",
  "p99 of scrolling, the worst of both renderers; {n} frames with placeholders":
    "滚动的 p99，取两个渲染器中较差的；{n} 帧有占位符",

  // Renderers
  "Canvas 2D and WebGPU": "Canvas 2D 与 WebGPU",
  "{cells} cells in view, {width} × {height}, scrolling five screens a second.":
    "视野内 {cells} 个单元格，{width} × {height}，每秒滚动五屏。",
  "Frame work, p99": "每帧用时，p99",
  Median: "中位数",
  "Late frames": "迟到的帧",

  // Architecture
  "What flows between the browser, the server and the simulator":
    "浏览器、服务器与仿真器之间的数据流",
  "One kernel without I/O, in the browser, the server, the tests and the simulator.":
    "同一个不含 I/O 的内核，用在浏览器、服务器、测试和仿真器里。",
  "Browser tab": "浏览器标签页",
  "Main thread": "主线程",
  Interface: "界面",
  Input: "输入",
  Scene: "场景",
  "Engine worker": "引擎 Worker",
  "Kernel and formulas": "内核与公式",
  "Sync client": "同步客户端",
  "Write-ahead log": "预写日志",
  "Two snapshots": "两份快照",
  Server: "服务器",
  "Sync server": "同步服务器",
  "An actor for each workbook": "每个工作簿一个 Actor",
  "Stored before acknowledged": "先落库再确认",
  Snapshots: "快照",
  "AI draft branches": "AI 草稿分支",
  "Deterministic simulator": "确定性仿真器",
  "A simulated network": "模拟的网络",
  "Faults from a seed": "按种子注入的故障",
  "commands by ID": "按 ID 的命令",
  "pages of cells": "单元格页",
  "logged before it is sent": "先写日志再发送",
  transactions: "事务",
  "ordered commits": "定序的提交",
  "fork, review, merge": "分叉、审阅、合并",
  "same code": "同一份代码",
  "What an edit goes through": "一次编辑的经过",
  "A command names the cell by the identity of its row and column.": "命令用行列的身份指明单元格。",
  "The worker applies it, logs it, and recalculates in slices of 4 ms.":
    "Worker 应用它，写入日志，以 4 毫秒一片重算。",
  "Changed pages of cells come back, and the next frame draws them.":
    "变化的单元格页返回，下一帧画出来。",
  "In a shared workbook the server orders it, stores it, then tells everyone.":
    "在共享工作簿里，由服务器定序、落库，然后通知所有人。",
  "Edits not yet sent are taken back, and replayed after it.":
    "尚未发送的编辑先撤回，在它之后重放。",

  // Checks
  "How it is checked": "怎样验证",
  "{formulas} generated formulas: {agreement} agree with {libreoffice}. The rest is of {classes} explained kinds; {unexplained} are unexplained. Of {more} others, {ironcalc} agree with IronCalc and {formualizer} with Formualizer.":
    "{formulas} 条生成的公式中，{agreement} 与 {libreoffice} 一致，其余分为 {classes} 类已说明的差异，未说明的 {unexplained} 条。另外 {more} 条中，{ironcalc} 条与 IronCalc 一致，{formualizer} 条与 Formualizer 一致。",
  Arithmetic: "算术",
  "{cases} cases recorded in Excel 16.0 test comparing, rounding and MOD.":
    "用 Excel 16.0 中记录的 {cases} 个用例测试比较、取整与 MOD。",
  Sync: "同步",
  "Lost and reordered messages, crashes and restored backups, in one seeded world: {invariants} invariants, {ci} seeds on each push, {nightly} each night.":
    "在由种子决定的世界里丢失和打乱消息、崩溃、恢复备份：{invariants} 条不变量，每次推送 {ci} 个种子，每晚 {nightly} 个。",
  "Arbitrary input": "任意输入",
  "{targets} fuzz targets run each night.": "{targets} 个模糊测试目标每晚运行。",
  "The same bits": "逐位相同",
  "The kernel's self-check gives {digest} on x86-64 Windows and Linux, on Arm Linux, and in three browsers.":
    "内核自检在 x86-64 Windows 和 Linux、Arm Linux 以及三种浏览器里都得到 {digest}。",
  "Older files": "旧文件",
  "{files} frozen files in {generations} formats are read and replayed on each push.":
    "{generations} 种格式的 {files} 个冻结文件，每次推送都读取并重放。",
  Pictures: "画面",
  "{cases} pictures from both renderers are compared pixel by pixel.":
    "{cases} 幅画面由两个渲染器绘制，逐像素比较。",
  Browsers: "浏览器",
  "Chromium, Firefox and WebKit on each push, Safari each night.":
    "每次推送用 Chromium、Firefox 和 WebKit，每晚用 Safari。",

  // Limits
  "Known limits": "已知限制",
  "Not Excel: no macros, charts, pivot tables, conditional formats, pictures or printing.":
    "不是 Excel：没有宏、图表、数据透视表、条件格式、图片和打印。",
  "Formulas use commas and English function names.": "公式使用逗号和英文函数名。",
  "Shared workbooks hold up to 2 million cells.": "共享工作簿最多 200 万个单元格。",
  "Local workbooks are in this browser only, which may delete them.":
    "本地工作簿只在此浏览器里，浏览器可能会删除。",
  "No editing on phones and tablets.": "手机和平板上不能编辑。",
  "One server, which does not scale out.": "只有一台服务器，不能横向扩展。",
  "Safari: Canvas 2D only, input methods untested.": "Safari：只用 Canvas 2D，输入法未经测试。",
  "Not yet tested with a screen reader by a person.": "尚未由人用读屏软件测试。",
  "The AI Analyst needs a language model on the server. No scores are published.":
    "AI 分析师需要服务器上有语言模型。没有公布评测分数。",

  Articles: "文章",
  "Licensed under <0>MIT</0> or <1>Apache-2.0</1>, at your option.":
    "以 <0>MIT</0> 或 <1>Apache-2.0</1> 许可，任选其一。",
};

export default zhLanding;
