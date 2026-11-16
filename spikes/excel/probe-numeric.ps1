# Numeric semantics of the locally installed Excel: comparison, rounding, MOD.
#
# Reads the cases written by numeric-cases.mjs, puts each number into a cell by
# its exact bit pattern, lets Excel evaluate a fixed set of formulas over it,
# and writes every result back out, numbers again as bit patterns.
#
# Safe to run: a hidden Excel instance, one unsaved scratch workbook, no
# clipboard, closed without saving.
#
#   powershell -File spikes/excel/probe-numeric.ps1 -Out spikes/excel/results/numeric.tsv

param(
  [string]$Cases = '',
  [Parameter(Mandatory = $true)][string]$Out
)

$ErrorActionPreference = 'Stop'
if (-not $Cases) { $Cases = Join-Path $PSScriptRoot 'data\numeric-cases.tsv' }

# For each kind of case, the formulas evaluated over the pair in columns A and B.
$columns = [ordered]@{
  cmp  = @('=A1=B1', '=A1<>B1', '=A1<B1', '=A1>B1', '=A1<=B1', '=A1>=B1', '=A1-B1', '=(A1-B1)', '=A1-B1=0', '=DELTA(A1,B1)')
  rnd  = @('=ROUND(A1,B1)', '=ROUNDUP(A1,B1)', '=ROUNDDOWN(A1,B1)', '=TRUNC(A1,B1)')
  near = @('=INT(A1)', '=TRUNC(A1)', '=FLOOR(A1,1)', '=CEILING(A1,1)', '=ROUND(A1,0)', '=A1=ROUND(A1,0)', '=MOD(A1,1)')
  mod  = @('=MOD(A1,B1)', '=A1-B1*INT(A1/B1)', '=A1/B1', '=INT(A1/B1)')
  cond = @('=A1=B1', '=COUNTIF(B1,A1)', '=MATCH(A1,B1:B1,0)', '=IFERROR(VLOOKUP(A1,B1:B1,1,FALSE)=B1,FALSE)', '=SUMIF(B1,A1,B1)=B1', '=COUNTIF(B1,"="&A1)', '=MAX(A1,B1)=MIN(A1,B1)', '=SIGN(A1-B1)')
}

$errorNames = @{
  -2146826281 = '#DIV/0!'; -2146826246 = '#N/A'; -2146826259 = '#NAME?'; -2146826288 = '#NULL!'
  -2146826252 = '#NUM!'; -2146826265 = '#REF!'; -2146826273 = '#VALUE!'
}

function ConvertTo-Double([string]$hex) {
  [BitConverter]::Int64BitsToDouble([Convert]::ToInt64($hex, 16))
}

function ConvertTo-Field($value) {
  if ($null -eq $value) { return '' }
  if ($value -is [double]) { return [BitConverter]::DoubleToInt64Bits($value).ToString('X16') }
  if ($value -is [bool]) { if ($value) { return 'TRUE' } else { return 'FALSE' } }
  if ($value -is [int]) { if ($errorNames.ContainsKey($value)) { return $errorNames[$value] } else { return "error:$value" } }
  return "text:$value"
}

$all = Import-Csv -Path $Cases -Delimiter "`t"
$lines = New-Object System.Collections.Generic.List[string]
$xl = $null; $wb = $null
try {
  $xl = New-Object -ComObject Excel.Application
  $xl.Visible = $false
  $xl.DisplayAlerts = $false
  $wb = $xl.Workbooks.Add()
  $lines.Add("# Excel version $($xl.Version), build $($xl.Build)")
  $lines.Add('# Numbers are the 16 hex digits of an IEEE 754 double. "stored" is "=" when Excel kept')
  $lines.Add('# both inputs bit for bit, otherwise the two values it holds instead.')

  foreach ($kind in $columns.Keys) {
    $rows = @($all | Where-Object { $_.kind -eq $kind })
    $count = $rows.Count
    if ($count -eq 0) { continue }
    $formulas = $columns[$kind]
    $lines.Add("# ${kind}: id, kind, a, b, stored, " + ($formulas -join ', '))

    $ws = $wb.Worksheets.Add()
    $inputs = [object[,]]::new($count, 2)
    for ($i = 0; $i -lt $count; $i++) {
      $inputs[$i, 0] = ConvertTo-Double $rows[$i].a
      $inputs[$i, 1] = ConvertTo-Double $rows[$i].b
    }
    $ws.Range('A1').Resize($count, 2).Value2 = $inputs
    for ($j = 0; $j -lt $formulas.Count; $j++) {
      $ws.Cells.Item(1, 3 + $j).Resize($count, 1).Formula = $formulas[$j]
    }
    $xl.Calculate()
    $stored = $ws.Range('A1').Resize($count, 2).Value2
    $results = $ws.Cells.Item(1, 3).Resize($count, $formulas.Count).Value2

    for ($i = 0; $i -lt $count; $i++) {
      $a = ConvertTo-Field $stored[($i + 1), 1]
      $b = ConvertTo-Field $stored[($i + 1), 2]
      $kept = if ($a -eq $rows[$i].a -and $b -eq $rows[$i].b) { '=' } else { "$a,$b" }
      $fields = @($rows[$i].id, $kind, $rows[$i].a, $rows[$i].b, $kept)
      for ($j = 1; $j -le $formulas.Count; $j++) {
        $value = if ($formulas.Count -eq 1 -and $count -eq 1) { $results } else { $results[($i + 1), $j] }
        $fields += ConvertTo-Field $value
      }
      $lines.Add($fields -join "`t")
    }
  }

  # Formulas typed as text.
  $rows = @($all | Where-Object { $_.kind -eq 'lit' })
  if ($rows.Count -gt 0) {
    $lines.Add('# lit: id, kind, formula, value')
    $ws = $wb.Worksheets.Add()
    # Operands for the formulas that refer to cells.
    $ws.Range('X1').Value2 = 0.1; $ws.Range('X2').Value2 = 0.2; $ws.Range('X3').Value2 = -0.3; $ws.Range('X4').Value2 = 0.3
    for ($i = 0; $i -lt $rows.Count; $i++) {
      $cell = $ws.Cells.Item($i + 1, 1)
      $cell.Formula = $rows[$i].note
      $lines.Add((@($rows[$i].id, 'lit', $rows[$i].note, (ConvertTo-Field $cell.Value2)) -join "`t"))
    }
  }
}
finally {
  if ($wb) { try { $wb.Close($false) } catch {} }
  if ($xl) { try { $xl.Quit() } catch {}; [void][Runtime.InteropServices.Marshal]::ReleaseComObject($xl) }
  [GC]::Collect(); [GC]::WaitForPendingFinalizers()
}

$target = [IO.Path]::GetFullPath((Join-Path (Get-Location) $Out))
[IO.File]::WriteAllText($target, (($lines -join "`n") + "`n"), (New-Object Text.UTF8Encoding $false))
"$($lines.Count) lines written to $target"
