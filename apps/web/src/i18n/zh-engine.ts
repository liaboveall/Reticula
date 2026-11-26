/**
 * Chinese for what the engine, the workers and the server write in English: each
 * message, and each word a message may have in a placeholder. The messages are
 * listed in `packages/protocol/src/generated/ui-messages.json`, those of the
 * workers are read off their sources. The glossary is in `zh-core.ts`; here,
 *
 *   WAL, edit log 编辑日志 · pending 待确认 · epoch 代际 · digest 摘要 ·
 *   sequence 序号 · identifier 标识符 · checkpoint 检查点 · branch 分支 ·
 *   quota 配额, 存储空间 · frame (of the protocol) 帧 · token (of a model) token
 */
const zhEngine: Readonly<Record<string, string>> = {
  // What the history and the undo button call a change
  "Edit {addr_label}": "编辑 {addr_label}",
  "Clear cells": "清除单元格",
  "Insert row": "插入行",
  "Insert {count} rows": "插入 {count} 行",
  "Delete rows": "删除行",
  "Move rows": "移动行",
  "Resize rows": "调整行高",
  "Insert column": "插入列",
  "Insert {count} columns": "插入 {count} 列",
  "Delete columns": "删除列",
  "Move columns": "移动列",
  "Resize columns": "调整列宽",
  "Freeze panes": "冻结窗格",
  "Format cells": "设置单元格格式",
  "Auto-fill": "自动填充",
  "Fill down": "向下填充",
  "Fill right": "向右填充",
  Paste: "粘贴",
  Sort: "排序",
  "Replace All": "全部替换",
  "Define {trimmed}": "定义名称 {trimmed}",
  "Delete {name}": "删除名称 {name}",
  "Add sheet": "添加工作表",
  "Rename sheet": "重命名工作表",
  "Delete sheet": "删除工作表",
  "Move sheet": "移动工作表",
  "Undo: {label}": "撤销：{label}",
  "Redo: {label}": "重做：{label}",
  "Merge AI draft": "合并 AI 草稿",
  "Merge: {title}": "合并：{title}",

  // Why the engine turns an edit down
  "Nothing was changed: another workbook has been opened meanwhile.":
    "没有任何改动：期间已打开另一个工作簿。",
  "The workbook is open in another tab.": "此工作簿正在另一个标签页中打开。",
  "Target sheet does not exist.": "目标工作表不存在。",
  "Target sheet not found.": "找不到目标工作表。",
  "Target worksheet does not exist.": "目标工作表不存在。",
  "Target row or column no longer exists.": "目标行或列已不存在。",
  "Row count must be at least 1.": "行数至少为 1。",
  "Column count must be at least 1.": "列数至少为 1。",
  "Select at least one row to delete.": "请至少选中一行再删除。",
  "Select at least one column to delete.": "请至少选中一列再删除。",
  "Select at least one row to move.": "请至少选中一行再移动。",
  "Select at least one column to move.": "请至少选中一列再移动。",
  "Identifier counter exhausted.": "标识符已用尽。",
  "Collaborative worksheet identifier limit reached.": "协作工作簿的工作表标识符已用尽。",
  "Unsupported number format code: {code}": "不支持的数字格式代码：{code}",
  "Cannot apply AutoFilter to the selected range.": "无法对所选区域应用筛选。",
  "Column filter outside active AutoFilter bounds.": "该列不在当前筛选的范围内。",
  "Invalid named range identifier '{trimmed}'.": "“{trimmed}”不能用作名称。",
  "Invalid named range formula: {err}": "名称的公式无效：{err}",
  "Named range '{name}' was not found.": "找不到名称“{name}”。",
  "Named range identifier or comment is invalid.": "名称或其备注无效。",
  "Maximum named range count reached.": "名称的数量已达上限。",
  "Sheet ID already exists in this workbook.": "此工作簿中已有相同 ID 的工作表。",
  "Maximum sheet count (256) reached.": "工作表数量已达上限（256 个）。",
  "Sheet name must be 1–31 characters and cannot contain : \\ / ? * [ or ].":
    "工作表名称须为 1–31 个字符，且不能包含 : \\ / ? * [ 或 ]。",
  "Cannot delete the last visible worksheet.": "无法删除最后一个可见的工作表。",
  "Invalid worksheet operation.": "无效的工作表操作。",
  "Operation exceeds per-transaction cell limit.": "操作超过单个事务的单元格上限。",
  "Operation contains duplicate target cells.": "操作中有重复的目标单元格。",
  "Axis limit or target error ({axis_err}).": "行或列的操作无效（{axis_err}）。",
  "invalid run of rows or columns": "行或列的范围无效",
  "This worksheet has no room for that: it can have 2,000,000 rows and 16,384 columns, including deleted ones.":
    "此工作表已没有空间：最多 2,000,000 行、16,384 列，已删除的也计算在内。",
  "insertion point not found": "找不到插入位置",
  "row or column already exists": "行或列已存在",
  "row or column not found": "找不到行或列",
  "nothing to restore": "没有可恢复的内容",
  "destination within the moved rows or columns": "目标位置在要移动的行或列之内",
  "Dimension, freeze count, or format is out of bounds.": "尺寸、冻结数量或格式超出允许范围。",
  "Numeric value must be finite.": "数值必须是有限的。",
  "Cell text exceeds 32,767 UTF-16 code units.": "单元格文本超过 32,767 个 UTF-16 编码单元。",
  "Formula failed validation.": "公式未通过校验。",
  "String table capacity exhausted.": "文本存储已满。",
  "Local workbook memory limit (1 GiB) exceeded.": "超过本地工作簿的内存上限（1 GiB）。",
  "This workbook is read-only.": "此工作簿为只读。",
  "Row sort permutation is invalid.": "排序后的行顺序无效。",
  "Cut-and-paste source and destination ranges are invalid.": "剪切的源区域或粘贴的目标区域无效。",

  // Why a formula is not accepted
  "Formula cannot be empty after '='.": "“=”后面的公式不能为空。",
  "Formula exceeds maximum length of 8,192 characters.": "公式超过 8,192 个字符的长度上限。",
  "Formula exceeds maximum length of {MAX_FORMULA_CHARS} characters.":
    "公式超过 {MAX_FORMULA_CHARS} 个字符的长度上限。",
  "Formula exceeds maximum nesting depth of 64 levels.": "公式的嵌套超过 64 层。",
  "Function call exceeds maximum of 255 arguments.": "函数的参数超过 255 个。",
  "Formula syntax error.": "公式语法错误。",
  "Invalid token in formula ({lex}).": "公式中有无效的内容（{lex}）。",
  "formula too long": "公式过长",
  "unterminated text": "文本缺少右引号",
  "unterminated worksheet name": "工作表名称缺少右引号",
  "unknown error value": "未知的错误值",
  "malformed number": "数字格式不正确",
  "unexpected character": "意外的字符",
  "Invalid cell or range reference.": "无效的单元格或区域引用。",
  "Invalid array literal (must be rectangular constant literals).":
    "无效的数组常量（必须是由常量组成的矩形）。",
  "Expected expression after '='.": "“=”后面应有表达式。",
  "Unterminated string literal (missing closing '\"').": '文本缺少右引号 (")。',
  'Unterminated worksheet reference (expected closing "\'!" ).': "工作表引用不完整（缺少 '!）。",
  "Unrecognised error literal '{slice}'.": "无法识别的错误值“{slice}”。",
  "Invalid numeric literal.": "无效的数字。",
  "Unexpected character in formula.": "公式中有意外的字符。",
  "Unknown worksheet '{name}'.": "未知的工作表“{name}”。",
  "Invalid range endpoints around ':'.": "“:”两侧不是有效的区域端点。",
  "Incomplete range reference around ':'.": "“:”两侧的区域引用不完整。",
  "Unmatched closing ')'.": "多余的右括号“)”。",
  "Unclosed '(' (missing ')').": "左括号“(”没有对应的右括号“)”。",
  "Unknown function '{upper_name}'.": "未知的函数“{upper_name}”。",
  "{name} expects {min_args} argument, got {got}.":
    "{name} 需要 {min_args} 个参数，当前有 {got} 个。",
  "{name} expects {min_args} arguments, got {got}.":
    "{name} 需要 {min_args} 个参数，当前有 {got} 个。",
  "{name} expects at least {min_args} argument(s), got {got}.":
    "{name} 至少需要 {min_args} 个参数，当前有 {got} 个。",
  "{name} expects {min_args} to {max_args} arguments, got {got}.":
    "{name} 需要 {min_args} 到 {max_args} 个参数，当前有 {got} 个。",
  "Expected cell or row/column coordinate after ':'.": "“:”后面应为单元格或行列坐标。",
  "Expected expression after separator.": "分隔符后面应有表达式。",
  "Expected operand after operator.": "运算符后面应有运算数。",
  Worksheet: "工作表",
  "Reference cells on this worksheet.": "引用此工作表中的单元格。",
  "Named range": "名称",

  // Filling, copying and pasting
  "Fill range falls outside visible rows.": "填充区域超出可见的行。",
  "Fill range exceeds worksheet dimensions.": "填充区域超出工作表的范围。",
  "Fill range must align with source rows or source columns.": "填充区域必须与源区域的行或列对齐。",
  "Fill exceeds 2,000,000-cell transaction limit.": "填充超过单个事务 2,000,000 个单元格的上限。",
  "Select at least one cell to copy.": "请至少选中一个单元格再复制。",
  "Copy range exceeds worksheet dimensions.": "复制区域超出工作表的范围。",
  "Copy selection falls outside visible rows.": "复制的选区超出可见的行。",
  "Clipboard copy is capped at {MAX_CLIPBOARD_CELLS} cells (use Export CSV for larger ranges).":
    "一次最多复制 {MAX_CLIPBOARD_CELLS} 个单元格（更大的区域请导出 CSV）。",
  "Paste target falls outside visible rows.": "粘贴的目标超出可见的行。",
  "Paste target exceeds worksheet bounds.": "粘贴的目标超出工作表的范围。",
  "Pasted table exceeds worksheet bounds.": "粘贴的表格超出工作表的范围。",
  "Paste exceeds 2,000,000-cell transaction limit.": "粘贴超过单个事务 2,000,000 个单元格的上限。",

  // Finding and replacing
  "Search query cannot be empty": "查找内容不能为空",
  "Regular expression exceeded per-cell evaluation step limit":
    "正则表达式在单个单元格上的匹配步数超过上限",
  "Regular expression exceeds {MAX_REGEX_LEN} characters": "正则表达式超过 {MAX_REGEX_LEN} 个字符",
  "Regular expression nesting too deep": "正则表达式嵌套过深",
  "Regular expression exceeds NFA state budget": "正则表达式过于复杂",
  "Unexpected closing parenthesis ')'": "多余的右括号“)”",
  "Unexpected end of regular expression": "正则表达式不完整",
  "Unclosed group '('": "分组的左括号“(”没有闭合",
  "Unclosed character class '['": "字符组的“[”没有闭合",
  "Unclosed character range": "字符范围不完整",
  "Invalid character class range endpoint": "字符组中范围的端点无效",
  "Character class range out of order": "字符组中范围的起止颠倒",
  "Trailing backslash '\\' in regular expression": "正则表达式以反斜杠“\\”结尾",
  "Quantifier '{ch}' without preceding atom": "量词“{ch}”前面没有可重复的内容",
  "Quantifier repeat count exceeds {MAX_QUANTIFIER_REPEAT}":
    "量词的重复次数超过 {MAX_QUANTIFIER_REPEAT}",
  "Quantifier min exceeds max in {m,n}": "量词 {m,n} 的最小值大于最大值",

  // Opening, saving and importing
  "Engine halted after an unrecoverable internal panic; reload to recover.":
    "引擎因无法恢复的内部错误而停止，重新加载即可恢复。",
  "engine panic: {info}": "引擎内部错误：{info}",
  "Sequence number must be a safe non-negative integer.": "序号必须是安全范围内的非负整数。",
  "WAL header magic or CRC-32 checksum is corrupted.": "编辑日志的文件头或 CRC-32 校验和已损坏。",
  "WAL base snapshot sequence mismatch (found {found}, expected {expected}).":
    "编辑日志所依据的快照序号不符（实际为 {found}，应为 {expected}）。",
  "WAL base snapshot sequence is unreadable (expected {expected}).":
    "无法读取编辑日志所依据的快照序号（应为 {expected}）。",
  "Non-contiguous WAL sequence at byte {offset} (expected {expected_seq}, found {found_seq}).":
    "编辑日志在第 {offset} 字节处序号不连续（应为 {expected_seq}，实际为 {found_seq}）。",
  "Mid-file WAL corruption at byte {offset} after sequence {last_valid_seq}.":
    "编辑日志在第 {offset} 字节处损坏，位于序号 {last_valid_seq} 之后。",
  "WAL corruption detected: {corr}": "编辑日志已损坏：{corr}",
  "WAL header encode failed: {e}": "无法写入编辑日志的文件头：{e}",
  "WAL replay failed: {e}": "无法重放编辑日志：{e}",
  "encodeSeedStub failed: {e}": "encodeSeedStub 失败：{e}",
  "encodeSnapshotFull failed: {e}": "encodeSnapshotFull 失败：{e}",
  "No active snapshot stream.": "当前没有正在写入的快照。",
  "Snapshot step failed: {e}": "写入快照失败：{e}",
  "Snapshot decode failed: {e}": "无法读取快照：{e}",
  "invalid WireTxnV3: {e}": "无效的 WireTxnV3：{e}",
  "invalid WireTxnV3 ops: {e}": "WireTxnV3 中有无效的操作：{e}",
  "apply op failed: {e}": "无法应用操作：{e}",
  "CSV file exceeds the 256 MiB local import limit.": "CSV 文件超过 256 MiB 的本地导入上限。",
  "No active CSV import.": "当前没有正在进行的 CSV 导入。",
  "CSV file is not valid UTF-8 text.": "CSV 文件不是有效的 UTF-8 文本。",
  "CSV file contains an unclosed double-quoted field.": "CSV 文件中有未闭合的双引号字段。",
  "CSV dimensions exceed worksheet limits (2,000,000 rows × 16,384 cols).":
    "CSV 的大小超过工作表上限（2,000,000 行 × 16,384 列）。",

  // History
  "Return to the live workbook before editing.": "请先返回当前版本再编辑。",
  "snapshot decode failed: {err}": "无法读取快照：{err}",
  "snapshot #{seq} is not in LRU cache": "快照 #{seq} 不在缓存中",
  "missing cached snapshot": "缓存中缺少快照",
  "no base snapshot loaded": "尚未加载基准快照",
  "base snapshot #{base_seq} evicted before fallback replay":
    "基准快照 #{base_seq} 在重放之前已被移出缓存",
  "could not decode history transaction batch": "无法读取一批历史事务",
  "missing transaction in history slice": "历史记录中缺少事务",
  "non-contiguous history sequence: expected {max_seq}, got {seq}":
    "历史记录的序号不连续：应为 {max_seq}，实际为 {seq}",
  "history target #{target_seq} outside loaded span #{base_seq}..=#{max_seq}":
    "修订 #{target_seq} 不在已加载的范围 #{base_seq}–#{max_seq} 内",
  "sequence offset overflow": "序号偏移溢出",
  "worksheet {sheet_id} does not exist at #{current_seq}":
    "在修订 #{current_seq} 中不存在工作表 {sheet_id}",
  "failed to decode history txn #{seq}: {err}": "无法读取历史事务 #{seq}：{err}",
  "failed to apply history txn #{seq}: {err}": "无法应用历史事务 #{seq}：{err}",

  // Collaboration
  "No collaborative workbook is open.": "没有打开的协作工作簿。",
  "This collaborative workbook is read-only.": "此协作工作簿为只读。",
  "The collaborative workbook is still catching up.": "协作工作簿仍在同步。",
  "Collaborative edits require a verified replica.": "副本通过校验后才能协作编辑。",
  "Collaborative session has failed.": "协作会话已失败。",
  "Collaborative checkpoint metadata is missing.": "缺少协作检查点的元数据。",
  "Invalid collaborative replica metadata.": "协作副本的元数据无效。",
  "Invalid collaborative document or branch identifier.": "协作文档或分支的标识符无效。",
  "Pending transactions do not belong to this collaborative replica.":
    "待确认的事务不属于此协作副本。",
  "Could not decode collaborative checkpoint: {error}": "无法读取协作检查点：{error}",
  "Could not encode collaborative checkpoint: {error}": "无法写入协作检查点：{error}",
  "Could not recover pending transactions: {error}": "无法恢复待确认的事务：{error}",
  "Could not encode collaborative output.": "无法生成协作数据。",
  "Could not preserve rejected collaborative edits.": "无法留存被拒绝的协作编辑。",
  "This transaction cannot be encoded for sync.": "无法为同步编码此事务。",
  "This collaborative transaction exceeds the sync frame limit.":
    "此协作事务超过同步帧的大小上限。",

  // Workers, storage and the connection
  "A collaborative worker requires an effect handler.": "协作 Worker 需要副作用处理函数。",
  "Could not apply a collaboration effect.": "无法应用协作带来的更改。",
  "The engine worker failed.": "引擎 Worker 出错。",
  "The engine worker stopped.": "引擎 Worker 已停止。",
  "No collaborative worker is open.": "没有打开的协作 Worker。",
  "This worker holds a collaborative workbook.": "此 Worker 正在使用协作工作簿。",
  "Open a local workbook in a separate worker.": "请在单独的 Worker 中打开本地工作簿。",
  "Open a collaborative workbook in a fresh worker.": "请在新的 Worker 中打开协作工作簿。",
  "Engine did not emit ready on fresh workbook initialization": "新建工作簿时引擎没有就绪",
  "Could not open collaborative replica.": "无法打开协作副本。",
  "Could not open read-only collaborative view.": "无法打开协作工作簿的只读视图。",
  "This collaborative workbook was saved by a newer version of Reticula. Please update and reload.":
    "此协作工作簿由较新版本的 Reticula 保存。请更新并重新加载。",
  "Offline: cannot open an additional window for this collaborative workbook without a network connection.":
    "离线：没有网络连接时，无法为此协作工作簿再打开一个窗口。",
  "This replica session was replaced by a newer connection.": "此副本的会话已被更新的连接取代。",
  "Access to this workbook was revoked.": "对此工作簿的访问权限已被撤销。",
  "Access denied while downloading collaborative snapshot.": "下载协作快照时被拒绝访问。",
  "Server memory budget is currently busy (4008). Retrying shortly…":
    "服务器内存暂时不足 (4008)，稍后重试…",
  "A newer version of Reticula is required by the server. Please reload.":
    "服务器需要较新版本的 Reticula。请重新加载。",
  "Server backup restored (epoch {oldEpoch} → {newEpoch}). Local edits are being reconciled.":
    "服务器已从备份恢复（代际 {oldEpoch} → {newEpoch}）。正在合并本地编辑。",
  "State verification mismatch detected; resyncing workbook from server.":
    "状态校验不一致，正在从服务器重新同步工作簿。",
  "Snapshot HTTP {status}": "快照请求返回 HTTP {status}",
  "Exit AI draft preview before editing.": "请先退出 AI 草稿预览再编辑。",
  "History playback requires an active collaborative workbook.":
    "只有打开协作工作簿时才能查看历史记录。",
  "Could not load snapshot #{snapshotSeq} (HTTP {status})":
    "无法加载快照 #{snapshotSeq} (HTTP {status})",
  "Could not load history transactions (HTTP {status})": "无法加载历史事务 (HTTP {status})",
  'Workbook "{docId}" is currently open in another browser tab.':
    "工作簿“{docId}”已在浏览器的另一个标签页中打开。",
  'Workbook "{docId}" requires Safe Mode recovery before uploading to cloud.':
    "工作簿“{docId}”需要先在安全模式中恢复，才能上传到云端。",
  'Workbook "{docId}" has to be opened once to repair its data tables.':
    "需要先打开一次工作簿“{docId}”，以修复其中的数据表。",
  "CSV exceeds the 256 MiB local workbook import limit.": "CSV 超过 256 MiB 的本地工作簿导入上限。",
  "Snapshot file is missing; use Export Raw Files instead.": "快照文件缺失，请改用“导出原始文件”。",
  "Snapshot slot '{activeSlot}' is missing or empty.": "快照槽“{activeSlot}”缺失或为空。",
  "state.json is unreadable.": "无法读取 state.json。",
  "state.json has invalid activeSlot": "state.json 中的 activeSlot 无效",
  "Previous open attempt terminated unexpectedly while loading or recalculating (sentinel = {nextOpeningCount}).":
    "上一次打开在加载或重算时意外中止（计数为 {nextOpeningCount}）。",
  "Safe Mode was requested via ?safe=1.": "地址中的 ?safe=1 要求进入安全模式。",
  "SyncAccessHandle wrote 0 bytes.": "SyncAccessHandle 写入了 0 字节。",
  "WAL SyncAccessHandle write returned 0": "编辑日志的 SyncAccessHandle 写入了 0 字节",
  "Snapshot SyncAccessHandle write returned 0": "快照的 SyncAccessHandle 写入了 0 字节",
  "Browser storage quota was reached while saving collaborative checkpoint. Pending edits remain in memory.":
    "保存协作检查点时浏览器存储空间已满。待确认的编辑仍在内存中。",
  "Browser storage quota was reached while writing the pending edit log. Your workbook remains in memory.":
    "写入待确认的编辑日志时浏览器存储空间已满。工作簿仍在内存中。",
  "Browser storage quota was reached while saving unsynced edits as a local workbook.":
    "将未同步的编辑另存为本地工作簿时浏览器存储空间已满。",
  "Browser storage quota was reached while updating collaborative storage.":
    "更新协作存储时浏览器存储空间已满。",
  "Browser storage quota was reached while writing the edit log. Your workbook is still safe in memory — export a .rtcl snapshot or delete unused local workbooks.":
    "写入编辑日志时浏览器存储空间已满。工作簿仍安全地保留在内存中，请导出 .rtcl 快照，或删除不用的本地工作簿。",
  "Browser storage quota was reached while compacting a snapshot. Your workbook remains intact in memory — export .rtcl to save to disk.":
    "整理快照时浏览器存储空间已满。工作簿在内存中完好，请导出 .rtcl 保存到磁盘。",
  "Browser storage quota was reached while writing snapshot. Previous snapshot and edit log remain intact — export .rtcl to save to disk.":
    "写入快照时浏览器存储空间已满。之前的快照和编辑日志完好，请导出 .rtcl 保存到磁盘。",
  "Browser storage quota was reached while initializing local storage. Your workbook is running in memory — export .rtcl to save.":
    "初始化本地存储时浏览器存储空间已满。工作簿目前只在内存中，请导出 .rtcl 保存。",

  // XLSX files
  "XLSX input ({bytes} bytes) exceeds 64 MiB limit ({MAX_XLSX_INPUT_BYTES} bytes)":
    "XLSX 文件（{bytes} 字节）超过 64 MiB 的上限（{MAX_XLSX_INPUT_BYTES} 字节）",
  "XLSX decompressed XML ({bytes} bytes) exceeds 256 MiB limit ({MAX_XLSX_UNCOMPRESSED_BYTES} bytes)":
    "XLSX 解压后的 XML（{bytes} 字节）超过 256 MiB 的上限（{MAX_XLSX_UNCOMPRESSED_BYTES} 字节）",
  "invalid XLSX archive: {msg}": "无效的 XLSX 压缩包：{msg}",
  "invalid OOXML XML: {msg}": "无效的 OOXML XML：{msg}",
  "XLSX read error: {msg}": "读取 XLSX 出错：{msg}",
  "XLSX write error: {msg}": "写入 XLSX 出错：{msg}",
  "snapshot codec error: {msg}": "快照编解码出错：{msg}",
  "workbook construction error: {msg}": "生成工作簿出错：{msg}",
  "worksheet not found for patch: {name}": "找不到要修改的工作表：{name}",
  "workbook has zero worksheets": "工作簿中没有工作表",
  "worksheet count {sheets} exceeds maximum {MAX_SHEETS}":
    "工作表有 {sheets} 个，超过 {MAX_SHEETS} 个的上限",
  "missing xl/workbook.xml": "缺少 xl/workbook.xml",
  "missing worksheet entry {zip_path}": "缺少工作表文件 {zip_path}",
  "failed reading {name} as UTF-8: {e}": "无法以 UTF-8 读取 {name}：{e}",
  "workbook.xml.rels: {e}": "workbook.xml.rels：{e}",
  "workbook.xml: {e}": "workbook.xml：{e}",
  "metadata.xml: {e}": "metadata.xml：{e}",
  "sharedStrings.xml: {e}": "sharedStrings.xml：{e}",
  "styles.xml: {e}": "styles.xml：{e}",
  "snapshot bytes were already consumed": "快照数据已被取走",
  "xlsx bytes were already consumed": "XLSX 数据已被取走",
  "{merged_cells_count} merged range(s) on `{name}`":
    "“{name}”中的 {merged_cells_count} 个合并区域",
  "{conditional_fmt_count} conditional formatting block(s) on `{name}`":
    "“{name}”中的 {conditional_fmt_count} 处条件格式",
  "{drawing_count} drawing/shape reference(s) on `{name}`":
    "“{name}”中的 {drawing_count} 处绘图或形状",
  "VBA macro project (xl/vbaProject.bin) is not executed or re-emitted on fresh export":
    "VBA 宏工程 (xl/vbaProject.bin) 不会执行，重新导出时也不保留",
  "{charts} embedded chart XML part(s)": "{charts} 个嵌入的图表",
  "{drawings} drawing/media archive asset(s)": "{drawings} 个绘图或媒体文件",
  "{pivots} pivot table definition(s)": "{pivots} 个数据透视表",
  "{comments} worksheet comment stream(s)": "{comments} 组工作表批注",
  "{external_links} external workbook/data connection part(s)":
    "{external_links} 个外部工作簿或数据连接",
  "Skipped {truncated_count} cell(s) exceeding Excel's 1,048,576 x 16,384 worksheet limits":
    "跳过 {truncated_count} 个单元格，它们超出 Excel 工作表 1,048,576 × 16,384 的上限",
  "Skipped {truncated_count} patched cell(s) exceeding Excel's 1,048,576 x 16,384 worksheet limits":
    "跳过 {truncated_count} 个修改过的单元格，它们超出 Excel 工作表 1,048,576 × 16,384 的上限",

  // Reviewing a draft
  "Edit {sheet_name}!{span} ({cluster} cells)": "编辑 {sheet_name}!{span}（{cluster} 个单元格）",
  "Format {sheet_name} ({format_items} cells)":
    "设置 {sheet_name} 的格式（{format_items} 个单元格）",
  "Sort {sheet_name}!{range_str} ({rows} rows)": "排序 {sheet_name}!{range_str}（{rows} 行）",
  "Conflict at {sheet_name}!{address}": "{sheet_name}!{address} 有冲突",
  "Define named range {name} = {rhs}": "定义名称 {name} = {rhs}",
  "Delete named range {canonical_name}": "删除名称 {canonical_name}",
  "Add worksheet '{name}'": "添加工作表“{name}”",
  "Delete worksheet '{sname}'": "删除工作表“{sname}”",
  "Rename worksheet '{old}' -> '{name}'": "将工作表“{old}”重命名为“{name}”",
  "Reorder worksheet '{sname}'": "移动工作表“{sname}”",
  "Insert {ids} row(s) on {sname} {pos_hint}": "在 {sname} {pos_hint}插入 {ids} 行",
  "at row {p}": "第 {p} 行处",
  "after anchor row": "基准行之后",
  "at top": "顶部",
  "Insert {ids} column(s) on {sname}": "在 {sname} 插入 {ids} 列",
  "Delete {del_rows} row(s) on {sname}": "在 {sname} 删除 {del_rows} 行",
  "Delete {del_cols} column(s) on {sname}": "在 {sname} 删除 {del_cols} 列",
  "Move {count} row(s) on {sname}": "在 {sname} 移动 {count} 行",
  "Move {count} column(s) on {sname}": "在 {sname} 移动 {count} 列",
  "Structural update": "结构更改",
  Workbook: "工作簿",
  "<deleted on main>": "<主表中已删除>",
  "<deleted row>": "<已删除的行>",
  "{sname} row {pos}": "{sname} 第 {pos} 行",
  "{sname} row#{row}": "{sname} 行 #{row}",
  "{sname} col {letters}": "{sname} {letters} 列",
  "{sname} col#{col}": "{sname} 列 #{col}",
  "sheet '{sname}'": "工作表“{sname}”",
  "Main branch deleted rows or columns inside the sorted range": "主表删除了排序区域内的行或列",
  "Main branch modified cells inside the sorted range": "主表修改了排序区域内的单元格",
  "Main branch deleted this cell's row, column, or worksheet since the fork point":
    "草稿创建后，主表删除了此单元格所在的行、列或工作表",
  "Both main and AI draft modified this cell since the fork point":
    "草稿创建后，主表和 AI 草稿都修改了此单元格",
  "Main branch edited cells on '{sname}' after fork; deleting sheet will discard them":
    "草稿创建后，主表编辑了“{sname}”中的单元格；删除该工作表会丢失这些编辑",
  "Main branch modified cells on rows deleted by the AI draft":
    "主表修改了 AI 草稿所删除的行中的单元格",
  "Main branch modified cells on columns deleted by the AI draft":
    "主表修改了 AI 草稿所删除的列中的单元格",
  "Target worksheet was deleted on main": "主表中已删除目标工作表",
  "Worksheet '{sname}' was deleted on main": "主表中已删除工作表“{sname}”",
  "Named range modified concurrently on main": "主表同时修改了此名称",
  "cannot apply draft edit at {sheet_name}!{address}: target row, column, or worksheet was deleted on main":
    "无法应用草稿对 {sheet_name}!{address} 的编辑：主表中已删除目标行、列或工作表",
  "cannot initialize merge IdAllocator": "无法为合并初始化 IdAllocator",
  "exhausted ID space during merge remap": "合并时标识符已用尽",

  // Merging a draft
  "Main changed while you were reviewing; review the updated diff and resolve any conflicts.":
    "审阅期间主表有改动。请审阅更新后的差异并解决冲突。",
  "{unresolved_conflicts} conflict cell(s) require an explicit Keep Main or Use AI Draft choice":
    "{unresolved_conflicts} 个冲突单元格需要选择“保留主表”或“采用 AI 草稿”",
  "{conflicts} conflict cell(s) require an explicit resolution":
    "{conflicts} 个冲突单元格需要先解决",
  "{closure_issues} formula reference(s) depend on rejected structural inserts":
    "{closure_issues} 处公式引用依赖未接受的插入",
  "{issues} formula reference(s) depend on rejected structural inserts":
    "{issues} 处公式引用依赖未接受的插入",
  "merge conflict": "合并冲突",
  "merge rejected: {reason}": "合并被拒绝：{reason}",
  "branch is already {status}": "分支已处于“{status}”状态",
  "branch is no longer active": "分支已不再处于活动状态",
  active: "进行中",
  merged: "已合并",
  abandoned: "已放弃",
  "cannot merge branch while an AI analyst run is still in progress":
    "AI 分析师仍在运行，无法合并该分支",
  "cannot merge main into itself": "无法将主表合并到自身",
  "cannot abandon the main branch": "无法放弃主表",
  "diff requires a non-main branch": "只能对主表以外的分支计算差异",
  "branch kind must be `user` or `ai_draft`": "分支类型必须是 `user` 或 `ai_draft`",
  "branch kind must be `ai_draft` or `user`": "分支类型必须是 `ai_draft` 或 `user`",
  "branch title must contain 1 to 120 printable characters": "分支标题须为 1 到 120 个可打印字符",

  // Why the server turns a transaction down
  Forbidden: "没有权限",
  InvalidOp: "无效的操作",
  "NonContiguousClientSeq { expected: {expected} }": "事务序号不连续，应为 {expected}",
  QuotaExceeded: "超过配额",
  "EpochMismatch { server_epoch: {server_epoch} }": "代际不一致，服务器为 {server_epoch}",
  UpgradeRequired: "需要更新版本",
  PayloadTooLarge: "数据过大",

  // Running the AI Analyst
  "AI analyst is not configured on this server": "此服务器没有配置 AI 分析师",
  "AI analyst is disabled on restart": "服务器重启后停用了 AI 分析师",
  "an AI analyst run is already in progress for your account": "此账号已有 AI 分析师在运行",
  "server AI analyst capacity or memory budget is saturated": "服务器的 AI 分析师名额或内存已满",
  "daily AI run limit reached ({used_runs}/{max_runs})":
    "今天的 AI 运行次数已用完 ({used_runs}/{max_runs})",
  "daily AI token limit reached ({used_tokens}/{max_tokens})":
    "今天的 AI token 已用完 ({used_tokens}/{max_tokens})",
  "prompt must contain between 1 and {MAX_PROMPT_CHARS} characters":
    "提示须为 1 到 {MAX_PROMPT_CHARS} 个字符",
  "prompt contains unsupported control characters": "提示中有不支持的控制字符",
  "Completed analysis with no workbook changes.": "分析完成，工作簿没有改动。",
  "agent run cancelled": "运行已取消",
  cancelled: "已取消",
  "agent run timed out after {0}s": "运行超过 {0} 秒，已中止",
  "agent run exceeded budget: {0}": "运行超过预算：{0}",
  "reached max_steps ({max_steps})": "达到步数上限 ({max_steps})",
  "reached max_tokens ({max_tokens})": "达到 token 上限 ({max_tokens})",
  "aborted after {identical_repeat_count} identical consecutive `{name}` calls":
    "连续 {identical_repeat_count} 次以相同参数调用 `{name}`，已中止",
  "workbook operation failed: {0}": "工作簿操作失败：{0}",
  "step store error: {0}": "保存步骤出错：{0}",
  "merge planning error: {0}": "准备合并时出错：{0}",
  "LLM provider error: {0}": "模型服务出错：{0}",
  "upstream HTTP {status} after {retries} retries: {snippet}":
    "重试 {retries} 次后上游仍返回 HTTP {status}：{snippet}",
  "upstream HTTP {status}: {snippet}": "上游返回 HTTP {status}：{snippet}",
  "upstream error: {msg}": "上游出错：{msg}",
  "SSE upstream error: {msg}": "上游 SSE 出错：{msg}",
  "SSE chunk read error: {e}": "读取 SSE 数据出错：{e}",
  "network error after {retries} retries: {err}": "重试 {retries} 次后仍有网络错误：{err}",
  "failed reading body: {e}": "无法读取响应正文：{e}",
  "invalid JSON response: {e}; snippet={text_body}": "响应不是有效的 JSON：{e}；片段：{text_body}",
  "response missing `choices[0]`": "响应中缺少 `choices[0]`",
  "response missing `choices[0].message`": "响应中缺少 `choices[0].message`",
  "draft engine initialization failed: {err}": "无法初始化草稿引擎：{err}",
  "failed to initialize draft IdAllocator": "无法为草稿初始化 IdAllocator",
  "missing agent replica metadata": "缺少 AI 副本的元数据",
  "missing agent replica watermark on recovery": "恢复时缺少 AI 副本的水位",
  "draft branch is no longer active": "草稿分支已不再进行",
  "workbook log_epoch advanced since run started; cannot resume safely":
    "运行开始后工作簿的代际有变化，无法安全地继续",
  "maximum crash recovery resume limit (2) exceeded": "崩溃后恢复运行的次数超过上限（2 次）",
  "run interrupted mid-mutating tool call; cannot resume idempotently":
    "运行在修改数据的过程中中断，无法安全地继续",
  "agent concurrency slots saturated on startup": "启动时 AI 分析师的名额已满",
  "failed to load draft branch state on recovery": "恢复时无法加载草稿分支",
  "shared memory budget saturated on startup": "启动时共享内存已满",

  // Answers of the server
  "authentication required": "需要登录",
  "GitHub authentication failed": "GitHub 登录失败",
  "access denied": "没有权限",
  "not found": "未找到",
  "rate limit exceeded": "请求过于频繁",
  "server document budget busy": "服务器的文档内存暂时不足",
  "internal server error": "服务器内部错误",
  "Reticula API returned {status}": "Reticula API 返回 {status}",
  "server storage capacity reached": "服务器存储空间已满",
  "user storage quota exceeded": "账号的存储空间已用完",
  "owner storage quota exceeded": "所有者的存储空间已用完",
  "owner table storage quota exceeded": "所有者的数据表空间已用完",
  "table uploads are busy; try again shortly": "正在上传的数据表太多，请稍后重试",
  "another table is being uploaded for the owner of this workbook":
    "此工作簿的所有者有另一个数据表正在上传",
  "another table has this ID": "另一个数据表已使用此标识符",
  "the workbook has as many tables as it can": "工作簿的数据表已达上限",
  "the request has too many bytes of the table": "请求中的数据超出了数据表的大小",
  "the table is not being uploaded": "此数据表不在上传中",
  "this is not a table file": "这不是数据表文件",
  "the table file is larger than this server accepts": "数据表文件超过服务器接受的大小",
  "the table file does not have the declared checksum": "数据表文件与声明的校验和不符",
  "the table file is not as long as it says it is": "数据表文件的长度与其记录不符",
  "the table has too many columns or groups of rows for this server":
    "数据表的列或行组超过服务器的上限",
  "the table file could not be read": "无法读取数据表文件",
  "replica ID space exhausted": "副本标识符已用尽",
  "snapshot payload exceeds import byte limit": "快照超过导入的大小上限",
  "invalid or oversized snapshot": "快照无效或过大",
  "title must contain 1 to 120 printable characters": "标题须为 1 到 120 个可打印字符",
  "{field} must be between 1 and 120 characters": "{field}须为 1 到 120 个字符",
  "{field} must not contain control characters": "{field}不能包含控制字符",
  "workbook title": "工作簿标题",
  "table name": "数据表名称",
  "version name": "版本名称",
  "X-Workbook-Title header is required": "缺少 X-Workbook-Title 请求头",
  "X-Workbook-Title is not valid UTF-8": "X-Workbook-Title 不是有效的 UTF-8",
  "invalid version sequence": "版本序号无效",
  "version sequence exceeds workbook head": "版本序号超过工作簿的最新修订",
  "a named version with this title already exists": "已有同名的版本",
  'account deletion requires confirm = "DELETE"': "删除账号需要输入 DELETE 确认",
  "create an active viewer share link before enabling link view":
    "请先创建有效的查看者共享链接，再允许通过链接查看",
  "link lifetime must be between 1 second and 1 year": "链接的有效期须在 1 秒到 1 年之间",
  "share links can grant editor or viewer access": "共享链接只能授予编辑者或查看者权限",
  "share link label must be at most 120 printable characters":
    "共享链接的备注最多 120 个可打印字符",
  "only editor or viewer roles can be assigned": "只能分配编辑者或查看者角色",
  "cannot assign deleted user": "无法为已删除的用户分配角色",
  "cannot demote the sole workbook owner": "无法更改工作簿唯一所有者的角色",
  "cannot remove the sole workbook owner": "无法移除工作簿的唯一所有者",
  Viewer: "查看者",

  // The simulator
  "Edit R{row}C{col}": "编辑 R{row}C{col}",
  "Insert Row {after_row_idx}": "插入第 {after_row_idx} 行",
  "Delete Row {row_idx}": "删除第 {row_idx} 行",
  "Undo {label}": "撤销：{label}",
  "nothing to undo on this replica": "此副本没有可撤销的操作",
  "Hello (seq #{last_applied_seq})": "Hello（序号 #{last_applied_seq}）",
  "Welcome (r{replica}, head #{head_seq})": "Welcome（r{replica}，最新 #{head_seq}）",
  "Welcome (r{replica}, head #{head_seq}, {txns} txn)":
    "Welcome（r{replica}，最新 #{head_seq}，{txns} 个事务）",
  "Welcome (r{replica}, head #{head_seq}, snap #{snapshot_seq})":
    "Welcome（r{replica}，最新 #{head_seq}，快照 #{snapshot_seq}）",
  "Push {txns} txn(s) from #{client_seq}": "Push：自 #{client_seq} 起 {txns} 个事务",
  "Push (empty)": "Push（空）",
  "Commit #{seq} (r{replica})": "Commit #{seq} (r{replica})",
  "Ack r{replica} c#{client_seq} -> #{seq}": "Ack r{replica} c#{client_seq} → #{seq}",
  "Reject c#{client_seq}: {reason}": "Reject c#{client_seq}：{reason}",
  "Digest #{seq}": "Digest #{seq}",
  Presence: "Presence",
  "Ping #{n}": "Ping #{n}",
  "Pong #{n}": "Pong #{n}",
  "Fetch snapshot #{snap_seq}": "获取快照 #{snap_seq}",
  "Restarted from persisted OPFS state": "已从 OPFS 中保存的状态重新启动",
  "Rescued {txns} terminally rejected edit(s)": "已留存 {txns} 条最终被拒绝的修改",
  "Server epoch changed {old_epoch} -> {new_epoch}": "服务器代际由 {old_epoch} 变为 {new_epoch}",
  "Digest mismatch detected at #{seq}; resyncing": "在 #{seq} 处发现摘要不一致，正在重新同步",
  "unsupported link mode `{raw}`": "不支持的连接模式 `{raw}`",
  "replica index out of range": "副本编号超出范围",
  "replica count must be 2 or 3, got {replica_count}":
    "副本数必须是 2 或 3，当前为 {replica_count}",
  "missing SheetId(0)": "缺少 SheetId(0)",
  "row index {row} out of range": "行号 {row} 超出范围",
  "row index {idx} out of range": "行号 {idx} 超出范围",
  "row index {row_idx} out of range": "行号 {row_idx} 超出范围",
  "column index {col} out of range": "列号 {col} 超出范围",
  "invalid row position": "行的位置无效",
  "row ID space exhausted": "行标识符已用尽",
  "failed to inspect prior cell": "无法读取单元格原来的内容",
  "invalid cell input: {err}": "单元格输入无效：{err}",
  "local edit rejected: {err}": "本地修改被拒绝：{err}",
  "OPFS snapshot encode failed: {err}": "无法写入 OPFS 快照：{err}",
  "OPFS snapshot decode failed: {err}": "无法读取 OPFS 快照：{err}",
  "OPFS pending WAL encode failed: {err}": "无法写入 OPFS 中的待确认日志：{err}",
  "OPFS pending WAL scan failed: {err}": "无法读取 OPFS 中的待确认日志：{err}",
  "server snapshot encode failed: {err}": "服务器无法写入快照：{err}",
  "missing snapshot #{snap_seq}": "缺少快照 #{snap_seq}",
  "failed to decode snapshot #{snap_seq}: {err}": "无法读取快照 #{snap_seq}：{err}",
  "failed to decode client frame: {err}": "无法读取客户端的帧：{err}",
  "failed to decode server frame: {err}": "无法读取服务器的帧：{err}",
  "deliverAll exceeded step bound": "deliverAll 超过步数上限",
  "online drain exceeded step bound": "在线投递超过步数上限",
  "step {cursor} ({event}): {err}": "第 {cursor} 步（{event}）：{err}",
};

export default zhEngine;
