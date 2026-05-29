//! CMOS RTC on QEMU's x86_64 `q35` machine.
//!
//! The MC146818-style RTC behind ports 0x70/0x71 keeps calendar time (QEMU initialises it
//! from the host clock, UTC by default). The whole-second part is what the `eo9:time/time.now`
//! wall clock needs; the sub-second part comes from the TSC (src/arch/x86_64/timer.rs),
//! mirroring the PL031 + generic-timer split on aarch64. Reads wait out an in-progress
//! update and repeat until two passes agree, the standard dance for this device.

use super::io::{inb, outb};

/// Index and data ports. Bit 7 of the index keeps NMI disabled, which is how it boots.
const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

/// Register numbers.
const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

/// Status A: update in progress.
const STATUS_A_UIP: u8 = 1 << 7;
/// Status B: hours are 24-hour (not 12-hour).
const STATUS_B_24H: u8 = 1 << 1;
/// Status B: values are binary (not BCD).
const STATUS_B_BINARY: u8 = 1 << 2;

fn read_register(register: u8) -> u8 {
    outb(CMOS_INDEX, register | 0x80);
    inb(CMOS_DATA)
}

fn bcd_to_binary(value: u8) -> u8 {
    (value >> 4) * 10 + (value & 0x0F)
}

/// One raw calendar read (seconds, minutes, hours, day, month, year).
fn read_calendar() -> [u8; 6] {
    while read_register(REG_STATUS_A) & STATUS_A_UIP != 0 {
        core::hint::spin_loop();
    }
    [
        read_register(REG_SECONDS),
        read_register(REG_MINUTES),
        read_register(REG_HOURS),
        read_register(REG_DAY),
        read_register(REG_MONTH),
        read_register(REG_YEAR),
    ]
}

/// Days from 1970-01-01 to `year`-`month`-`day` (proleptic Gregorian; the standard
/// days-from-civil computation).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Seconds since the Unix epoch.
pub fn seconds() -> u32 {
    // Read until two consecutive passes agree, so a rollover mid-read cannot produce a
    // nonsense timestamp.
    let mut raw = read_calendar();
    loop {
        let again = read_calendar();
        if again == raw {
            break;
        }
        raw = again;
    }

    let status_b = read_register(REG_STATUS_B);
    let convert = |value: u8| -> u8 {
        if status_b & STATUS_B_BINARY != 0 {
            value
        } else {
            bcd_to_binary(value)
        }
    };

    let second = u32::from(convert(raw[0]));
    let minute = u32::from(convert(raw[1]));
    // A 12-hour clock keeps its PM flag in bit 7 of the hours register.
    let hour_register = raw[2];
    let mut hour = u32::from(convert(hour_register & 0x7F));
    if status_b & STATUS_B_24H == 0 && hour_register & 0x80 != 0 {
        hour = (hour % 12) + 12;
    }
    let day = u32::from(convert(raw[3]));
    let month = u32::from(convert(raw[4]));
    // Two-digit year; the century register is firmware-specific, so assume 20xx (valid
    // until 2100, like the rest of this century's firmware).
    let year = 2000 + i64::from(convert(raw[5]));

    let days = days_from_civil(year, month.max(1), day.max(1));
    (days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second)).max(0)
        as u32
}
