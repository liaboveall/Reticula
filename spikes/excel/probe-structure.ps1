# Structural-edit semantics of the locally installed Excel.
#
# Records how formulas change when rows are inserted, deleted, moved, sorted and
# filled. The engine addresses rows and columns by stable identity instead of by
# position; this is the reference its visible behaviour is compared against.
#
# Safe to run: a hidden Excel instance, one unsaved scratch workbook, no
# clipboard, closed without saving.
#
#   pwsh spikes/excel/probe-structure.ps1 > spikes/excel/results/structure.txt

$ErrorActionPreference = 'Stop'
$xl = $null; $wb = $null
try {
  $xl = New-Object -ComObject Excel.Application
  $xl.Visible = $false
  $xl.DisplayAlerts = $false
  "Excel version $($xl.Version), build $($xl.Build)"
  $wb = $xl.Workbooks.Add()

  # A1:A12 hold 1..12 and C1 holds =SUM(A5:A9).
  function New-Case {
    $ws = $wb.Worksheets.Add()
    foreach ($i in 1..12) { $ws.Cells.Item($i, 1).Value2 = $i }
    $ws.Range('C1').Formula = '=SUM(A5:A9)'
    return $ws
  }
  function Show($label, $cell) {
    $value = $cell.Value2
    if ($value -is [int]) { $value = $cell.Text }
    '{0,-58} {1,-24} = {2}' -f $label, $cell.Formula, $value
  }
  function Attempt($label, [scriptblock]$action) {
    try { & $action; '{0,-58} ok' -f $label }
    catch { '{0,-58} refused: {1}' -f $label, ($_.Exception.Message -replace '\s+', ' ').Trim() }
  }

  ''
  '== a range reference, =SUM(A5:A9), under inserts and deletes =='
  $ws = New-Case; [void]$ws.Rows.Item(7).Insert();  Show 'insert at row 7 (inside the range)' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Rows.Item(10).Insert(); Show 'insert at row 10 (just below the last row)' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Rows.Item(5).Insert();  Show 'insert at row 5 (at the first row)' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Rows.Item(9).Delete();  Show 'delete row 9 (the last row)' $ws.Range('C1')
  [void]$ws.Rows.Item(9).Insert();                  Show '  then insert at row 9' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Rows.Item(5).Delete();  Show 'delete row 5 (the first row)' $ws.Range('C1')
  [void]$ws.Rows.Item(5).Insert();                  Show '  then insert at row 5' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Rows.Item(9).Delete(); [void]$ws.Rows.Item(8).Insert(); Show 'delete row 9, then insert at row 8' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Rows.Item('5:9').Delete(); Show 'delete rows 5:9 (the whole range)' $ws.Range('C1')

  ''
  '== a single-cell reference, =A5 =='
  $ws = New-Case; $ws.Range('D1').Formula = '=A5'
  [void]$ws.Rows.Item(3).Insert(); Show 'insert at row 3 (above the referenced cell)' $ws.Range('D1')
  $ws = New-Case; $ws.Range('D1').Formula = '=A5'
  [void]$ws.Rows.Item(5).Delete(); Show 'delete row 5 (the referenced row)' $ws.Range('D1')

  ''
  '== whole-column and position-dependent formulas, insert at row 3 =='
  $ws = New-Case
  $ws.Range('D1').Formula = '=SUM(A:A)'
  $ws.Range('D5').Formula = '=ROW()'
  $ws.Range('E5').Formula = '=ROW(A5)'
  $ws.Range('F1').Formula = '=MATCH(9,A1:A12,0)'
  $ws.Range('G1').Formula = '=INDEX(A1:A12,5)'
  $ws.Range('H1').Formula = '=ROWS(A1:A12)'
  $ws.Range('I1').Formula = '=INDIRECT("A5")'
  $ws.Range('J1').Formula = '=OFFSET(A1,4,0)'
  Show 'before: SUM over a whole column' $ws.Range('D1')
  [void]$ws.Rows.Item(3).Insert()
  $ws.Range('A3').Value2 = 100
  Show 'after:  SUM over a whole column (new row holds 100)' $ws.Range('D1')
  Show 'after:  =ROW(), was in D5' $ws.Range('D6')
  Show 'after:  =ROW(A5), was in E5' $ws.Range('E6')
  Show 'after:  =MATCH(9,A1:A12,0)' $ws.Range('F1')
  Show 'after:  =INDEX(A1:A12,5)' $ws.Range('G1')
  Show 'after:  =ROWS(A1:A12)' $ws.Range('H1')
  Show 'after:  =INDIRECT("A5")' $ws.Range('I1')
  Show 'after:  =OFFSET(A1,4,0)' $ws.Range('J1')

  ''
  '== the sheet is full: row 1048576 =='
  $ws = New-Case
  Attempt 'insert a row while the last row is empty' { [void]$ws.Rows.Item(3).Insert() }
  $ws.Cells.Item(1048576, 1).Value2 = 1
  Attempt 'insert a row while the last row holds a value' { [void]$ws.Rows.Item(3).Insert() }
  $ws.Range('D1').Formula = '=ROWS(A:A)'; Show 'row count' $ws.Range('D1')

  ''
  '== copy and fill adjust relative references =='
  $ws = New-Case
  $ws.Range('D1').Formula = '=A1+$A$2+A$3+$A4'
  [void]$ws.Range('D1').Copy($ws.Range('E3'))
  Show 'copy D1 to E3 (one column right, two rows down)' $ws.Range('E3')
  [void]$ws.Range('D1:D3').FillDown()
  Show 'fill D1 down: D3' $ws.Range('D3')

  ''
  '== cut a cell onto a cell that is referenced elsewhere =='
  $ws = New-Case
  $ws.Range('A20').Value2 = 20
  $ws.Range('D1').Formula = '=A9'
  $ws.Range('D2').Formula = '=A20'
  $ws.Range('D3').Formula = '=SUM(A18:A22)'
  $ws.Range('D4').Formula = '=SUM(A19:A20)'
  [void]$ws.Range('A9').Cut($ws.Range('A20'))
  Show 'reference to the moved cell, was =A9' $ws.Range('D1')
  Show 'reference to the overwritten cell, was =A20' $ws.Range('D2')
  Show 'range containing the overwritten cell, was =SUM(A18:A22)' $ws.Range('D3')
  Show 'range ending at the overwritten cell, was =SUM(A19:A20)' $ws.Range('D4')
  Show 'the other range, was =SUM(A5:A9)' $ws.Range('C1')

  ''
  '== cut with an explicit destination: a corner of a range moves =='
  $ws = New-Case; [void]$ws.Range('A9').Cut($ws.Range('A20')); Show 'cut A9 to A20 (last cell moved away)' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Range('A5').Cut($ws.Range('A2'));  Show 'cut A5 to A2 (first cell moved up)' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Range('A7').Cut($ws.Range('A20')); Show 'cut A7 to A20 (an interior cell)' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Range('A9').Cut($ws.Range('A3'));  Show 'cut A9 to A3 (last cell to above the first)' $ws.Range('C1')
  $ws = New-Case; [void]$ws.Rows.Item(9).Cut($ws.Rows.Item(20)); Show 'cut row 9 onto row 20 (whole row, overwrite)' $ws.Range('C1')

  ''
  '== sort A1:D5 by column A (values 5,3,1,4,2) =='
  $ws = $wb.Worksheets.Add()
  $vals = 5, 3, 1, 4, 2
  foreach ($i in 1..5) {
    $ws.Cells.Item($i, 1).Value2 = $vals[$i - 1]
    $ws.Cells.Item($i, 2).Formula = "=A$i*10"          # same row, inside the range
    $ws.Cells.Item($i, 4).Formula = "=H$i"             # same row, outside the range
    $ws.Cells.Item($i, 8).Value2 = 100 * $i
  }
  $ws.Range('C1').Formula = '=A2'; $ws.Range('C2').Formula = '=A3'; $ws.Range('C3').Formula = '=A4'
  $ws.Range('C4').Formula = '=A5'; $ws.Range('C5').Formula = '=A1'   # one row down, or four rows up
  $ws.Range('F1').Formula = '=A2'; $ws.Range('F2').Formula = '=$A$2'; $ws.Range('F3').Formula = '=SUM(A1:A3)'
  $ws.Range('E1').Formula = '=$H$1'; $ws.Range('E2').Formula = '=H$1'
  $missing = [Type]::Missing
  [void]$ws.Range('A1:E5').Sort($ws.Range('A1'), 1, $missing, $missing, 1, $missing, 1, 2)
  'column A after sorting:           ' + ((1..5 | ForEach-Object { $ws.Cells.Item($_, 1).Value2 }) -join ', ')
  'B, was =A<row>*10:                ' + ((1..5 | ForEach-Object { $ws.Cells.Item($_, 2).Formula }) -join '  ')
  'C, was one row down / 4 rows up:  ' + ((1..5 | ForEach-Object { $ws.Cells.Item($_, 3).Formula }) -join '  ')
  'D, was =H<row> (outside the sort): ' + ((1..5 | ForEach-Object { $ws.Cells.Item($_, 4).Formula }) -join '  ')
  'E, absolute references:           ' + ((1..5 | ForEach-Object { $ws.Cells.Item($_, 5).Formula }) -join '  ')
  Show 'outside the range: =A2' $ws.Range('F1')
  Show 'outside the range: =$A$2' $ws.Range('F2')
  Show 'outside the range: =SUM(A1:A3)' $ws.Range('F3')

  ''
  '== arrays and implicit intersection =='
  $ws = New-Case
  try {
    $cell = $ws.Range('E1'); $cell.Formula2 = '=A1:A3*2'
    "dynamic-array formula =A1:A3*2 in E1: spills=$($cell.HasSpill), E1:E3 = $($cell.Value2), $($ws.Range('E2').Value2), $($ws.Range('E3').Value2)"
  } catch { "dynamic arrays are not available: $($_.Exception.Message)" }
  $cell = $ws.Range('G5'); $cell.Formula = '=A1:A12'
  "legacy formula =A1:A12 in G5: value=$($cell.Value2), stored as $($cell.Formula2)"

  ''
  '== sheets =='
  $data = $wb.Worksheets.Add(); $data.Name = 'Data'; $data.Range('A1').Value2 = 5
  $ws = $wb.Worksheets.Add(); $ws.Range('D1').Formula = '=Data!A1'
  Show 'before deleting the sheet' $ws.Range('D1')
  $data.Delete()
  Show 'after deleting the referenced sheet' $ws.Range('D1')
  $first = $wb.Worksheets.Add(); $first.Name = 'Report'
  $second = $wb.Worksheets.Add()
  Attempt 'rename a second sheet to an existing name' { $second.Name = 'Report' }
}
catch { "ERROR: $($_.Exception.Message)" }
finally {
  if ($wb) { try { $wb.Close($false) } catch {} }
  if ($xl) { try { $xl.Quit() } catch {}; [void][Runtime.InteropServices.Marshal]::ReleaseComObject($xl) }
  [GC]::Collect(); [GC]::WaitForPendingFinalizers()
}
