# Moving whole rows in the locally installed Excel: cut, then "Insert Cut Cells".
#
# This is the one probe that uses the system clipboard, because Excel offers the
# operation no other way. The text on the clipboard is saved first and put back
# afterwards; anything else on it (an image, a list of files) is lost. Run it
# when nothing important is on the clipboard.
#
# Otherwise as safe as the other probes: a hidden Excel instance, one unsaved
# scratch workbook, closed without saving.
#
#   powershell -File spikes/excel/probe-clipboard.ps1

$ErrorActionPreference = 'Stop'
$savedText = $null
try { $savedText = Get-Clipboard -Raw } catch {}

$xl = $null; $wb = $null
try {
  $xl = New-Object -ComObject Excel.Application
  $xl.Visible = $false
  $xl.DisplayAlerts = $false
  "Excel version $($xl.Version), build $($xl.Build)"
  $wb = $xl.Workbooks.Add()

  # A1:A12 hold 1..12, so a value names the row it started in.
  # C1 is a range over rows 5 to 9; D1:D3 refer to single cells in it.
  function New-Case {
    $ws = $wb.Worksheets.Add()
    foreach ($i in 1..12) { $ws.Cells.Item($i, 1).Value2 = $i }
    $ws.Range('C1').Formula = '=SUM(A5:A9)'
    $ws.Range('D1').Formula = '=A5'
    $ws.Range('D2').Formula = '=A7'
    $ws.Range('D3').Formula = '=A9'
    return $ws
  }

  # Cuts the rows and inserts them above row $before, as dragging a row does.
  function Move-Rows($label, $rows, $before) {
    $ws = New-Case
    [void]$ws.Rows.Item($rows).Cut()
    [void]$ws.Rows.Item($before).Insert()   # consumes the cut, so nothing stays pending
    # The fixture cells moved with their rows; find them again by their formulas.
    $order = (1..12 | ForEach-Object { $ws.Cells.Item($_, 1).Value2 }) -join ' '
    $formulas = @{}
    foreach ($row in 1..12) {
      foreach ($column in 3, 4) {
        $cell = $ws.Cells.Item($row, $column)
        if ($cell.HasFormula) { $formulas[$cell.Formula] = $cell.Value2 }
      }
    }
    $range = ($formulas.Keys | Where-Object { $_ -like '=SUM*' }) | Select-Object -First 1
    $singles = ($formulas.Keys | Where-Object { $_ -notlike '=SUM*' } | Sort-Object { $formulas[$_] } | ForEach-Object { "$_ ($($formulas[$_]))" }) -join '  '
    '{0,-52} {1,-14} = {2,-3} rows now: {3}' -f $label, $range, $formulas[$range], $order
    '{0,-52} single cells: {1}' -f '', $singles
  }

  ''
  '== the range is =SUM(A5:A9); single cells are =A5, =A7, =A9 =='
  ''
  '-- an interior row leaves or moves within the range'
  Move-Rows 'row 7 to above row 2 (interior row moves out, up)' 7 2
  Move-Rows 'row 7 to above row 12 (interior row moves out, down)' 7 12
  Move-Rows 'row 6 to above row 9 (interior row, stays inside)' 6 9
  ''
  '-- the last row of the range moves'
  Move-Rows 'row 9 to above row 12 (last row moves down, out)' 9 12
  Move-Rows 'row 9 to above row 7 (last row moves inside)' 9 7
  Move-Rows 'row 9 to above row 5 (last row to above the first)' 9 5
  Move-Rows 'row 9 to above row 2 (last row far above the first)' 9 2
  ''
  '-- the first row of the range moves'
  Move-Rows 'row 5 to above row 2 (first row moves up, out)' 5 2
  Move-Rows 'row 5 to above row 8 (first row moves inside)' 5 8
  Move-Rows 'row 5 to above row 10 (first row to below the last)' 5 10
  Move-Rows 'row 5 to above row 12 (first row far below the last)' 5 12
  ''
  '-- a row from outside moves in'
  Move-Rows 'row 11 to above row 7 (into the interior)' 11 7
  Move-Rows 'row 11 to above row 5 (to the first row position)' 11 5
  Move-Rows 'row 11 to above row 10 (just below the last row)' 11 10
  Move-Rows 'row 2 to above row 7 (from above, into the interior)' 2 7
  Move-Rows 'row 2 to above row 10 (from above, to just below)' 2 10
  ''
  '-- several rows at once'
  Move-Rows 'rows 5:6 to above row 12 (first two rows move out)' '5:6' 12
  Move-Rows 'rows 8:9 to above row 2 (last two rows move out)' '8:9' 2
  Move-Rows 'rows 5:9 to above row 2 (the whole range moves)' '5:9' 2
  Move-Rows 'rows 4:10 to above row 2 (the range and its neighbours)' '4:10' 2
}
catch { "ERROR: $($_.Exception.Message)" }
finally {
  if ($wb) { try { $wb.Close($false) } catch {} }
  if ($xl) { try { $xl.Quit() } catch {}; [void][Runtime.InteropServices.Marshal]::ReleaseComObject($xl) }
  [GC]::Collect(); [GC]::WaitForPendingFinalizers()
  if ($null -ne $savedText) { try { Set-Clipboard -Value $savedText } catch {} }
}
