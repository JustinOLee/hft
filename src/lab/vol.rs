//! Volatility: how jumpy is BTC right now?
//!
//! We measure the log return each second, r = ln(price_now / price_before),
//! square it (so up and down moves both count), and keep an exponentially
//! weighted moving average (EWMA): recent seconds count most, and a move's
//! weight halves every `halflife_s` seconds. The square root of that average
//! is sigma: the typical move in one second.
//!
//! Levers to play with:
//!   halflife_s  short = reacts fast but is noisy; long = smooth but slow
//!   prior       the guess we start with before any data arrives
//!
//! Why once a second and not every tick: the last price bounces between
//! the bid and the ask ("bid-ask bounce"), and that bounce isn't real
//! movement. Sampling less often makes it matter less.

pub struct Vol {
    halflife_s: f64,
    var_per_s: f64,
    last: Option<(u64, f64)>, // (second, ln price)
}

impl Vol {
    pub fn new(prior_sigma: f64, halflife_s: f64) -> Vol {
        Vol { halflife_s, var_per_s: prior_sigma * prior_sigma, last: None }
    }

    /// Feed the latest price, at most once per second (`sec` = event time).
    pub fn sample(&mut self, sec: u64, price: f64) {
        let lp = price.ln();
        if let Some((s0, l0)) = self.last {
            if sec <= s0 {
                return;
            }
            let dt = (sec - s0) as f64;
            let r = lp - l0;
            let observed = r * r / dt; // variance per second over this gap
            let weight = 1.0 - 0.5f64.powf(dt / self.halflife_s);
            self.var_per_s += weight * (observed - self.var_per_s);
        }
        self.last = Some((sec, lp));
    }

    /// Typical log move in one second.
    pub fn sigma(&self) -> f64 {
        self.var_per_s.sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_the_size_of_moves() {
        // Alternate +0.01% and -0.01% every second: sigma should approach 1e-4.
        let mut v = Vol::new(1e-3, 10.0);
        let mut p = 70_000.0;
        for s in 0..300 {
            p *= if s % 2 == 0 { 1.0001 } else { 1.0 / 1.0001 };
            v.sample(s, p);
        }
        assert!((v.sigma() - 1e-4).abs() < 2e-6, "{}", v.sigma());
    }
}
