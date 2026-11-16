// Times the loops exported by the release build of this crate, inside the
// WebAssembly engine of the Node that runs it.
//
//   node time-wasm.mjs path/to/spike_determinism.wasm
import { readFileSync } from "node:fs";

const COUNT = 20_000_000;
const ROUNDS = 5;

const module = new WebAssembly.Module(readFileSync(process.argv[2]));
const { exports } = new WebAssembly.Instance(module, {});

const nanosecondsPerCall = (loop) => {
  let best = Number.POSITIVE_INFINITY;
  for (let round = 0; round < ROUNDS; round++) {
    const start = performance.now();
    loop(COUNT);
    best = Math.min(best, performance.now() - start);
  }
  return (best * 1e6) / COUNT;
};

const loops = Object.keys(exports)
  .filter((name) => name.startsWith("spike_") && !name.startsWith("spike_libm_"))
  .sort();
for (const name of loops) exports[name](COUNT / 10); // warm up

const overhead = nanosecondsPerCall(exports.spike_baseline);
console.log(`node ${process.version}, ns per call, best of ${ROUNDS}`);
console.log(`(loop overhead of ${overhead.toFixed(2)} ns subtracted)`);
for (const name of loops) {
  if (name === "spike_baseline") continue;
  const cost = nanosecondsPerCall(exports[name]) - overhead;
  console.log(`${name.replace("spike_", "").padEnd(14)}${cost.toFixed(2).padStart(8)}`);
}
