/**
 * Chinese texts of the benchmarks and the simulator. The glossary is in
 * `zh-core.ts`; what only these pages speak of is called
 *
 *   benchmark 基准 · suite 测试集 · incremental recalculation 增量重算 · agreement 一致率 ·
 *   differential 差分 · simulator 仿真器 · invariant 不变量 · converge 收敛 · digest 摘要 ·
 *   epoch 代际 · sequence number 序号 · pending 待确认 · frame 帧 · deliver 投递 ·
 *   replay 重放 (of transactions), 回放 (of a recorded demo)
 */
const zhLabs: Readonly<Record<string, string>> = {
  // Benchmarks
  "Reticula Benchmarks": "Reticula 基准测试",
  "Performance & Differential Verification": "性能与差分验证",
  "Archived reproducible measurements from <0>bench/results/</0> — zero external analytics, unauthenticated static JSON.":
    "可复现的测量结果，归档在 <0>bench/results/</0>。没有外部统计分析，静态 JSON，无需登录。",
  "Baseline {commit}": "基线 {commit}",
  "HTTP {status} loading benchmark archive": "加载基准归档时返回 HTTP {status}",
  "Failed to load benchmark archive": "无法加载基准归档",
  "Error loading benchmark history: {error}": "加载基准历史时出错：{error}",
  "Loading archived benchmark results…": "正在加载归档的基准结果…",
  "Built-in Functions": "内置函数",
  "{functions} ({entries} entries)": "{functions}（{entries} 个条目）",
  "15-sig-fig IEEE 754 + Excel 1900 rules": "IEEE 754，15 位有效数字，Excel 1900 日期规则",
  "B4 Incremental 100k": "B4 增量重算 10 万",
  "Chrome WASM p50 (budget ≤ 50 ms)": "Chrome WASM p50（预算 ≤ 50 毫秒）",
  "B7a Top-Row Insert": "B7a 顶部插入行",
  "Run-encoded O(runs) identity axis": "游程编码的标识轴，O(游程数)",
  "B9a Collab Replay (1k edits)": "B9a 协作重放（1,000 条修改）",
  "Down from 981.71 ms via M2b worker incremental":
    "M2b 改为在 Worker 中增量处理，此前为 981.71 毫秒",
  "50-Client Compose Load": "50 客户端 Compose 负载",
  "{achieved} txns/s": "{achieved} 事务/秒",
  "{server} MiB server · 0 lost": "服务器 {server} MiB · 丢失 0 个事务",
  "LibreOffice Differential": "LibreOffice 差分对比",
  "{formulas} formulas · {n} unclassified": "{formulas} 个公式 · {n} 个未归类",
  "<0>Hardware & Cross-Browser Methodology:</0> Measured on <1>{cpu}</1> ({browser}). Canvas2D scroll work stays at <0>{scroll}</0> p99 in Chrome (<1>margin=2</1> prefetch: {margin}), whereas headed Firefox Canvas2D jump frame work measured <0>{firefox}</0> p99 due to Gecko software 2D text upload overhead — both strictly inside the 16.7 ms 60 Hz frame budget.":
    "<0>硬件与跨浏览器的测量方法：</0>在 <1>{cpu}</1>（{browser}）上测得。Chrome 中 Canvas2D 滚动的每帧工作耗时 p99 为 <0>{scroll}</0>（<1>margin=2</1> 预取：{margin}）；带界面的 Firefox 中 Canvas2D 跳转帧的 p99 为 <0>{firefox}</0>，原因是 Gecko 软件 2D 上传文字的开销。两者都严格低于 60 Hz 下 16.7 毫秒的帧预算。",
  "Cross-Milestone SVG Trends": "各里程碑的趋势",
  "Engine & Startup Latency (ms, Chrome WASM p50)": "引擎与启动延迟（毫秒，Chrome WASM p50）",
  "B9 Collaborative Sync Evolution (p50 ms)": "B9 协作同步的演进（p50，毫秒）",
  "LibreOffice Stratified Agreement by Family": "各函数族与 LibreOffice 的分层一致率",
  "Chrome WASM Engine and Startup Latency Comparison": "Chrome WASM 引擎与启动延迟对比",
  "B2 Sum": "B2 求和",
  "B4 100k": "B4 10 万",
  "B3 1M": "B3 100 万",
  "B5 Chain": "B5 链",
  "B10 1stScr": "B10 首屏",
  "{ms}ms": "{ms} 毫秒",
  "Baseline ({commit})": "基线（{commit}）",
  "Latest ({commit})": "最新（{commit}）",
  "B9 Collaborative Sync Replay Latency Evolution": "B9 协作同步重放延迟的演进",
  "WASM p50": "WASM p50",
  "Native p50": "原生 p50",
  "LibreOffice Differential Agreement Ratio by Function Family":
    "各函数族与 LibreOffice 差分对比的一致率",

  // The table of the benchmarks
  "Filterable Benchmark Matrix": "基准结果表",
  "All|runs": "全部",
  Native: "原生",
  "Chrome WASM": "Chrome WASM",
  "Compose Load": "Compose 负载",
  "Startup & Render": "启动与渲染",
  Environment: "环境",
  Suite: "测试集",
  Commit: "提交",
  Metric: "指标",
  Measured: "实测",
  Conditions: "条件",
  "Compose + PG18": "Compose + PG18",
  "Chrome Reference Net": "Chrome 参考网络",
  "B1 Load 20M + 1M formulas (p50)": "B1 载入 2000 万单元格 + 100 万公式（p50）",
  "B11 Save 20M snapshot (p50)": "B11 保存 2000 万单元格快照（p50）",
  "B2 SUM 1M cells (p50)": "B2 SUM 100 万单元格（p50）",
  "B3 Full recalc 1M (p50)": "B3 全量重算 100 万（p50）",
  "B4 Incremental 100k (p50 / p95)": "B4 增量重算 10 万（p50 / p95）",
  "B5 Chain 1M formulas (p50)": "B5 100 万公式的依赖链（p50）",
  "B7a Top-row local insert": "B7a 本地顶部插入行",
  "B7a Cross-range insert (p50)": "B7a 跨引用区域插入行（p50）",
  "B12 50M load / save / 100k recalc": "B12 5000 万单元格载入 / 保存 / 重算 10 万",
  "B9a 1,000 pending replay (p50) / B9b 100k block":
    "B9a 重放 1,000 条待确认修改（p50）/ B9b 10 万单元格块",
  "B9a 1,000 pending replay (p50 / p95) / B9b 100k":
    "B9a 重放 1,000 条待确认修改（p50 / p95）/ B9b 10 万",
  "{clients} clients · {millions}M cells · sustained throughput":
    "{clients} 个客户端 · {millions} 百万单元格 · 持续吞吐量",
  "{achieved} / {target} txns/s": "{achieved} / {target} 事务/秒",
  "digests match: YES, lost={lost}": "摘要一致，丢失 {lost} 个事务",
  "digests match: NO, lost={lost}": "摘要不一致，丢失 {lost} 个事务",
  "Remote visibility latency (loopback / 50 ms RTT p50→p95)":
    "远端可见延迟（本机回环 / 50 毫秒 RTT，p50→p95）",
  "Server {server} MiB · Postgres {postgres} MiB": "服务器 {server} MiB · Postgres {postgres} MiB",
  "B10 Cold start shell / firstScreen (p50)": "B10 冷启动：界面框架 / 首屏（p50）",
  "S1/S2 Canvas2D scroll work p99 (vs Firefox headed jump)":
    "S1/S2 Canvas2D 滚动工作耗时 p99（对比带界面 Firefox 的跳转）",
  "{chrome} (FF {firefox})": "{chrome}（FF {firefox}）",
  "{chrome} (Chrome only)": "{chrome}（仅 Chrome）",

  // Formula correctness
  "Formula Correctness & Differential Pipeline": "公式正确性与差分流水线",
  "Verified against <0>{libreoffice}</0> (<1>{image}</1>) over <0>{formulas}</0> stratified formulas ({catalog} catalog / {unique} unique functions, ≥ {samples} samples/fn, {cells} compared cells), <0>{nonError}</0> non-error, <0>{agreement}</0> agreement, <0>{unclassified} unclassified</0>. Also cross-checked on {rustFormulas} formulas vs <1>ironcalc_base</1> ({ironcalc} match) and <1>formualizer-eval</1> ({formualizer} match), plus {ciSeeds}/{nightlySeeds} simulation seeds and <1>{generations}</1> wire compatibility fixtures.":
    "以 <0>{libreoffice}</0>（<1>{image}</1>）为参照，验证了 <0>{formulas}</0> 个分层抽样的公式（目录中 {catalog} 个函数，去重后 {unique} 个，每个函数 ≥ {samples} 个样本，共比较 {cells} 个单元格）：<0>{nonError}</0> 无错误值，<0>{agreement}</0> 一致，<0>{unclassified} 个未归类</0>。另用 {rustFormulas} 个公式与 <1>ironcalc_base</1>（{ironcalc} 个一致）和 <1>formualizer-eval</1>（{formualizer} 个一致）交叉核对，并运行 {ciSeeds}/{nightlySeeds} 个仿真种子和 <1>{generations}</1> 线上格式兼容性夹具。",
  Family: "函数族",
  "Formulas|count": "公式数",
  "Non-Error": "无错误值",
  Matched: "一致",
  Agreement: "一致率",
  Unclassified: "未归类",
  "Public Known Divergences ({libreoffice} LibreOffice + {rust} Rust Clusters)":
    "公开的已知差异（LibreOffice {libreoffice} 类 + Rust {rust} 类）",
  Engine: "引擎",
  "Function / Category": "函数 / 类别",
  Disposition: "归类",
  Count: "数量",
  Sample: "样例",
  "Reticula vs Reference": "Reticula 对参照",
  Explanation: "说明",
  "<0>{reticula}</0> vs <0>{reference}</0>": "<0>{reticula}</0> 对 <0>{reference}</0>",
  "Intentional Excel 1900 calendar / 15-sig-fig / half-away rule":
    "有意遵循 Excel 的规则：1900 日历 / 15 位有效数字 / 远离零舍入",
  "AI Analyst — Self-Hosted Evaluation Harness": "AI 分析师：可自行运行的评测框架",
  "The repository ships a reproducible headless evaluation CLI (<0>cargo xtask eval</0>) for SpreadsheetBench Verified (400 tasks, CC BY-SA 4.0) using dual-path XLSX export (<0>patch_xlsx</0> default, <0>export_xlsx</0> fallback on structural edits), pinned <0>reticula-libreoffice:26.8.2</0> container grading, 95% Wilson confidence intervals, and three-way root-cause attribution (<0>kernel_gap</0>, <0>agent_strategy</0>, <0>model_capability</0>).":
    "代码仓库自带可复现的无界面评测命令行（<0>cargo xtask eval</0>），用于 SpreadsheetBench Verified（400 道题，CC BY-SA 4.0）：XLSX 导出有两条路径（默认 <0>patch_xlsx</0>，有结构修改时退回 <0>export_xlsx</0>），用固定版本的 <0>reticula-libreoffice:26.8.2</0> 容器评分，给出 95% Wilson 置信区间，并把失败归为三类根因（<0>kernel_gap</0>、<0>agent_strategy</0>、<0>model_capability</0>）。",
  "<0>Published Score Status:</0> Public paid-model leaderboard runs are intentionally omitted; bring your own OpenAI-compatible key via <1>.env.local</1> to run <1>cargo xtask spike s6</1> or <1>cargo xtask eval</1> locally, or try all 10 deterministic interactive AI replays directly in the <2>Editor</2>.":
    "<0>公布分数的情况：</0>有意不提供付费模型的公开排行成绩。可以在 <1>.env.local</1> 中填入自己的 OpenAI 兼容密钥，在本地运行 <1>cargo xtask spike s6</1> 或 <1>cargo xtask eval</1>；也可以直接在<2>编辑器</2>中体验全部 10 个确定性的交互式 AI 回放。",

  // The simulator
  "Sync & CRDT Simulator Lab": "同步与 CRDT 仿真实验室",
  "Dedicated <0>reticula-sim-wasm</0> engine loaded on demand: step offline replica merges interactively or run the 5-replica fault injector inside your browser.":
    "按需加载专用的 <0>reticula-sim-wasm</0> 引擎：在浏览器中逐步操作副本的离线合并，或运行 5 副本故障注入。",
  "Interactive Replicas": "交互式副本",
  "5-Replica Auto-Run": "5 副本自动运行",
  "Loading simulator WebAssembly module…": "正在加载仿真器的 WebAssembly 模块…",
  "Could not load simulator WASM.": "无法加载仿真器的 WASM。",
  "Replicas:": "副本：",
  "2 Replicas (Alice & Bob)": "2 个副本（Alice 和 Bob）",
  "3 Replicas (Alice, Bob & Carol)": "3 个副本（Alice、Bob 和 Carol）",
  "Reset Lab": "重置实验室",
  "Deliver 1": "投递 1 帧",
  "Deliver All ({n})": "全部投递（{n}）",
  "Server State": "服务器状态",
  "headSeq #{seq}": "headSeq #{seq}",
  "epoch {epoch}": "代际 {epoch}",
  "digest {digest}…": "摘要 {digest}…",
  "All Replicas Converged": "所有副本已收敛",
  "Diverged / Pending Sync": "未收敛 / 等待同步",
  "{name} ({role})": "{name}（{role}）",
  "Replica {n}": "副本 {n}",
  Converged: "已收敛",
  "initializing|connection": "正在初始化",
  "offline|connection": "离线",
  "handshaking|connection": "正在握手",
  "awaitingSnapshot|connection": "等待快照",
  "online|connection": "在线",
  "seq #{seq}": "序号 #{seq}",
  "pending {n}": "待确认 {n}",
  "rep {id}": "副本 ID {id}",
  "Replica {n} row": "副本 {n} 的行",
  "Replica {n} column": "副本 {n} 的列",
  "Replica {n} cell input": "副本 {n} 的单元格输入",
  "Replica {n} network link": "副本 {n} 的网络连接",
  "Value or =formula": "值或 =公式",
  Apply: "应用",
  "Row Top": "顶部插入行",
  "Link: Online": "连接：在线",
  "Link: Delayed": "连接：延迟",
  "Link: Dropped": "连接：断开",
  "Crash & Restart": "崩溃并重启",
  "Restarted from persisted OPFS state": "已从 OPFS 中保存的状态重启",
  "Rescued {n} terminally rejected edit(s)": "已留存 {n} 条最终被拒绝的修改",
  "Server epoch changed {from} -> {to}": "服务器代际由 {from} 变为 {to}",
  "Digest mismatch detected at #{seq}; resyncing": "在 #{seq} 处发现摘要不一致，正在重新同步",
  "row index {row} out of range": "行号 {row} 超出范围",
  "column index {col} out of range": "列号 {col} 超出范围",
  "invalid cell input: {reason}": "单元格输入无效：{reason}",
  "local edit rejected: {reason}": "本地修改被拒绝：{reason}",
  "Wire Queue Inspector ({n} frames queued)": "传输队列（{n} 帧排队中）",
  "No queued network frames. Switch a replica link to Delayed to hold frames for step-by-step inspection.":
    "没有排队的网络帧。把某个副本的连接切换为“延迟”，即可留住网络帧，逐步查看。",
  "Deterministic Seed:": "确定性种子：",
  "Seed must be an unsigned 64-bit integer.": "种子必须是 64 位无符号整数。",
  "Run Seed": "运行种子",
  "Running…": "正在运行…",
  Reset: "重置",
  "Steps: <0>{done} / {total}</0>": "步骤：<0>{done} / {total}</0>",
  "server headSeq #{seq}": "服务器 headSeq #{seq}",
  "{n} Invariants Verified": "{n} 条不变量均已验证",
  "Verification Failed": "验证未通过",
  "Fault & Operation Telemetry": "故障与操作统计",
  "Local edits": "本地修改",
  "SetCellsIf / Formula": "SetCellsIf / 公式",
  "Client crashes / OPFS loss": "客户端崩溃 / OPFS 丢失",
  "Server crashes / Epoch bumps": "服务器崩溃 / 代际更替",
  "Snapshot catchups / Rejects": "快照追赶 / 拒绝",
  "Rogue frames rejected": "拒绝的违规帧",
  "5 Simulated Collaborators": "5 个模拟协作者",
  Replica: "副本",
  Role: "角色",
  "Replica ID": "副本 ID",
  Connection: "连接",
  "Confirmed Seq": "已确认序号",
  Pending: "待确认",
  "Protocol Invariants (1–{n})": "协议不变量（1–{n}）",
  Idle: "未运行",
  "Checking…": "正在检查…",
  Verified: "已验证",
  Failed: "未通过",
  "Every replica's visible state equals its confirmed state plus its pending edits, replayed":
    "每个副本的可见状态等于已确认状态加上重放的待确认修改",
  "Every acknowledged transaction is in the server log exactly once":
    "每个已确认的事务在服务器日志中出现且只出现一次",
  "The server log has no gaps and keeps each replica's transactions in order":
    "服务器日志没有缺口，同一副本的事务保持先后顺序",
  "Every confirmed state has the server's StateDigest at the same sequence number":
    "每个已确认状态的 StateDigest 与服务器在同一序号上的相同",
  "No two replicas ever create the same row, column or sheet ID":
    "任何两个副本都不会创建相同的行、列或工作表 ID",
  "Invalid and unauthorized frames are rejected and leave the server unchanged":
    "无效或未授权的帧会被拒绝，服务器状态不变",
  "Applying remote edits in place gives the same state as replaying from scratch":
    "就地应用远端修改与从头重放得到相同的状态",
  "Once the network is quiet, every replica's digests match the server's":
    "网络静默后，每个副本的摘要都与服务器一致",
  "The full log and the latest snapshot plus its tail both replay to the server's state":
    "重放完整日志，或重放最新快照及其后的日志，都得到服务器的状态",
  "Every terminally rejected transaction is kept in the local unsynced archive":
    "每个最终被拒绝的事务都留存在本地的未同步存档中",
  "Committed sorts agree on the server, on every replica and after a rebuild from a snapshot":
    "已提交的排序在服务器、每个副本以及由快照重建后都一致",
};

export default zhLabs;
