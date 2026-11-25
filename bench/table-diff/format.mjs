// What cells show and what sums come to, worked out again without the engine.

const pad = (number, width) => String(number).padStart(width, "0");

function civil(days) {
  return new Date(Number(days) * 86_400_000).toISOString().slice(0, 10);
}

/** A whole number divided by ten to the power of `scale`. */
export function decimal(mantissa, scale, fixed) {
  let digits = pad(mantissa < 0n ? -mantissa : mantissa, scale + 1);
  const whole = digits.slice(0, digits.length - scale);
  digits = digits.slice(digits.length - scale);
  if (!fixed) digits = digits.replace(/0+$/, "");
  return `${mantissa < 0n ? "-" : ""}${whole}${digits === "" ? "" : `.${digits}`}`;
}

/** What a cell of a column that is stored as integers shows. */
export function shown(kind, value) {
  switch (kind.type) {
    case "decimal":
      return decimal(value, kind.scale, kind.fixed);
    case "date":
      return civil(value);
    case "datetime": {
      const day = 86_400_000_000n;
      const ofDay = ((value % day) + day) % day;
      const seconds = ofDay / 1_000_000n;
      const micros = ofDay % 1_000_000n;
      const clock = [seconds / 3600n, (seconds / 60n) % 60n, seconds % 60n].map((n) => pad(n, 2));
      const rest =
        micros === 0n
          ? ""
          : micros % 1000n === 0n
            ? `.${pad(micros / 1000n, 3)}`
            : `.${pad(micros, 6)}`;
      return `${civil((value - ofDay) / day)} ${clock.join(":")}${rest}`;
    }
    case "boolean":
      return value === 0n ? "FALSE" : "TRUE";
    default:
      return String(value);
  }
}

/** A finite floating-point number as a whole number times two to a power. */
function parts(number) {
  const view = new DataView(new ArrayBuffer(8));
  view.setFloat64(0, number);
  const bits = view.getBigUint64(0);
  const exponent = Number((bits >> 52n) & 0x7ffn);
  const fraction = bits & 0xfffffffffffffn;
  const mantissa = exponent === 0 ? fraction : fraction | (1n << 52n);
  return [bits >> 63n ? -mantissa : mantissa, Math.max(exponent, 1) - 1075];
}

/** The sum of numbers, and that of their sizes, exactly: times two to the power of 1,074. */
export function exactSum(numbers) {
  const sums = new Map();
  for (const number of numbers) {
    const [mantissa, exponent] = parts(number);
    const of = sums.get(exponent) ?? [0n, 0n];
    of[0] += mantissa;
    of[1] += mantissa < 0n ? -mantissa : mantissa;
    sums.set(exponent, of);
  }
  let sum = 0n;
  let size = 0n;
  for (const [exponent, of] of sums) {
    sum += of[0] << BigInt(exponent + 1074);
    size += of[1] << BigInt(exponent + 1074);
  }
  return { sum, size };
}

const abs = (number) => (number < 0n ? -number : number);

/** A decimal number as a fraction. */
export function fraction(text) {
  const [, sign, whole, rest = "", exponent = "0"] =
    /^(-?)(\d+)(?:\.(\d*))?(?:[eE]([+-]?\d+))?$/.exec(text);
  const places = BigInt(rest.length) - BigInt(exponent);
  const digits = BigInt(`${sign}${whole}${rest}`);
  return places >= 0n ? [digits, 10n ** places] : [digits * 10n ** -places, 1n];
}

/**
 * Whether a text of fifteen digits shows a sum, divided by `by`. The sum may be
 * wrong by two units of rounding, and by the square of one for every number
 * that was added to every other one: 2^-52 of it, and n^2 2^-105 of the sizes.
 */
export function isSum(text, { sum, size }, count, by) {
  const [numerator, denominator] = fraction(text);
  const apart = abs((numerator << 1074n) * by - sum * denominator);
  const all = abs(sum) * denominator;
  const n = BigInt(count);
  // Fifteen digits, of which the last one is rounded.
  const digits = (all << 105n) / (2n * 10n ** 14n);
  return apart << 105n <= digits + (all << 53n) + n * n * size * denominator;
}

/** Whether a text shows the mean of whole numbers, and it is rightly said to be exact. */
export function isMean(figure, sum, count, scale) {
  const [numerator, denominator] = fraction(figure.text);
  const by = BigInt(count) * 10n ** BigInt(scale);
  const apart = abs(numerator * by - sum * denominator);
  if (figure.exact !== (apart === 0n)) return false;
  // Half of a unit of the last place that is shown, and of fifteen digits.
  const places = figure.text.includes(".") ? figure.text.split(".")[1].length : 0;
  if (2n * apart * 10n ** BigInt(places) > by * denominator) return false;
  return figure.exact || 2n * apart * 10n ** 14n <= abs(sum) * denominator;
}

/** Whether a text of fifteen digits shows a number. */
export function isShown(text, number) {
  const read = Number(text);
  return text !== "" && Math.abs(read - number) <= 5.1e-15 * Math.abs(number) + 5e-324;
}

/**
 * Whether a number as it is written comes back from the floating-point number
 * nearest to it: whether its digits are as few as mean that number, and as
 * close to it as so many can be. Either of two that are as close will do.
 */
export function comesBack(text) {
  const digits = (written) =>
    written
      .split(/[eE]/)[0]
      .replace(/[-.]/g, "")
      .replace(/^0+|0+$/g, "");
  const given = digits(text);
  const number = Number(text);
  if (given === "") return true;
  const shortest = number.toExponential();
  if (!Number.isFinite(number) || number === 0) return false;
  if (given === digits(shortest)) return true;
  if (given.length !== digits(shortest).length) return false;
  const distance = (written) => {
    const [numerator, denominator] = fraction(written);
    return [abs((numerator << 1074n) - exactSum([number]).sum * denominator), denominator];
  };
  const [a, ofA] = distance(text);
  const [b, ofB] = distance(shortest);
  return a * ofB === b * ofA;
}

export const compareBytes = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));