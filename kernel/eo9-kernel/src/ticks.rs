//! Pure arithmetic on generic-timer ticks.
//!
//! Kept free of any hardware access so it compiles — and its unit tests run — on the host
//! triple as well as on bare metal.

/// Convert a tick count of a counter running at `frequency_hz` into microseconds.
///
/// Uses 128-bit intermediate math so it cannot overflow for any realistic counter value
/// (the architected counter is 64-bit and typically runs at tens of MHz).
pub fn ticks_to_us(ticks: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return 0;
    }
    (u128::from(ticks) * 1_000_000 / u128::from(frequency_hz)) as u64
}

/// Convert a tick count of a counter running at `frequency_hz` into nanoseconds.
pub fn ticks_to_ns(ticks: u64, frequency_hz: u64) -> u64 {
    if frequency_hz == 0 {
        return 0;
    }
    (u128::from(ticks) * 1_000_000_000 / u128::from(frequency_hz)) as u64
}

#[cfg(test)]
mod tests {
    use super::{ticks_to_ns, ticks_to_us};

    #[test]
    fn nanosecond_conversion_matches_the_frequency() {
        // One full second of ticks at 62.5 MHz and at the 1 GHz QEMU `virt` frequency.
        assert_eq!(ticks_to_ns(62_500_000, 62_500_000), 1_000_000_000);
        assert_eq!(ticks_to_ns(1_000_000_000, 1_000_000_000), 1_000_000_000);
        // A single tick at 62.5 MHz is 16 ns; zero frequency stays defined.
        assert_eq!(ticks_to_ns(1, 62_500_000), 16);
        assert_eq!(ticks_to_ns(12345, 0), 0);
    }

    #[test]
    fn one_second_of_ticks_is_a_million_microseconds() {
        assert_eq!(ticks_to_us(62_500_000, 62_500_000), 1_000_000);
        assert_eq!(ticks_to_us(24_000_000, 24_000_000), 1_000_000);
    }

    #[test]
    fn fractional_and_large_values() {
        // 10 ms at 62.5 MHz (the QEMU virt default frequency).
        assert_eq!(ticks_to_us(625_000, 62_500_000), 10_000);
        // A counter value far beyond what u64 microsecond math would tolerate naively.
        assert_eq!(ticks_to_us(u64::MAX, 1_000_000), u64::MAX);
    }

    #[test]
    fn zero_frequency_is_reported_as_zero_rather_than_dividing_by_zero() {
        assert_eq!(ticks_to_us(12345, 0), 0);
    }
}
