//! An estimate of what the tokens would cost on the Anthropic API. Transcripts carry no price,
//! so this uses the list prices published on 2026-10-17
//! (https://platform.claude.com/docs/en/about-claude/pricing), which age with every new model;
//! a subscription does not bill this way at all. Models missing from the list are left out.

use crate::model::Tokens;

/// Dollars per million tokens: input, cache read, output. Cache writes cost 1.25 times the
/// input price when kept 5 minutes, 2 times when kept an hour.
const PRICES: &[(&str, f64, f64, f64)] = &[
    ("fable-5-1", 10.0, 0.25, 50.0),
    ("fable-5", 10.0, 1.0, 50.0),
    ("opus-5-5", 4.0, 0.20, 20.0),
    ("opus-5", 5.0, 0.50, 25.0),
    ("opus-4-8", 5.0, 0.50, 25.0),
    ("opus-4-7", 5.0, 0.50, 25.0),
    ("opus-4-6", 5.0, 0.50, 25.0),
    ("opus-4-5", 5.0, 0.50, 25.0),
    ("opus-4-1", 15.0, 1.50, 75.0),
    ("opus-4", 15.0, 1.50, 75.0),
    ("sonnet-5-5", 2.0, 0.10, 10.0),
    ("sonnet-5", 2.0, 0.20, 10.0),
    ("sonnet-4", 3.0, 0.30, 15.0),
    ("haiku-5-5", 0.10, 0.01, 0.50),
    ("haiku-4-5", 1.0, 0.10, 5.0),
];

/// Haiku 5.5 prompts over 100k tokens (cache included) are billed at these rates instead.
const HAIKU_5_5_LONG: (f64, f64, f64) = (0.50, 0.05, 2.50);

/// Prices for a model id such as `claude-opus-5-5` or `eu.anthropic.claude-haiku-4-5-20251001`;
/// the longest matching entry wins, so `opus-4-5` is not read as `opus-4`.
pub fn of(model: &str) -> Option<(f64, f64, f64)> {
    PRICES.iter().filter(|(k, ..)| model.contains(k)).max_by_key(|(k, ..)| k.len()).map(|&(_, i, r, o)| (i, r, o))
}

/// Estimated price of one call's tokens in millionths of a dollar, or none for a model with no
/// known price. Fast mode doubles the rates.
pub fn cost_micros(model: &str, t: &Tokens, fast: bool) -> Option<u64> {
    let (mut i, mut r, mut o) = of(model)?;
    if model.contains("haiku-5-5") && t.fresh + t.cache_write + t.cache_read > 100_000 {
        (i, r, o) = HAIKU_5_5_LONG;
    }
    let k = if fast { 2.0 } else { 1.0 };
    let w5 = t.cache_write.saturating_sub(t.cache_write_1h);
    let usd_per_m = t.fresh as f64 * i + w5 as f64 * i * 1.25 + t.cache_write_1h as f64 * i * 2.0 + t.cache_read as f64 * r + t.output as f64 * o;
    // Dollars per million tokens times tokens is micro-dollars.
    Some((usd_per_m * k).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_each_kind_of_token() {
        let m = 1_000_000;
        let t = Tokens { fresh: m, cache_write: m, cache_write_1h: m, cache_read: m, output: m, ..Tokens::default() };
        // 4 fresh + 8 (1h write) + 0.20 read + 20 output dollars.
        assert_eq!(cost_micros("claude-opus-5-5", &t, false), Some(32_200_000));
        assert_eq!(cost_micros("claude-opus-5-5", &t, true), Some(64_400_000));
        assert_eq!(of("claude-opus-4-5-20251101"), Some((5.0, 0.5, 25.0)));
        assert_eq!(of("claude-opus-4-20250514"), Some((15.0, 1.5, 75.0)));
        assert_eq!(of("claude-sonnet-4-5-20250929"), Some((3.0, 0.3, 15.0)));
        let small = Tokens { fresh: 1000, output: 1000, ..Tokens::default() };
        assert_eq!(cost_micros("claude-haiku-5-5", &small, false), Some(600));
        assert_eq!(of("<synthetic>"), None);
    }
}
