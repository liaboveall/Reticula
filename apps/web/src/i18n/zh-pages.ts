/**
 * Chinese texts of the pages other than the editor. What only the benchmarks and
 * the simulator say is in `zh-labs.ts`. The glossary is in `zh-core.ts`.
 */
const zhPages: Readonly<Record<string, string>> = {
  // Links between the pages
  Pages: "页面",
  Editor: "编辑器",
  "Open Editor": "打开编辑器",

  Language: "语言",

  // Signing in
  "Dev: {account}": "开发登录：{account}",
  "Sign-in failed": "登录失败",

  // Workbooks
  "My Cloud Workbooks": "我的云端工作簿",
  "Shared with Me": "与我共享",
  "Local Browser Docs": "本地工作簿",
  "Built-in Examples": "内置示例",
  "Sign in to manage cloud collaborative workbooks.": "登录后即可管理多人协作的云端工作簿。",
  "New cloud workbook title": "新云端工作簿的标题",
  "Import .rtcl": "导入 .rtcl",
  "No cloud workbooks yet.": "还没有云端工作簿。",
  "No shared workbooks yet.": "还没有与我共享的工作簿。",
  "No local OPFS workbooks found in this browser.": "此浏览器的 OPFS 中没有本地工作簿。",
  "role: {role} · visibility: {visibility} · head #{seq} · {size} · updated {time}":
    "角色：{role} · 可见范围：{visibility} · 最新修订 #{seq} · {size} · 更新于 {time}",
  "Open|verb": "打开",
  "Open {title}": "打开 {title}",
  "id: {id} · {size}": "ID：{id} · {size}",
  "Open Local": "在本地打开",
  "Convert to Cloud": "转为云端工作簿",
  "Delete Local": "删除本地工作簿",
  'Packing "{title}" and uploading to cloud…': "正在打包“{title}”并上传到云端…",
  "Failed to load workbooks": "无法加载工作簿",
  "Could not create cloud workbook": "无法创建云端工作簿",
  "Could not rename workbook": "无法重命名工作簿",
  "Could not delete workbook": "无法删除工作簿",
  "Could not import snapshot": "无法导入快照",
  "Could not convert local workbook to cloud": "无法将本地工作簿转为云端工作簿",
  "Could not remove local workbook": "无法删除本地工作簿",
  "1,000,000-Row Budget Sample": "1,000,000 行预算示例",
  "22 columns · 100,000 bound formulas · runs locally in Web Worker + OPFS":
    "22 列 · 100,000 个绑定公式 · 通过 Web Worker + OPFS 在本地运行",
  "Open Sample": "打开示例",
  "The workbook is open in another tab. Close it there to delete it.":
    "工作簿正在另一个标签页中打开，请先在那里关闭再删除。",
  "{n} data tables · {size} · stay in this browser": "{n} 个数据表 · {size} · 只在此浏览器里",
  "10,000,000-Row Data Table": "10,000,000 行数据表",
  "15 columns · made in this browser, about 190 MB of its storage":
    "15 列 · 在此浏览器里生成，约占 190 MB 存储",
  "Open Data Table": "打开数据表",
  "Deterministic Sync Simulation Lab": "确定性同步仿真实验室",
  "Interactive 2–3 replica offline merge & 5-replica fault injector without sign-in":
    "动手体验 2–3 个副本的离线合并，或对 5 个副本注入故障，无需登录",
  "Open /sim": "打开 /sim",

  // Account
  "Account & Storage": "账号与存储",
  "Loading account…": "正在加载账号…",
  "Not signed in. Sign in to view cloud storage usage or manage data.":
    "尚未登录。登录后即可查看云端存储用量、管理数据。",
  "Owned workbooks: {owned} · Shared workbooks: {shared}":
    "拥有的工作簿：{owned} · 与我共享的工作簿：{shared}",
  "Cloud Storage Usage": "云端存储用量",
  "Data Tables": "数据表",
  "{used} used of {quota} ({percent}%)": "已用 {used}，共 {quota}（{percent}%）",
  "Server disk volume is full; creating or importing new cloud workbooks is paused.":
    "服务器磁盘已满，暂停新建和导入云端工作簿。",
  "Server storage volume is above the 80% warning threshold.":
    "服务器存储用量已超过 80% 的警戒线。",
  "Daily AI Analyst Quota": "AI 分析师每日配额",
  "Status: {status} · Runs today: {runs} / {runLimit} · Tokens today: {tokens} / {tokenLimit}":
    "状态：{status} · 今日运行：{runs} / {runLimit} · 今日 token：{tokens} / {tokenLimit}",
  Enabled: "已启用",
  "Demo replays only (cloud LLM not configured)": "仅演示回放（未配置云端 LLM）",
  "Delete Account": "删除账号",
  "Permanently deletes your user profile, sessions, GitHub link, and all cloud workbooks you own (including snapshots). Contributions you made inside workbooks owned by others remain intact under “Deleted user” so collaborator history stays valid. Type <0>DELETE</0> below to confirm.":
    "永久删除用户资料、会话、与 GitHub 的关联，以及所拥有的全部云端工作簿（包括快照）。在他人拥有的工作簿中所做的修改会完整保留，署名改为“已注销用户”，以保证协作者的历史记录有效。在下方输入 <0>DELETE</0> 确认。",
  'Type "DELETE"': "输入“DELETE”",
  "Permanently Delete Account": "永久删除账号",
  "Could not load account summary": "无法加载账号信息",
  "Could not delete account": "无法删除账号",

  // Privacy
  "Privacy & Data Notice": "隐私与数据说明",
  "Reticula is an independent personal project built as a local-first spreadsheet engine with optional collaborative cloud storage.":
    "Reticula 是一个独立的个人项目：一个本地优先的电子表格引擎，另有可选的协作云存储。",
  "What Data Is Stored": "存储哪些数据",
  "<0>Local mode (/app without Cloud):</0> All spreadsheets stay strictly inside your browser's Origin Private File System (OPFS) or memory and never leave your device.":
    "<0>本地模式（/app，不使用云端）：</0>所有表格都只保存在浏览器的源私有文件系统（OPFS）或内存中，绝不会离开本机设备。",
  "<0>Cloud collaboration (opt-in sign-in):</0> When you sign in via GitHub, the server stores your GitHub numeric user ID, public login/display name, avatar URL, and the cloud workbooks, revision logs, and snapshots you create or share.":
    "<0>云端协作（自愿登录）：</0>通过 GitHub 登录后，服务器会存储登录者的 GitHub 数字用户 ID、公开的登录名/显示名称、头像网址，以及登录者创建或共享的云端工作簿、修订日志和快照。",
  "Cookies & Tracking": "Cookie 与跟踪",
  "No third-party analytics, ads, or tracking scripts are used. Cloud sign-in uses only strictly necessary first-party <0>HttpOnly</0> session cookies.":
    "不使用任何第三方统计分析、广告或跟踪脚本。云端登录只使用严格必要的第一方 <0>HttpOnly</0> 会话 Cookie。",
  "Who Can Access Cloud Workbooks": "谁可以访问云端工作簿",
  "Workbooks default to <0>private</0> and are accessible only to explicit workbook members unless the owner creates an active link-view share token and enables <0>link_view</0>.":
    "工作簿默认为 <0>private</0>，只有明确列为成员的用户可以访问，除非所有者创建了有效的“通过链接查看”共享令牌，并启用 <0>link_view</0>。",
  "Deleting Your Data & Backups": "删除数据与备份",
  "You can permanently erase your profile, login session, and all owned cloud workbooks at any time from the <0>Account page</0>. Because this is a personal project with no commercial SLA guarantee, export important workbooks regularly as <1>.rtcl</1> snapshots.":
    "可以随时在<0>账号页面</0>永久清除个人资料、登录会话，以及所拥有的全部云端工作簿。这是一个个人项目，没有商业 SLA 保证，因此请定期将重要的工作簿导出为 <1>.rtcl</1> 快照。",
};

export default zhPages;
