//! Floating-point functions whose results are identical on every target.
//!
//! The standard library leaves the precision of `sin`, `exp`, `powf` and
//! friends unspecified: they call into the platform's math library and differ
//! between operating systems, architectures and compiler releases. The engine
//! has to give the same answer in a browser, on a server and on a developer's
//! machine, so every such function is taken from a pure-Rust implementation
//! pinned to an exact version.
//!
//! This module is the only place in the workspace that calls that library.
//! Basic arithmetic (`+ - * /`) is exactly specified by IEEE 754 and is used
//! directly; fused multiply-add is never used.

macro_rules! unary {
    ($($(#[$doc:meta])+ $name:ident => $function:ident;)+) => {
        $(
            $(#[$doc])+
            #[inline]
            #[must_use]
            pub fn $name(x: f64) -> f64 {
                libm::$function(x)
            }
        )+
    };
}

unary! {
    /// Largest integer less than or equal to `x`.
    floor => floor;
    /// Smallest integer greater than or equal to `x`.
    ceil => ceil;
    /// Integer part of `x`, rounding toward zero.
    trunc => trunc;
    /// Nearest integer to `x`, with halfway cases rounded away from zero.
    round => round;
    /// Nearest integer to `x`, with halfway cases rounded to the even neighbour.
    round_ties_even => rint;
    /// Square root. Exactly rounded.
    sqrt => sqrt;
    /// Cube root.
    cbrt => cbrt;
    /// `e` raised to the power `x`.
    exp => exp;
    /// 2 raised to the power `x`.
    exp2 => exp2;
    /// `exp(x) - 1`, accurate near zero.
    exp_m1 => expm1;
    /// Natural logarithm.
    ln => log;
    /// `ln(1 + x)`, accurate near zero.
    ln_1p => log1p;
    /// Base-2 logarithm.
    log2 => log2;
    /// Base-10 logarithm.
    log10 => log10;
    /// Sine of an angle in radians.
    sin => sin;
    /// Cosine of an angle in radians.
    cos => cos;
    /// Tangent of an angle in radians.
    tan => tan;
    /// Arcsine, in radians.
    asin => asin;
    /// Arccosine, in radians.
    acos => acos;
    /// Arctangent, in radians.
    atan => atan;
    /// Hyperbolic sine.
    sinh => sinh;
    /// Hyperbolic cosine.
    cosh => cosh;
    /// Hyperbolic tangent.
    tanh => tanh;
    /// Inverse hyperbolic sine.
    asinh => asinh;
    /// Inverse hyperbolic cosine.
    acosh => acosh;
    /// Inverse hyperbolic tangent.
    atanh => atanh;
}

/// `x` raised to the power `y`.
#[inline]
#[must_use]
pub fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

/// Four-quadrant arctangent of `y / x`, in radians.
#[inline]
#[must_use]
pub fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

/// Length of the hypotenuse, `sqrt(x² + y²)`, without intermediate overflow.
#[inline]
#[must_use]
pub fn hypot(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}

/// Remainder of `x / y` with the sign of `x`. Exact.
#[inline]
#[must_use]
pub fn fmod(x: f64, y: f64) -> f64 {
    libm::fmod(x, y)
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[test]
    fn rounding_modes() {
        assert_eq!(floor(-1.5), -2.0);
        assert_eq!(ceil(-1.5), -1.0);
        assert_eq!(trunc(-1.5), -1.0);
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -3.0);
        assert_eq!(round_ties_even(2.5), 2.0);
        assert_eq!(round_ties_even(3.5), 4.0);
    }

    #[test]
    fn exact_cases() {
        assert_eq!(sqrt(9.0), 3.0);
        assert_eq!(cbrt(27.0), 3.0);
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(ln(1.0), 0.0);
        assert_eq!(log2(8.0), 3.0);
        assert_eq!(log10(1000.0), 3.0);
        assert_eq!(pow(2.0, 10.0), 1024.0);
        assert_eq!(hypot(3.0, 4.0), 5.0);
        assert_eq!(sin(0.0), 0.0);
        assert_eq!(cos(0.0), 1.0);
        assert_eq!(atan2(0.0, 1.0), 0.0);
    }

    #[test]
    fn fmod_is_the_exact_remainder() {
        // 0.1 is slightly above one tenth in binary, so it fits into 1 nine
        // times, not ten.
        assert_eq!(fmod(1.0, 0.1), 0.099_999_999_999_999_95);
        assert_eq!(fmod(-7.0, 3.0), -1.0);
        assert!(fmod(1.0, 0.0).is_nan());
    }

    #[test]
    fn domain_errors_are_nan() {
        assert!(sqrt(-1.0).is_nan());
        assert!(ln(-1.0).is_nan());
        assert!(asin(2.0).is_nan());
        assert_eq!(ln(0.0), f64::NEG_INFINITY);
    }
}
