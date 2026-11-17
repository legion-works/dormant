//! Duration text used by configuration files and command-line arguments.

use std::time::Duration;

/// A duration syntax, unit, or precision error. Offsets count UTF-8 bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A character cannot occur in a quantity or unit.
    #[error("invalid character at {0}")]
    InvalidCharacter(usize),
    /// A component does not start with an ASCII digit.
    #[error("expected number at {0}")]
    NumberExpected(usize),
    /// A suffix is absent or unsupported.
    #[error("{}", unit_error(unit, *value))]
    UnknownUnit {
        /// First byte of the suffix.
        start: usize,
        /// Byte after the suffix.
        end: usize,
        /// Suffix from the input.
        unit: String,
        /// Integer quantity before the suffix.
        value: u64,
    },
    /// Arithmetic overflow or a fraction that the unit cannot represent.
    #[error(
        "number is too large or cannot be represented without a lack of precision (values below 1ns are not supported)"
    )]
    NumberOverflow,
    /// The input contains no quantity.
    #[error("value was empty")]
    Empty,
}

fn unit_error(unit: &str, value: u64) -> String {
    if unit.is_empty() {
        format!("time unit needed, for example {value}sec or {value}ms")
    } else {
        format!(
            "unknown time unit {unit:?}, supported units: ns, us/µs, ms, sec, min, hours, days, weeks, months, years (and few variations)"
        )
    }
}

fn digit(value: u64, ch: char) -> Result<u64, Error> {
    value
        .checked_mul(10)
        .and_then(|v| v.checked_add(u64::from(ch as u32 - '0' as u32)))
        .ok_or(Error::NumberOverflow)
}

fn letter(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == 'µ'
}

// Use different scales for integer and fractional quantities. The text format
// limits large-unit fractions to integer seconds, not nanoseconds.
fn scales(unit: &str) -> Option<(u64, bool, u64)> {
    Some(match unit {
        "ns" | "nsec" | "nanos" => (1, true, 0),
        "us" | "usec" | "µs" => (1000, true, 1000),
        "ms" | "msec" | "millis" => (1_000_000, true, 1_000_000),
        "s" | "sec" | "secs" | "second" | "seconds" => (1, false, 1_000_000_000),
        "m" | "min" | "mins" | "minute" | "minutes" => (60, false, 60_000_000_000),
        "h" | "hr" | "hrs" | "hour" | "hours" => (3600, false, 3600),
        "d" | "day" | "days" => (86400, false, 86400),
        "w" | "wk" | "wks" | "week" | "weeks" => (604_800, false, 604_800),
        "M" | "month" | "months" => (2_630_016, false, 2_630_016),
        "y" | "yr" | "yrs" | "year" | "years" => (31_557_600, false, 31_557_600),
        _ => return None,
    })
}

struct Cursor<'a> {
    text: &'a str,
    offset: usize,
}

struct Quantity {
    whole: u64,
    fraction: Option<(u64, u64)>,
    unit_start: usize,
}

impl Cursor<'_> {
    fn next(&mut self) -> Option<(usize, char)> {
        let ch = self.text[self.offset..].chars().next()?;
        let offset = self.offset;
        self.offset += ch.len_utf8();
        Some((offset, ch))
    }

    fn first(&mut self) -> Result<Option<u64>, Error> {
        let start = self.offset;
        while let Some((_, ch)) = self.next() {
            if ch.is_ascii_digit() {
                return digit(0, ch).map(Some);
            }
            if !ch.is_whitespace() {
                return Err(Error::NumberExpected(start));
            }
        }
        Ok(None)
    }

    fn quantity(&mut self, mut whole: u64) -> Result<Quantity, Error> {
        let mut fraction = None;
        let mut last = self.offset;
        while let Some((offset, ch)) = self.next() {
            if letter(ch) {
                if fraction.is_some_and(|(_, denominator)| denominator == 1) {
                    return Err(Error::InvalidCharacter(last));
                }
                return Ok(Quantity {
                    whole,
                    fraction,
                    unit_start: offset,
                });
            }
            if ch.is_ascii_digit() {
                if let Some((numerator, denominator)) = &mut fraction {
                    *numerator = digit(*numerator, ch)?;
                    *denominator = denominator.checked_mul(10).ok_or(Error::NumberOverflow)?;
                } else {
                    whole = digit(whole, ch)?;
                }
            } else if ch == '.' && fraction.is_none() {
                fraction = Some((0_u64, 1_u64));
                last = offset;
                continue;
            } else if !ch.is_whitespace() {
                return Err(Error::InvalidCharacter(if fraction.is_some() {
                    last
                } else {
                    offset
                }));
            }
            last = self.offset;
        }
        Ok(Quantity {
            whole,
            fraction,
            unit_start: last,
        })
    }
}

fn accumulate(out: &mut Duration, value: u64, nanos: bool) -> Result<(), Error> {
    let (seconds, subsecond) = if nanos {
        let total = u64::from(out.subsec_nanos())
            .checked_add(value)
            .ok_or(Error::NumberOverflow)?;
        (total / 1_000_000_000, total % 1_000_000_000)
    } else {
        (value, u64::from(out.subsec_nanos()))
    };
    let seconds = out
        .as_secs()
        .checked_add(seconds)
        .ok_or(Error::NumberOverflow)?;
    *out = Duration::new(
        seconds,
        u32::try_from(subsecond).expect("normalized nanoseconds"),
    );
    Ok(())
}

/// Parse duration components with the configuration grammar.
///
/// Units range from `ns` to `years`. A month is 2,630,016 seconds and a year
/// is 31,557,600 seconds. Components can touch or use Unicode whitespace.
/// Fractions must fit the precision and arithmetic limits of their unit.
/// The bare string `0` is also accepted.
///
/// # Errors
/// Returns a syntax, unit, overflow, or precision error for invalid input.
pub fn parse_duration(input: &str) -> Result<Duration, Error> {
    if input == "0" {
        return Ok(Duration::ZERO);
    }
    let mut cursor = Cursor {
        text: input,
        offset: 0,
    };
    let mut first = cursor.first()?.ok_or(Error::Empty)?;
    let mut out = Duration::ZERO;
    loop {
        let Quantity {
            whole,
            fraction,
            unit_start: start,
        } = cursor.quantity(first)?;
        if fraction.is_some_and(|(_, denominator)| denominator == 1) {
            return Err(Error::InvalidCharacter(start));
        }
        let mut end = cursor.offset;
        let mut next = None;
        while let Some((offset, ch)) = cursor.next() {
            if ch.is_ascii_digit() {
                next = Some(digit(0, ch)?);
                break;
            }
            if ch.is_whitespace() {
                break;
            }
            if !letter(ch) {
                return Err(Error::InvalidCharacter(offset));
            }
            end = cursor.offset;
        }
        let unit = &input[start..end];
        let (scale, nanos, fractional_scale) = scales(unit).ok_or_else(|| Error::UnknownUnit {
            start,
            end,
            unit: unit.to_owned(),
            value: whole,
        })?;
        accumulate(
            &mut out,
            whole.checked_mul(scale).ok_or(Error::NumberOverflow)?,
            nanos,
        )?;
        if let Some((numerator, denominator)) = fraction {
            if fractional_scale == 0 {
                return Err(Error::NumberOverflow);
            }
            let value = numerator
                .checked_mul(fractional_scale)
                .ok_or(Error::NumberOverflow)?;
            if value % denominator != 0 {
                return Err(Error::NumberOverflow);
            }
            accumulate(&mut out, value / denominator, nanos || scale <= 60)?;
        }
        let next = match next {
            Some(value) => Some(value),
            None => cursor.first()?,
        };
        match next {
            Some(value) => first = value,
            None => return Ok(out),
        }
    }
}

/// Format a duration with descending units and ASCII microseconds (`us`).
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let mut parts = Vec::new();
    let mut seconds = duration.as_secs();
    for (scale, suffix, plural) in [
        (31_557_600, "year", true),
        (2_630_016, "month", true),
        (86400, "day", true),
        (3600, "h", false),
        (60, "m", false),
        (1, "s", false),
    ] {
        let count = seconds / scale;
        seconds %= scale;
        if count > 0 {
            let plural = if plural && count > 1 { "s" } else { "" };
            parts.push(format!("{count}{suffix}{plural}"));
        }
    }
    let mut nanos = duration.subsec_nanos();
    for (scale, suffix) in [(1_000_000, "ms"), (1000, "us"), (1, "ns")] {
        let count = nanos / scale;
        nanos %= scale;
        if count > 0 {
            parts.push(format!("{count}{suffix}"));
        }
    }
    if parts.is_empty() {
        "0s".to_owned()
    } else {
        parts.join(" ")
    }
}

/// Serde string or null adapter for an optional duration.
pub mod option;
/// Serde string adapter for a required duration.
pub mod serde;
