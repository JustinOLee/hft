//! Exact decimal numbers stored as integers.
//!
//! Kalshi sends prices as strings like "0.5600" (dollars, up to 4 decimals)
//! and counts as strings like "300.00" (contracts, 2 decimals). We never turn
//! these into floats: in floating point, 0.1 + 0.2 = 0.30000000000000004, and
//! an order at a price that is off by a hair gets rejected.
//!
//! Instead we count in small whole units:
//!   Price = 1/10000 of a dollar   "0.5600" -> 5600,  $1.00 -> 10000
//!   Qty   = 1/100 of a contract   "300.00" -> 30000
//! Integer arithmetic is exact, fast, and identical on every machine.

/// Price in units of $0.0001.
pub type Price = i64;
/// Quantity in units of 0.01 contracts.
pub type Qty = i64;

pub const PRICE_DECIMALS: u32 = 4;
pub const QTY_DECIMALS: u32 = 2;
/// A contract pays $1.00 if it resolves YES.
pub const ONE_DOLLAR: Price = 10_000;

/// Parse a decimal string into an integer number of 10^-`decimals` units.
/// Returns None for anything malformed, including more decimal places than
/// expected: better to fail loudly than to silently round a price.
pub fn parse(s: &str, decimals: u32) -> Option<i64> {
    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() || frac.len() > decimals as usize {
        return None;
    }
    if !whole.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    // "0.56" with 4 decimals: whole=0, frac="56" -> pad to "5600".
    let scale = 10_i64.pow(decimals);
    let frac_value: i64 = if frac.is_empty() { 0 } else { frac.parse().ok()? };
    let frac_scaled = frac_value * 10_i64.pow(decimals - frac.len() as u32);
    // checked_* return None instead of silently overflowing.
    let value = whole.parse::<i64>().ok()?.checked_mul(scale)?.checked_add(frac_scaled)?;
    Some(if negative { -value } else { value })
}

/// Format an integer number of 10^-`decimals` units as a decimal string.
pub fn format(value: i64, decimals: u32) -> String {
    let scale = 10_i64.pow(decimals);
    let sign = if value < 0 { "-" } else { "" };
    let v = value.unsigned_abs();
    let s = scale as u64;
    format!("{sign}{}.{:0width$}", v / s, v % s, width = decimals as usize)
}

pub fn parse_price(s: &str) -> Option<Price> {
    parse(s, PRICE_DECIMALS)
}

pub fn parse_qty(s: &str) -> Option<Qty> {
    parse(s, QTY_DECIMALS)
}

pub fn format_price(p: Price) -> String {
    format(p, PRICE_DECIMALS)
}

// Tests live next to the code. `cargo test` compiles and runs everything
// marked #[test]; `assert_eq!` fails the test if the two sides differ.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prices() {
        assert_eq!(parse_price("0.5600"), Some(5600));
        assert_eq!(parse_price("0.56"), Some(5600));
        assert_eq!(parse_price("1.0000"), Some(ONE_DOLLAR));
        assert_eq!(parse_price("0.0001"), Some(1));
        assert_eq!(parse_price("0"), Some(0));
    }

    #[test]
    fn parses_quantities_and_negatives() {
        assert_eq!(parse_qty("300.00"), Some(30000));
        assert_eq!(parse_qty("-54.00"), Some(-5400));
        assert_eq!(parse_qty("7"), Some(700));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(parse_price("0.56001"), None); // too many decimals
        assert_eq!(parse_price(""), None);
        assert_eq!(parse_price(".5"), None);
        assert_eq!(parse_price("abc"), None);
        assert_eq!(parse_price("1e3"), None);
        assert_eq!(parse_price("0.5.6"), None);
        assert_eq!(parse_qty("99999999999999999999"), None); // overflow
    }

    #[test]
    fn formats_round_trip() {
        for s in ["0.5600", "1.0000", "0.0001", "-0.0100", "12.3400"] {
            assert_eq!(format_price(parse_price(s).unwrap()), s);
        }
    }
}
