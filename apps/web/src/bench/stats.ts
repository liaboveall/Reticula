/** What is reported of a set of samples. */
export interface Summary {
  readonly count: number;
  readonly min: number;
  readonly mean: number;
  readonly p50: number;
  readonly p95: number;
  readonly p99: number;
  readonly max: number;
}

export function summarize(samples: readonly number[]): Summary {
  const sorted = Float64Array.from(samples).sort();
  const count = sorted.length;
  // The smallest sample that at least this share of the samples do not exceed.
  const quantile = (share: number) =>
    sorted[Math.min(count - 1, Math.max(0, Math.ceil(share * count) - 1))] ?? 0;
  let sum = 0;
  for (const sample of sorted) {
    sum += sample;
  }
  return {
    count,
    min: sorted[0] ?? 0,
    mean: count > 0 ? sum / count : 0,
    p50: quantile(0.5),
    p95: quantile(0.95),
    p99: quantile(0.99),
    max: sorted[count - 1] ?? 0,
  };
}

/** A small seeded generator, so that every run asks for the same positions. */
export class Random {
  #state: number;

  constructor(seed: number) {
    this.#state = seed >>> 0;
  }

  /** The next 32 bits (mulberry32). */
  next(): number {
    this.#state = (this.#state + 0x6d2b79f5) >>> 0;
    let mixed = this.#state;
    mixed = Math.imul(mixed ^ (mixed >>> 15), mixed | 1);
    mixed ^= mixed + Math.imul(mixed ^ (mixed >>> 7), mixed | 61);
    return (mixed ^ (mixed >>> 14)) >>> 0;
  }

  /** A whole number from zero up to, not including, `bound`. */
  below(bound: number): number {
    return Math.floor((this.next() / 2 ** 32) * bound);
  }

  /** A number from zero up to, not including, one. */
  unit(): number {
    return this.next() / 2 ** 32;
  }
}

/** The smallest step the clock was seen to take, in milliseconds. */
export function timerResolution(): number {
  let resolution = Number.POSITIVE_INFINITY;
  for (let previous = performance.now(), seen = 0; seen < 200; ) {
    const now = performance.now();
    if (now > previous) {
      resolution = Math.min(resolution, now - previous);
      previous = now;
      seen++;
    }
  }
  return resolution;
}

/** Resolves at the start of the next frame with that frame's time. */
export function nextFrame(): Promise<number> {
  return new Promise((resolve) => requestAnimationFrame(resolve));
}

/** The time between frames, as the browser is delivering them right now. */
export async function frameInterval(frames = 120): Promise<Summary> {
  const intervals: number[] = [];
  let previous = await nextFrame();
  for (let frame = 0; frame < frames; frame++) {
    const now = await nextFrame();
    intervals.push(now - previous);
    previous = now;
  }
  return summarize(intervals);
}
