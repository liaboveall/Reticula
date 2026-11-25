"""Writes the Parquet files of this directory that Arrow writes, and the values
of those that DuckDB writes.

    python -I make.py write <directory>
    python -I make.py read <directory> <name>...
"""

import datetime as dt
import decimal
import os
import struct
import sys
import uuid

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

ROWS = 61
decimal.getcontext().prec = 100
EPOCH = dt.datetime(1970, 1, 1)


def some(values):
    """Every seventh value is missing, and so are ten in a row."""
    return [None if i % 7 == 3 or 40 <= i < 50 else v for i, v in enumerate(values)]


def types():
    n = range(ROWS)
    D = decimal.Decimal
    columns = {
        "truth": pa.array(some([i % 3 == 0 for i in n]), pa.bool_()),
        "i8": pa.array(some([i - 48 for i in n]), pa.int8()),
        "i16": pa.array(some([(i - 48) * 600 for i in n]), pa.int16()),
        "i32": pa.array(some([(i - 48) * 40_000_000 for i in n]), pa.int32()),
        "i64": pa.array(some([(i - 48) * 190_000_000_000_000_000 for i in n]), pa.int64()),
        "u8": pa.array(some([i * 2 for i in n]), pa.uint8()),
        "u16": pa.array(some([i * 650 for i in n]), pa.uint16()),
        "u32": pa.array(some([i * 44_000_000 for i in n]), pa.uint32()),
        "u64": pa.array(some([i * 90_000_000_000_000_000 for i in n]), pa.uint64()),
        "f32": pa.array(some([i / 10 for i in n]), pa.float32()),
        "f64": pa.array(
            some([[float("nan"), float("inf"), -0.0, 5e-324][i] if i < 4 else i / 7 for i in n]),
            pa.float64(),
        ),
        "dec9": pa.array(some([D(i - 48) / 4 for i in n]), pa.decimal128(9, 2)),
        "dec18": pa.array(some([D(i - 48) * D("2083333.3333333333") for i in n]), pa.decimal128(18, 10)),
        "dec38": pa.array(some([D(i - 48) * 10**30 for i in n]), pa.decimal128(38, 5)),
        "day": pa.array(some([dt.date(1582, 10, 1) + dt.timedelta(days=i * 1_700) for i in n]), pa.date32()),
        "ms": pa.array(some([EPOCH + dt.timedelta(days=(i - 48) * 999, milliseconds=i) for i in n]), pa.timestamp("ms")),
        "us": pa.array(some([EPOCH + dt.timedelta(days=(i - 48) * 999, microseconds=i) for i in n]), pa.timestamp("us")),
        "utc": pa.array(some([EPOCH + dt.timedelta(hours=i * 999) for i in n]), pa.timestamp("us", "UTC")),
        "time_ms": pa.array(some([dt.time(i % 24, i % 60, 59, (i % 3) * 1_000) for i in n]), pa.time32("ms")),
        "time_us": pa.array(some([dt.time(i % 24, i % 60, 59, i % 3) for i in n]), pa.time64("us")),
        "text": pa.array(some([["", "a\tb", "c\nd\\", "漢字", "😀", "plain"][i % 6] * (i % 4) for i in n]), pa.string()),
        "large": pa.array(some([f"value {i}" for i in n]), pa.large_string()),
        "words": pa.array(some([["north", "south", "east"][i % 3] for i in n])).dictionary_encode(),
        "id": pa.array(some([uuid.UUID(int=i * 0x1234567890ABCDEF1234567890).bytes for i in n]), pa.uuid()),
        "empty": pa.nulls(ROWS),
    }
    table = pa.table(columns)
    required = pa.array(list(n), pa.int32())
    field = pa.field("must", pa.int32(), nullable=False)
    return table.append_column(field, required)


def encodings():
    n = range(ROWS)
    D = decimal.Decimal
    columns = {
        "i32 delta": (some([i * i for i in n]), pa.int32(), "DELTA_BINARY_PACKED"),
        "i64 delta": (some([(i - 48) ** 5 * 10**9 for i in n]), pa.int64(), "DELTA_BINARY_PACKED"),
        "us delta": (some([EPOCH + dt.timedelta(seconds=i * 61) for i in n]), pa.timestamp("us"), "DELTA_BINARY_PACKED"),
        "i32 split": (some([i * 70_000 - 9 for i in n]), pa.int32(), "BYTE_STREAM_SPLIT"),
        "i64 split": (some([i * 70_000 - 9 for i in n]), pa.int64(), "BYTE_STREAM_SPLIT"),
        "f32 split": (some([i / 3 for i in n]), pa.float32(), "BYTE_STREAM_SPLIT"),
        "f64 split": (some([i / 3 for i in n]), pa.float64(), "BYTE_STREAM_SPLIT"),
        "dec split": (some([D(i) / 8 for i in n]), pa.decimal128(17, 3), "BYTE_STREAM_SPLIT"),
        "text lengths": (some([f"{'x' * (i % 5)}{i}" for i in n]), pa.string(), "DELTA_LENGTH_BYTE_ARRAY"),
        "text parts": (some([f"https://example.org/{i // 9}/{i}" for i in n]), pa.string(), "DELTA_BYTE_ARRAY"),
        "id parts": (some([uuid.UUID(int=(i << 100) | 7).bytes for i in n]), pa.uuid(), "DELTA_BYTE_ARRAY"),
        "truth runs": (some([i < 30 for i in n]), pa.bool_(), "RLE"),
        "half": (some([np.float16(i / 4 - 3) for i in n]), pa.float16(), "PLAIN"),
        "half split": (some([np.float16(i / 4 - 3) for i in n]), pa.float16(), "BYTE_STREAM_SPLIT"),
        "text plain": (some([f"{'y' * (i % 5)}{i}" for i in n]), pa.string(), "PLAIN"),
    }
    return columns


def put_encodings(put, name, columns, **options):
    table = pa.table({name: pa.array(values, kind) for name, (values, kind, _) in columns.items()})
    chosen = {name: encoding for name, (_, _, encoding) in columns.items()}
    put(name, table, use_dictionary=False, column_encoding=chosen, **options)


def write(directory):
    def put(name, table, **options):
        path = os.path.join(directory, name + ".parquet")
        pq.write_table(table, path, **{"compression": "none", "store_schema": False, **options})

    table = types()
    put("arrow-old", table, version="1.0", data_page_version="1.0", use_dictionary=False, coerce_timestamps="us", allow_truncated_timestamps=True)
    put("arrow-snappy", table, version="2.6", compression="snappy")
    put("arrow-gzip", table, version="2.6", compression="gzip", data_page_version="2.0")
    put("arrow-zstd", table, version="2.6", compression="zstd", row_group_size=40)
    put("arrow-lz4", table, version="2.6", compression="lz4", data_page_version="2.0", row_group_size=50)
    put("arrow-checksums", table, version="2.4", compression="snappy", write_page_checksum=True, data_page_size=64, write_batch_size=8)

    # DuckDB reads split streams of floating-point numbers only, and texts in
    # parts but no other bytes.
    split = {"i32 split", "i64 split", "dec split", "half split", "id parts"}
    known = {name: column for name, column in encodings().items() if name not in split}
    others = {name: column for name, column in encodings().items() if name in split}
    put_encodings(put, "arrow-encodings", known, data_page_version="2.0")
    put_encodings(put, "arrow-encodings-zstd", known, compression="zstd", data_page_size=100, write_batch_size=16)
    put_encodings(put, "arrow-split", others, compression="snappy", data_page_version="2.0")

    n = range(ROWS)
    D = decimal.Decimal
    put("arrow-int96", pa.table({"at": pa.array(some([EPOCH + dt.timedelta(days=i * 200, microseconds=i) for i in n]), pa.timestamp("us"))}), use_deprecated_int96_timestamps=True)
    nanos = [(i - 48) * 10**15 + (i % 4) * 250 for i in n]
    put("arrow-nanos", pa.table({
        "at": pa.array(some(nanos), pa.timestamp("ns")),
        "time": pa.array(some([i * 10**11 + (i % 3) * 50 for i in n]), pa.time64("ns")),
    }), version="2.6")
    put("arrow-decimals", pa.table({
        "dec9": pa.array(some([D(i - 48) / 8 for i in n]), pa.decimal32(9, 3)),
        "dec18": pa.array(some([D(i - 48) / 8 for i in n]), pa.decimal64(18, 4)),
        "dec19": pa.array(some([D(i - 48) / 8 for i in n]), pa.decimal128(19, 4)),
    }), store_decimal_as_integer=True)
    put("arrow-digits", pa.table({
        "dec76": pa.array(some([D(i - 48).scaleb(69) + D("0.00001") for i in n]), pa.decimal256(76, 5)),
    }))
    put("arrow-unsigned", pa.table({
        "u64": pa.array(some([2**64 - 1 - i for i in n]), pa.uint64()),
        "bytes": pa.array(some([b"\xff\x00" if i == 55 else b"a" for i in n]), pa.binary()),
        "text": pa.array(some([f"{i}".encode() for i in n]), pa.binary()),
    }))
    put("arrow-nested", pa.table({
        "before": pa.array(list(n), pa.int64()),
        "list": pa.array([[i, None] if i % 2 else None for i in n], pa.list_(pa.int32())),
        "struct": pa.array([{"a": i, "b": {"c": "x"}} for i in n]),
        "map": pa.array([[("k", i)] for i in n], pa.map_(pa.string(), pa.int32())),
        "after": pa.array([f"{i}" for i in n]),
    }))
    put("arrow-empty", types().slice(0, 0))
    words = [f"one of some {i % 40} words" for i in range(300)]
    put("arrow-fallback", pa.table({"word": words}), dictionary_pagesize_limit=400, data_page_size=300, write_batch_size=10, row_group_size=190)


def cell(value, kind):
    """What a cell shows, as the tests write it."""
    if value is None:
        return ""
    if pa.types.is_floating(kind):
        return "" if value != value or value in (float("inf"), float("-inf")) else repr(float(value))
    if pa.types.is_boolean(kind):
        return "TRUE" if value else "FALSE"
    if pa.types.is_timestamp(kind):
        # Counted in microseconds: the dates of Python end too early.
        day, micros = divmod(value, 86_400_000_000)
        seconds, micros = divmod(micros, 1_000_000)
        date = np.datetime64(int(day), "D")
        if not np.datetime64("0001-01-01") <= date <= np.datetime64("9999-12-31"):
            return ""
        return f"{date} {seconds // 3600:02}:{seconds // 60 % 60:02}:{seconds % 60:02}{threes(micros * 1_000)}"
    if pa.types.is_time(kind):
        nanos = value
        seconds, nanos = divmod(nanos, 10**9)
        return f"{seconds // 3600:02}:{seconds // 60 % 60:02}:{seconds % 60:02}{threes(nanos)}"
    if pa.types.is_decimal(kind):
        return f"{value:f}"
    text = str(value)
    return text.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n").replace("\r", "\\r")


def threes(nanos):
    if nanos == 0:
        return ""
    digits = f"{nanos:09}"
    while digits.endswith("000"):
        digits = digits[:-3]
    return "." + digits


def flat(kind):
    bytes_only = pa.types.is_binary(kind) or pa.types.is_fixed_size_binary(kind)
    return not pa.types.is_nested(kind) and not bytes_only


def read(directory, name):
    """Writes the values of a file, as Arrow reads them."""
    table = pq.read_table(os.path.join(directory, name + ".parquet"))
    names, columns = [], []
    for field, column in zip(table.schema, table.columns):
        kind = field.type
        if not flat(kind):
            continue
        if pa.types.is_timestamp(kind) and kind.unit == "ns":
            # Arrow cuts off towards zero, which is the wrong way before 1970.
            values = [None if v is None else v // 1_000 for v in column.cast(pa.int64()).to_pylist()]
        elif pa.types.is_timestamp(kind):
            values = column.cast(pa.timestamp("us", kind.tz)).cast(pa.int64()).to_pylist()
        elif pa.types.is_time(kind):
            values = column.cast(pa.time64("ns")).cast(pa.int64()).to_pylist()
        elif pa.types.is_date(kind):
            days = column.cast(pa.int32()).to_pylist()
            first, last = -719_162, 2_932_896
            values = [None if d is None or not first <= d <= last else np.datetime64(d, "D") for d in days]
        else:
            values = column.to_pylist()
        names.append(field.name)
        columns.append([cell(value, kind) for value in values])
    with open(os.path.join(directory, name + ".tsv"), "w", encoding="utf-8", newline="\n") as out:
        out.write("\t".join(names) + "\n")
        for row in zip(*columns):
            out.write("\t".join(row) + "\n")


if __name__ == "__main__":
    if sys.argv[1] == "write":
        write(sys.argv[2])
    else:
        for each in sys.argv[3:]:
            read(sys.argv[2], each)
