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

## Moving rows

```
powershell -File spikes/excel/probe-clipboard.ps1
```

Moves whole rows with cut and "Insert Cut Cells" and prints what happens to a
range and to single-cell references. This is the one probe that uses the
clipboard: Excel offers the operation no other way. The clipboard's text is
saved and restored; anything else on it is lost.

What the recorded run shows, for a range such as `SUM(A5:A9)`:

- A single-cell reference always follows its row.
- When the first or last row of the range moves *outward* (the last row further
  down, the first row further up), the range stretches to its new position and
  takes in the rows in between.
- When it moves anywhere else (to another place inside the range, or past the
  other end), the range does not follow it. It keeps the rows it still spans,
  as if the row had been deleted and then inserted at its destination.
- When the whole range moves, the reference moves with it.

So a range is not simply "the rows between two marked rows": dragging the last
row of a summed range up by two positions leaves the sum unchanged.

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
| `a - b` | Exact, except as the outermost operation of a formula: there the result is 0 when it is smaller than 2^(exponent of `a` − 49) and not subnormal. Wrapping the expression in parentheses, or in any function, keeps the exact result |
| `SUM`, `AVERAGE`, `SUBTOTAL`, `AGGREGATE` | Apply the same test to their last addition only, wherever they appear: `SUM(0.1, 0.2, -0.3)` is 0, `SUM(0.1, 0.2, -0.3, 0)` is 5.55e-17. `SUMPRODUCT` and `SUMIF` return the exact residue |

Subnormal numbers and negative zero written into a cell read back as 0.

Against the same data, plain IEEE comparison reproduces 72% of the equality
cases, and rounding the binary value directly reproduces 76% of the `ROUND`
cases.
