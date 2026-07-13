//! Weighted-random API-key selection with model filtering. Kept pure (takes an
//! `&mut impl Rng`) so selection is deterministic and unit-testable.

use crate::provider::ApiKey;
use rand::Rng;

/// Keys eligible to serve `model`: those with no model allow-list (serve anything)
/// or whose allow-list contains `model`.
pub fn eligible_keys<'a>(keys: &'a [ApiKey], model: &str) -> Vec<&'a ApiKey> {
    keys.iter()
        .filter(|k| k.models.is_empty() || k.models.iter().any(|m| m == model))
        .collect()
}

/// Pick one key weighted by `weight`. Keys with weight 0 are effectively ineligible
/// while any positive-weight key exists; if *all* candidates have weight 0, fall back
/// to a uniform pick so a misconfiguration never yields "no key".
pub fn weighted_pick<'a>(keys: &[&'a ApiKey], rng: &mut impl Rng) -> Option<&'a ApiKey> {
    if keys.is_empty() {
        return None;
    }
    let total: u64 = keys.iter().map(|k| k.weight as u64).sum();
    if total == 0 {
        let idx = rng.gen_range(0..keys.len());
        return Some(keys[idx]);
    }
    let mut pick = rng.gen_range(0..total);
    for k in keys {
        let w = k.weight as u64;
        if pick < w {
            return Some(*k);
        }
        pick -= w;
    }
    // Unreachable given the invariant above, but return the last key rather than panic.
    keys.last().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn key(id: &str, weight: u32, models: &[&str]) -> ApiKey {
        ApiKey {
            id: id.into(),
            value: "secret".into(),
            weight,
            models: models.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn eligibility_filters_by_model() {
        let keys = vec![
            key("any", 1, &[]),
            key("gpt-only", 1, &["gpt-4o"]),
            key("claude-only", 1, &["claude-3"]),
        ];
        let eligible = eligible_keys(&keys, "gpt-4o");
        let ids: Vec<_> = eligible.iter().map(|k| k.id.as_str()).collect();
        assert_eq!(ids, vec!["any", "gpt-only"]);
    }

    #[test]
    fn weighted_distribution_matches_weights() {
        let a = key("a", 1, &[]);
        let b = key("b", 3, &[]);
        let refs = vec![&a, &b];
        let mut rng = StdRng::seed_from_u64(42);
        let mut counts = (0u32, 0u32);
        for _ in 0..10_000 {
            match weighted_pick(&refs, &mut rng).unwrap().id.as_str() {
                "a" => counts.0 += 1,
                "b" => counts.1 += 1,
                _ => unreachable!(),
            }
        }
        // Expect roughly 1:3. Assert b/a ratio is within a sane tolerance of 3.
        let ratio = counts.1 as f64 / counts.0 as f64;
        assert!(
            ratio > 2.6 && ratio < 3.4,
            "ratio {ratio} not ~3 (a={}, b={})",
            counts.0,
            counts.1
        );
    }

    #[test]
    fn all_zero_weight_falls_back_to_uniform() {
        let a = key("a", 0, &[]);
        let b = key("b", 0, &[]);
        let refs = vec![&a, &b];
        let mut rng = StdRng::seed_from_u64(7);
        // Should always return *some* key, never None.
        for _ in 0..100 {
            assert!(weighted_pick(&refs, &mut rng).is_some());
        }
    }

    #[test]
    fn empty_returns_none() {
        let refs: Vec<&ApiKey> = vec![];
        let mut rng = StdRng::seed_from_u64(1);
        assert!(weighted_pick(&refs, &mut rng).is_none());
    }
}
