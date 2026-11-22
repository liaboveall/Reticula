# Manual Excel COM evaluation cross-check for stratified differential workbooks
# (`17-m3a.md` §8.1). Loads `.xlsx` batches generated with `--dump-xlsx`,
# forces full rebuild recalculation in a hidden read-only Excel instance
# without touching the clipboard, verifies Z1/Z2 sentinels on every sheet,
# and archives evaluated values alongside the local Excel build number.
#
# Usage:
#   powershell -File spikes/excel/diff-excel.ps1 -InputDir target/qa/lo-dump -Out spikes/excel/results/diff-excel-16.0.20711.tsv
#   powershell -File spikes/excel/diff-excel.ps1 -Workbook target/qa/lo-dump/in/batch_01.xlsx -Out spikes/excel/results/batch01.tsv

param(
  [string]$InputDir = '',
  [string]$Workbook = '',
  [Parameter(Mandatory = $true)][string]$Out,
  [int]$MaxPerSheet = 0
)

$ErrorActionPreference = 'Stop'

if (-not $Workbook -and -not $InputDir) {
  throw 'Specify either -InputDir <dir> or -Workbook <file>.'
}

$files = New-Object System.Collections.Generic.List[string]
if ($Workbook) {
  $files.Add([IO.Path]::GetFullPath($Workbook))
} else {
  $inSub = Join-Path $InputDir 'in'
  $searchRoot = if (Test-Path $inSub) { $inSub } else { $InputDir }
  foreach ($f in (Get-ChildItem -Path $searchRoot -Filter '*.xlsx' | Sort-Object Name)) {
    $files.Add($f.FullName)
  }
}

if ($files.Count -eq 0) {
  throw 'No .xlsx workbooks found to evaluate.'
}

$errorNames = @{
  -2146826281 = '#DIV/0!'; -2146826246 = '#N/A'; -2146826259 = '#NAME?'; -2146826288 = '#NULL!'
  -2146826252 = '#NUM!'; -2146826265 = '#REF!'; -2146826273 = '#VALUE!'; -2146826245 = '#SPILL!'
  -2146826244 = '#CALC!'
}

function ConvertTo-ResultField($value) {
  if ($null -eq $value) { return '' }
  if ($value -is [double]) { return $value.ToString('G17', [Globalization.CultureInfo]::InvariantCulture) }
  if ($value -is [bool]) { if ($value) { return 'TRUE' } else { return 'FALSE' } }
  if ($value -is [int]) { if ($errorNames.ContainsKey($value)) { return $errorNames[$value] } else { return "error:$value" } }
  return [string]$value
}

$lines = New-Object System.Collections.Generic.List[string]
$xl = $null
try {
  $xl = New-Object -ComObject Excel.Application
  $xl.Visible = $false
  $xl.DisplayAlerts = $false
  $xl.EnableEvents = $false

  $lines.Add("# Excel version $($xl.Version), build $($xl.Build)")
  $lines.Add("# Columns: workbook`tsheet`tcell`tformula`tvalue")

  foreach ($wbPath in $files) {
    $wb = $null
    try {
      $wb = $xl.Workbooks.Open($wbPath, 0, $true)
      $xl.CalculateFullRebuild()
      $wbName = [IO.Path]::GetFileName($wbPath)

      foreach ($ws in $wb.Worksheets) {
        $sName = [string]$ws.Name
        if ($sName -eq 'Data' -or $sName -eq 'Aux') { continue }

        $z1 = $ws.Range('Z1').Value2
        $z2 = $ws.Range('Z2').Value2
        if ([Math]::Abs(([double]$z1) - 124245.0) -gt 1e-9) {
          throw "Sentinel Z1 failed on ${wbName}!${sName}: expected 124245, got $z1"
        }
        if ([Math]::Abs(([double]$z2) - 136.0) -gt 1e-9) {
          throw "Sentinel Z2 failed on ${wbName}!${sName}: expected 136, got $z2"
        }

        $used = $ws.UsedRange
        $maxRow = [int]$used.Row + [int]$used.Rows.Count - 1
        $maxCol = [int]$used.Column + [int]$used.Columns.Count - 1
        if ($maxCol -gt 24) { $maxCol = 24 }

        $vals = $ws.Range($ws.Cells.Item(1, 1), $ws.Cells.Item($maxRow, $maxCol)).Value2
        $forms = $ws.Range($ws.Cells.Item(1, 1), $ws.Cells.Item($maxRow, $maxCol)).Formula

        $emitted = 0
        for ($r = 1; $r -le $maxRow; $r++) {
          for ($c = 1; $c -le $maxCol; $c++) {
            if ($MaxPerSheet -gt 0 -and $emitted -ge $MaxPerSheet) { break }
            $f = if ($maxRow -eq 1 -and $maxCol -eq 1) { $forms } else { $forms[$r, $c] }
            $v = if ($maxRow -eq 1 -and $maxCol -eq 1) { $vals } else { $vals[$r, $c] }
            if ($null -eq $v -and ($null -eq $f -or [string]$f -eq '')) { continue }
            $addr = $ws.Cells.Item($r, $c).Address($false, $false)
            $fClean = ([string]$f).Replace("`t", ' ').Replace("`r", '').Replace("`n", ' ')
            $vClean = (ConvertTo-ResultField $v).Replace("`t", ' ').Replace("`r", '').Replace("`n", ' ')
            $lines.Add("$wbName`t$sName`t$addr`t$fClean`t$vClean")
            $emitted++
          }
          if ($MaxPerSheet -gt 0 -and $emitted -ge $MaxPerSheet) { break }
        }
      }
    }
    finally {
      if ($wb) { try { $wb.Close($false) } catch {} }
    }
  }
}
finally {
  if ($xl) {
    try { $xl.Quit() } catch {}
    [void][Runtime.InteropServices.Marshal]::ReleaseComObject($xl)
  }
  [GC]::Collect()
  [GC]::WaitForPendingFinalizers()
}

$target = [IO.Path]::GetFullPath((Join-Path (Get-Location) $Out))
$targetDir = [IO.Path]::GetDirectoryName($target)
if ($targetDir -and -not (Test-Path $targetDir)) {
  New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
}
[IO.File]::WriteAllText($target, (($lines -join "`n") + "`n"), (New-Object Text.UTF8Encoding $false))
"$($lines.Count) lines written to $target"
