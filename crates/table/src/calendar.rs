//! Days and microseconds of the proleptic Gregorian calendar.

/// Microseconds of a day.
pub const DAY_MICROS: i64 = 86_400_000_000;
/// 0001-01-01, the first day that can be written.
pub const FIRST_DAY: i64 = -719_162;
/// 9999-12-31, the last day that can be written.
pub const LAST_DAY: i64 = 2_932_896;

/// Days since 1970-01-01 of a date. `None` when there is no such date, or it
/// is before the year 1 or after the year 9999.
#[must_use]
pub fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day < 1 {
        return None;
    }
    if day > days_in_month(year, month) {
        return None;
    }
    // Years counted from March, so that the leap day comes last.
    let y = if month <= 2 {
        year.wrapping_sub(1)
    } else {
        year
    };
    let era = y.div_euclid(400);
    let of_era = y.rem_euclid(400);
    let shifted = if month > 2 {
        month.wrapping_sub(3)
    } else {
        month.wrapping_add(9)
    };
    let of_year = shifted
        .wrapping_mul(153)
        .wrapping_add(2)
        .div_euclid(5)
        .wrapping_add(day)
        .wrapping_sub(1);
    let of_era_days = of_era
        .wrapping_mul(365)
        .wrapping_add(of_era.div_euclid(4))
        .wrapping_sub(of_era.div_euclid(100))
        .wrapping_add(of_year);
    Some(
        era.wrapping_mul(146_097)
            .wrapping_add(of_era_days)
            .wrapping_sub(719_468),
    )
}

/// Days of a month of 1 to 12.
#[must_use]
pub const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 => {
            let leap =
                year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0);
            if leap { 29 } else { 28 }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Year, month and day of a number of days since 1970-01-01.
#[must_use]
pub const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let of_era = z.rem_euclid(146_097);
    let year_of_era = of_era
        .wrapping_sub(of_era.div_euclid(1_460))
        .wrapping_add(of_era.div_euclid(36_524))
        .wrapping_sub(of_era.div_euclid(146_096))
        .div_euclid(365);
    let of_year = of_era.wrapping_sub(
        year_of_era
            .wrapping_mul(365)
            .wrapping_add(year_of_era.div_euclid(4))
            .wrapping_sub(year_of_era.div_euclid(100)),
    );
    let shifted = of_year.wrapping_mul(5).wrapping_add(2).div_euclid(153);
    let day = of_year
        .wrapping_sub(shifted.wrapping_mul(153).wrapping_add(2).div_euclid(5))
        .wrapping_add(1);
    let month = if shifted < 10 {
        shifted.wrapping_add(3)
    } else {
        shifted.wrapping_sub(9)
    };
    let year = year_of_era
        .wrapping_add(era.saturating_mul(400))
        .wrapping_add(if month <= 2 { 1 } else { 0 });
    (year, month, day)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::unwrap_used, reason = "tests")]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[test]
    fn every_day_comes_back() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(1, 1, 1), Some(FIRST_DAY));
        assert_eq!(days_from_civil(9999, 12, 31), Some(LAST_DAY));
        assert_eq!(days_from_civil(2000, 2, 29), Some(11_016));
        let mut expected = (1_i64, 1_i64, 1_i64);
        for days in FIRST_DAY..=LAST_DAY {
            assert_eq!(civil_from_days(days), expected);
            if days % 97 == 0 {
                let (y, m, d) = expected;
                assert_eq!(days_from_civil(y, m, d), Some(days));
            }
            expected.2 += 1;
            if expected.2 > days_in_month(expected.0, expected.1) {
                expected.2 = 1;
                expected.1 += 1;
                if expected.1 > 12 {
                    expected.1 = 1;
                    expected.0 += 1;
                }
            }
        }
    }

    #[test]
    fn dates_that_do_not_exist() {
        for (y, m, d) in [
            (0, 1, 1),
            (10_000, 1, 1),
            (2023, 0, 1),
            (2023, 13, 1),
            (2023, 2, 29),
            (1900, 2, 29),
            (2023, 4, 31),
            (2023, 1, 0),
            (2023, 1, 32),
            (i64::MAX, 1, 1),
            (2023, i64::MIN, 1),
            (2023, 1, i64::MAX),
        ] {
            assert_eq!(days_from_civil(y, m, d), None);
        }
    }

    #[test]
    fn any_number_of_days_is_survived() {
        for days in [i64::MIN, i64::MAX, 0, -1] {
            let (_, month, day) = civil_from_days(days);
            assert!((1..=12).contains(&month) && (1..=31).contains(&day));
        }
    }
}
