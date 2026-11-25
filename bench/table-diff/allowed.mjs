// What DuckDB and tables are known to see differently. Anything else that
// differs makes a run fail.

/** Differences of the types that are found out from delimited text. `holds` is a
 * condition, in SQL, that has to hold for one of the texts of the column, and
 * `anywhere` one that has to hold for one of any column. `loses` means that one
 * of them does not come back from the floating-point number nearest to it. */
export const TYPE_DIFFERENCES = [
  {
    ours: ["decimal"],
    theirs: ["DOUBLE"],
    reason: "numbers with a point are exact decimals here and DOUBLE there",
  },
  {
    ours: ["text"],
    theirs: ["BIGINT", "DOUBLE", "HUGEINT"],
    holds: "regexp_matches(c, '^[+]|^-?0[0-9]|^-0+([.]0*)?$')",
    reason: "numbers with a plus sign, zeros in front or a negative zero are text here",
  },
  {
    ours: ["text"],
    theirs: ["BIGINT", "DOUBLE", "HUGEINT"],
    // Found out again, without the engine.
    loses: true,
    reason: "numbers that lose digits as a DOUBLE are text here",
  },
  {
    ours: ["text"],
    theirs: ["BIGINT", "DOUBLE", "BOOLEAN", "DATE", "TIMESTAMP"],
    holds: "c <> trim(c)",
    reason: "spaces around a value make it a text here",
  },
  {
    ours: ["text"],
    theirs: ["DOUBLE"],
    holds: "lower(c) IN ('nan', 'inf', 'infinity')",
    reason: "NaN is text here",
  },
  {
    ours: ["text"],
    theirs: ["TIME"],
    reason: "a time of day without a date is text here",
  },
  {
    ours: ["text"],
    theirs: ["TIMESTAMP WITH TIME ZONE"],
    holds: "regexp_matches(c, '[+-][0-9][0-9]:[0-9][0-9]$')",
    reason: "a time with an offset is text here",
  },
  {
    ours: ["datetime"],
    theirs: ["TIMESTAMP WITH TIME ZONE"],
    anywhere: "regexp_matches(c, '^[0-9].*(Z|[+-][0-9][0-9]:[0-9][0-9])$')",
    reason: "a time with a Z is one without a time zone here",
  },
  {
    ours: ["date", "datetime"],
    theirs: ["VARCHAR"],
    holds: "regexp_matches(c, '/')",
    reason: "dates may be written in two ways in the same column here",
  },
  {
    ours: ["date"],
    theirs: ["TIMESTAMP", "VARCHAR"],
    anywhere: "regexp_matches(c, '^[0-9]{4}/[0-9]+/[0-9]+')",
    reason: "how dates are written is found out for every column by itself here",
  },
  {
    ours: ["datetime"],
    theirs: ["VARCHAR"],
    holds: "regexp_matches(c, '^[0-9-]+$')",
    reason: "dates and times may be in the same column here",
  },
];

/** Texts whose lower case has no simple rule: DuckDB folds one character at a
 * time. They have a capital I with a dot, or a capital sigma. */
export const FOLDING = {
  pattern: /[\u{130}\u{3a3}]/u,
  reason: "the dotted capital I and the sigma at the end of a word are folded as Unicode says here",
};

/** More different values than these are not all counted for a list. */
export const LISTED = {
  values: 1 << 20,
  bytes: 64 << 20,
  reason: "a list of values counts the first 1,048,576 of them or 64 MiB, and says so",
};

/** Views that were refused, of tables that were made for that. */
export const REFUSED = {
  reason: "long texts outside of a dictionary that begin alike need not be sorted",
};

/** Sums of floating-point numbers: each is compared with the exact sum. */
export const FLOAT_SUM = {
  reason:
    "a sum is compensated here: within two units of rounding of the exact one, which fsum need not be",
};