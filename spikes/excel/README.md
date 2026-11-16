# What Excel actually does

The engine refers to rows and columns by stable identity rather than by
position, and it has to decide how numbers compare and round. Both choices are
measured against Excel, so these scripts ask the locally installed Excel
directly instead of relying on documentation or memory.

They drive a hidden Excel instance through its automation interface, use one
unsaved scratch workbook, never touch the clipboard, and close without saving.
They need Windows with Excel installed. Results are recorded together with the
Excel version and build, because behaviour changes between builds.

## Structural edits

```
powershell -File spikes/excel/probe-structure.ps1
```

Prints how formulas change when rows are inserted, deleted, cut, sorted and
filled, and when sheets are deleted. The recorded output is in
[results/](results/).

## Comparison, rounding and MOD

```
node spikes/excel/numeric-cases.mjs > spikes/excel/data/numeric-cases.tsv
powershell -File spikes/excel/probe-numeric.ps1 -Out spikes/excel/results/numeric.tsv
node spikes/excel/check-numeric.mjs spikes/excel/results/numeric.tsv
```

The first step generates about 23,000 cases on the boundaries where a
spreadsheet can differ from plain IEEE 754 arithmetic: pairs of numbers a few
units in the last place apart, values just below and above a decimal half, and
remainders with decimal divisors. Numbers travel as bit patterns, so no decimal
conversion stands between the case and the cell. The second step has Excel
evaluate them. The third checks candidate rules against what Excel returned.

What the recorded run (Excel 16.0, build 20711) shows:

| Operation | Rule that reproduces every case |
|---|---|
| `=`, `<>`, `<`, `>`, `<=`, `>=` | Round both sides to 15 significant decimal digits, then compare |
| `COUNTIF`, `SUMIF` criteria | The same rule as `=` |
| `MATCH(…, 0)`, `VLOOKUP(…, FALSE)` | Bit-for-bit equality. Two numbers that are `=` can fail to match |
| `ROUND`, `ROUNDDOWN`, `TRUNC`, `FLOOR`, `CEILING` | Round to 15 significant digits, then round in decimal |
| `ROUNDUP` | The same decision; the result is `ROUNDDOWN` plus one unit added in floating point, so it can be one unit in the last place off the nearest double |
| `INT` | Below 10^15 as above; from 10^15 on, the exact floor |
| `MOD` | The exact remainder with the sign of the divisor. No 15-digit rounding. `#NUM!` once the quotient reaches about 10^13 |
| `a + b`, `a - b` | Exact, except as the outermost operation of a formula, where a result about 2^50 times smaller than the operands becomes 0. Wrapping the expression in parentheses, or in any function, keeps the exact result |
| `SUM`, `AVERAGE`, `SUBTOTAL`, `AGGREGATE` | Return 0 for such a cancelled sum wherever they appear. `SUMPRODUCT` and `SUMIF` return the exact residue |

Subnormal numbers and negative zero written into a cell read back as 0.

Against the same data, plain IEEE comparison reproduces 72% of the equality
cases, and rounding the binary value directly reproduces 76% of the `ROUND`
cases.
