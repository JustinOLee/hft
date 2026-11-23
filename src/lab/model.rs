//! A "fair value" model: what SHOULD the Up contract cost right now?
//!
//! The market asks: will BTC end the 5 minutes at or above where it started?
//! If BTC moves randomly (a "random walk"), the answer depends on just three
//! things:
//!
//!   distance  how far BTC is above (+) or below (-) the start price,
//!             measured as a log return: ln(spot / open)
//!   time      how many seconds are left
//!   vol       how jumpy BTC is: the typical log move in one second (sigma)
//!
//! In the time left, BTC typically moves about  sigma * sqrt(seconds_left).
//! Divide the distance by that and you get "how many typical moves ahead we
//! are" (a z-score). The normal distribution turns that into a probability:
//!
//!   P(up) = Phi( ln(spot / open) / (sigma * sqrt(seconds_left)) )
//!
//! Examples, with BTC typically moving $100 in the time left:
//!   exactly at the start price   -> z = 0   -> P = 50%
//!   $100 above                   -> z = 1   -> P = 84%
//!   $200 below                   -> z = -2  -> P = 2%
//!
//! This is the same idea as the Black-Scholes formula for a "digital" option.
//! It is a STARTING POINT, and it's wrong in several known ways (a good list
//! of things to improve):
//!   - Polymarket settles on a Chainlink price, Kalshi on the CF Benchmarks
//!     index. We use Coinbase. They differ by a few dollars ("basis").
//!   - Both settle on an AVERAGE price over a minute, not one instant.
//!     prob_up_avg below handles that (and the research that found it).
//!   - Real price moves have fat tails: big jumps happen more often than
//!     the normal distribution says.
//!   - Volatility isn't constant: it jumps around news and the US open.

/// What the market REALLY settles on (found from 30 days of results, see
/// docs/RESEARCH.md, E2): the average price over the window's last minute
/// vs the average over the minute before the window began (`reference`).
/// That rule matched the official results 95% of the time; "end price vs
/// start price" only 88%.
///
/// - Before the last minute: BTC wanders until the last minute starts,
///   then gets averaged. Averaging a random walk over `avg_s` seconds adds
///   only a third as much variance as `avg_s` seconds of wandering.
/// - Inside the last minute: part of the average is already known
///   (`partial_avg` over the seconds so far); only the rest is random.
pub fn prob_up_avg(spot: f64, reference: f64, sigma: f64, seconds_left: f64, avg_s: f64, partial_avg: Option<f64>) -> f64 {
    if sigma <= 0.0 || seconds_left <= 0.0 {
        let fin = partial_avg.unwrap_or(spot);
        return if fin >= reference { 1.0 } else { 0.0 };
    }
    if seconds_left > avg_s {
        let var_s = (seconds_left - avg_s) + avg_s / 3.0;
        return norm_cdf((spot / reference).ln() / (sigma * var_s.sqrt()));
    }
    let elapsed = avg_s - seconds_left;
    let known = partial_avg.unwrap_or(spot);
    let mean = (elapsed * known + seconds_left * spot) / avg_s;
    let sd = sigma * spot * (seconds_left.powi(3) / 3.0).sqrt() / avg_s;
    norm_cdf((mean - reference) / sd)
}

/// Probability the Up side wins, if it settled on the price at one instant
/// (the first, simpler model; kept for comparison: `"model": "point"`).
pub fn prob_up(spot: f64, open: f64, sigma_per_sqrt_s: f64, seconds_left: f64) -> f64 {
    let distance = (spot / open).ln();
    if seconds_left <= 0.0 || sigma_per_sqrt_s <= 0.0 {
        // No time (or no randomness) left: it is what it is.
        return if distance >= 0.0 { 1.0 } else { 0.0 };
    }
    norm_cdf(distance / (sigma_per_sqrt_s * seconds_left.sqrt()))
}

/// The normal distribution's cumulative probability: the chance a standard
/// normal random number is below `x`. Phi(0) = 0.5, Phi(1) = 0.84.
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

/// The error function, via the Abramowitz & Stegun 7.1.26 approximation
/// (accurate to about 1e-7, plenty for us).
fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let poly = ((((1.061_405_429 * t - 1.453_152_027) * t + 1.421_413_741) * t - 0.284_496_736) * t + 0.254_829_592) * t;
    sign * (1.0 - poly * (-x * x).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn normal_cdf_known_values() {
        assert!(close(norm_cdf(0.0), 0.5));
        assert!(close(norm_cdf(1.0), 0.841_34));
        assert!(close(norm_cdf(-2.0), 0.022_75));
    }

    #[test]
    fn prob_up_behaves() {
        let open = 70_000.0;
        let sigma = 1e-4; // per sqrt(second)
        // At the start price: a coin flip.
        assert!(close(prob_up(open, open, sigma, 300.0), 0.5));
        // One typical move above with 100 s left: sigma*sqrt(100) = 0.1%.
        let one_sd_up = open * (sigma * 10.0f64).exp();
        assert!(close(prob_up(one_sd_up, open, sigma, 100.0), 0.841_34));
        // Same distance, less time left: more certain.
        assert!(prob_up(one_sd_up, open, sigma, 25.0) > 0.97);
        // Time's up.
        assert_eq!(prob_up(open - 1.0, open, sigma, 0.0), 0.0);
        assert_eq!(prob_up(open, open, sigma, 0.0), 1.0); // ties go to Up
    }

    #[test]
    fn averaging_makes_the_end_less_random() {
        let (r, sigma) = (70_000.0, 1e-4);
        let up = r * 1.0005;
        // With 2 minutes to go, the average rule is a bit more confident than
        // the point rule (20 s of "averaged" variance instead of 60 s).
        assert!(prob_up_avg(up, r, sigma, 120.0, 60.0, None) > prob_up(up, r, sigma, 120.0));
        // Inside the last minute with most of the average already known and
        // far above the reference: nearly certain even if spot just dropped.
        let p = prob_up_avg(r * 0.9999, r, sigma, 5.0, 60.0, Some(r * 1.001));
        assert!(p > 0.99, "{p}");
        // At the reference with lots of time: a coin flip.
        assert!(close(prob_up_avg(r, r, sigma, 300.0, 60.0, None), 0.5));
    }
}
