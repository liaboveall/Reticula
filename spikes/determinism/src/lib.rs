//! Experiments behind three rules of the engine kernel. Not part of the engine.
//!
//! 1. *Never iterate a hash table where the order can reach a result.* The
//!    experiment fills `hashbrown` tables with the same keys and the same
//!    unseeded hasher on every target and digests the iteration order.
//! 2. *Never use a library's default generator for anything reproducible.* The
//!    experiment prints the first outputs of `rand`'s `SmallRng` for one seed.
//! 3. *Kernel crates are `no_std`, so rounding goes through `libm`.* The
//!    experiment measures what that costs natively and exposes the functions so
//!    the WebAssembly output can be inspected.
//!
//! `cargo xtask spike determinism` prints the report for the native target and
//! for WebAssembly. The interesting part is comparing reports across targets.

use core::hash::{BuildHasherDefault, Hash, Hasher};
use std::fmt::Write as _;

use hashbrown::HashSet;
use rand::rngs::SmallRng;
use rand::{Rng as _, SeedableRng as _};
use reticula_core::digest::Digest64;

/// FNV-1a. It has no per-process seed and does nothing target-specific, so any
/// difference in iteration order comes from the table, or from how a key type
/// feeds its bytes to the hasher.
struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xCBF2_9CE4_8422_2325)
    }
}

impl Hasher for Fnv1a {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

type FixedHashSet<K> = HashSet<K, BuildHasherDefault<Fnv1a>>;

/// Inserts `0..count` and returns a digest of the order iteration yields them in.
fn iteration_order<K>(count: u32, key: impl Fn(u32) -> K, value: impl Fn(&K) -> u64) -> u64
where
    K: Hash + Eq,
{
    let mut set = FixedHashSet::default();
    for index in 0..count {
        set.insert(key(index));
    }
    let mut digest = Digest64::new();
    for entry in &set {
        digest.write_u64(value(entry));
    }
    digest.finish()
}

/// The part of the report that should be compared across targets.
#[must_use]
pub fn report() -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "target: arch={} pointer_width={}",
        std::env::consts::ARCH,
        usize::BITS
    );

    // u64 keys hash identically everywhere, which isolates the table layout.
    for count in [12, 100, 1000, 100_000] {
        let order = iteration_order(count, u64::from, |key| *key);
        let _ = writeln!(out, "hash order, u64 keys,   n={count:<6} {order:016x}");
    }
    // usize keys also feed a different number of bytes to the hasher on 32-bit targets.
    for count in [12, 100, 1000, 100_000] {
        let order = iteration_order(count, |index| index as usize, |key| *key as u64);
        let _ = writeln!(out, "hash order, usize keys, n={count:<6} {order:016x}");
    }

    let mut small = SmallRng::seed_from_u64(42);
    let outputs: Vec<String> = (0..4)
        .map(|_| format!("{:016x}", small.next_u64()))
        .collect();
    let _ = writeln!(out, "SmallRng seed 42: {}", outputs.join(" "));
    out
}

/// Nanoseconds per call of the `libm` rounding functions and their `std`
/// counterparts. A quick reading to judge the cost of `no_std`, not a benchmark.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn rounding_cost() -> String {
    use std::hint::black_box;
    use std::time::Instant;

    use reticula_core::rng::Rng;

    const COUNT: usize = 1 << 22;
    const ROUNDS: u32 = 5;

    let mut rng = Rng::from_seed(7);
    let inputs: Vec<f64> = (0..COUNT).map(|_| rng.next_f64() * 2.0e6).collect();

    let time = |function: fn(f64) -> f64| {
        let mut best = f64::INFINITY;
        for _ in 0..ROUNDS {
            let start = Instant::now();
            let mut sum = 0.0;
            for input in &inputs {
                sum += function(black_box(*input));
            }
            black_box(sum);
            best = best.min(start.elapsed().as_secs_f64() * 1.0e9 / COUNT as f64);
        }
        best
    };

    type Unary = fn(f64) -> f64;
    let cases: [(&str, Unary, Unary); 4] = [
        ("floor", libm::floor, f64::floor),
        ("ceil", libm::ceil, f64::ceil),
        ("trunc", libm::trunc, f64::trunc),
        ("sqrt", libm::sqrt, f64::sqrt),
    ];
    let mut out = String::from("ns per call, best of 5      libm       std\n");
    for (name, from_libm, from_std) in cases {
        let _ = writeln!(
            out,
            "{name:<24} {:>8.2}  {:>8.2}",
            time(from_libm),
            time(from_std)
        );
    }
    out
}

/// Truncation through an integer round trip: two instructions on WebAssembly.
///
/// Exact for every input: values of magnitude 2^52 or more are already
/// integers, NaN and the infinities are returned unchanged, and the sign of
/// zero is preserved.
#[must_use]
pub fn trunc_by_cast(x: f64) -> f64 {
    const INTEGERS_ONLY: f64 = 4_503_599_627_370_496.0; // 2^52
    if x.abs() < INTEGERS_ONLY {
        ((x as i64) as f64).copysign(x)
    } else {
        x
    }
}

/// Floor built on [`trunc_by_cast`].
#[must_use]
pub fn floor_by_cast(x: f64) -> f64 {
    let truncated = trunc_by_cast(x);
    if truncated > x {
        truncated - 1.0
    } else {
        truncated
    }
}

/// Loops that run entirely inside the module, so that a timing from the host
/// measures the function and not the cost of calling into WebAssembly. Also
/// exported unmangled so the compiled code can be disassembled.
#[cfg(target_arch = "wasm32")]
pub mod exported {
    use super::{floor_by_cast, trunc_by_cast};

    fn sum_of(count: u32, function: impl Fn(f64) -> f64) -> f64 {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut sum = 0.0;
        for _ in 0..count {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let input = (state >> 11) as f64 * (2.0e6 / 9_007_199_254_740_992.0) - 1.0e6;
            sum += function(input);
        }
        sum
    }

    macro_rules! export_loop {
        ($($name:ident => $function:expr;)+) => {
            $(
                #[expect(unsafe_code, reason = "an unmangled export the host can call")]
                #[unsafe(no_mangle)]
                pub extern "C" fn $name(count: u32) -> f64 {
                    sum_of(count, $function)
                }
            )+
        };
    }

    export_loop! {
        spike_baseline => |x| x;
        spike_floor_libm => libm::floor;
        spike_floor_std => f64::floor;
        spike_floor_cast => floor_by_cast;
        spike_trunc_libm => libm::trunc;
        spike_trunc_std => f64::trunc;
        spike_trunc_cast => trunc_by_cast;
        spike_round_libm => libm::round;
        spike_round_std => f64::round;
        spike_sqrt_libm => |x| libm::sqrt(x.abs());
        spike_sqrt_std => |x| x.abs().sqrt();
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{floor_by_cast, trunc_by_cast};

    fn same(a: f64, b: f64) -> bool {
        a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
    }

    #[test]
    fn cast_rounding_matches_libm_bit_for_bit() {
        let mut rng = reticula_core::rng::Rng::from_seed(11);
        let special = [
            0.0,
            -0.0,
            0.5,
            -0.5,
            1.0,
            -1.0,
            0.999_999_999_999_999_9,
            -0.999_999_999_999_999_9,
            4_503_599_627_370_495.5,
            -4_503_599_627_370_495.5,
            4_503_599_627_370_496.0,
            9_007_199_254_740_993.0,
            1.0e300,
            -1.0e300,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::MAX,
            f64::MIN,
        ];
        let random = (0..200_000).flat_map(|_| {
            let unit = rng.next_f64() - 0.5;
            let any = f64::from_bits(rng.next_u64());
            [unit, unit * 10.0, unit * 1.0e6, unit * 1.0e16, any]
        });
        for x in special.into_iter().chain(random.collect::<Vec<_>>()) {
            assert!(same(trunc_by_cast(x), libm::trunc(x)), "trunc({x:e})");
            assert!(same(floor_by_cast(x), libm::floor(x)), "floor({x:e})");
        }
    }
}
