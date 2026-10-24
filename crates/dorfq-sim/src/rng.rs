//! The simulator's random numbers: SplitMix64, split into streams by purpose.

/// The SplitMix64 generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64 { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        finalize(self.state)
    }

    /// Uniform in `[0, 1)`, from the top 53 bits.
    pub fn unit(&mut self) -> f64 {
        unit(self.next_u64())
    }

    /// True with probability `p`.
    pub fn bernoulli(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// Exponential with the given rate, in the rate's reciprocal unit.
    pub fn exponential(&mut self, rate: f64) -> f64 {
        -(1.0 - self.unit()).ln() / rate
    }

    /// Uniform over `min..=max`.
    pub fn uniform(&mut self, min: u64, max: u64) -> u64 {
        let span = max - min + 1;
        min + (self.unit() * span as f64) as u64
    }
}

/// SplitMix64's output function.
fn finalize(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A 64-bit value as a unit-interval `f64`, from its top 53 bits.
pub fn unit(x: u64) -> f64 {
    (x >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// A symmetric mix of two keys: `mix(a, b) == mix(b, a)`.
pub fn mix(a: u64, b: u64) -> u64 {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    finalize(finalize(lo).wrapping_add(hi))
}

/// One replication's generators, one per purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Streams {
    pub arrivals: SplitMix64,
    pub truth: SplitMix64,
    pub ci: SplitMix64,
    pub churn: SplitMix64,
    pub stacks: SplitMix64,
}

impl Streams {
    /// Replication `k`'s streams: its root is seeded with output `k`
    /// (0-based) of the master generator, and stream `j` with output `j` of
    /// the root, in the order arrivals, truth, ci, churn, stacks.
    pub fn for_replication(master_seed: u64, k: u64) -> Streams {
        let mut master = SplitMix64::new(master_seed);
        let mut root_seed = master.next_u64();
        for _ in 0..k {
            root_seed = master.next_u64();
        }
        let mut root = SplitMix64::new(root_seed);
        Streams {
            arrivals: SplitMix64::new(root.next_u64()),
            truth: SplitMix64::new(root.next_u64()),
            ci: SplitMix64::new(root.next_u64()),
            churn: SplitMix64::new(root.next_u64()),
            stacks: SplitMix64::new(root.next_u64()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reproduces_the_reference_sequence_for_seed_zero() {
        let mut g = SplitMix64::new(0);
        assert_eq!(g.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(g.next_u64(), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(g.next_u64(), 0x06c4_5d18_8009_454f);
    }

    #[test]
    fn unit_draws_lie_in_the_half_open_interval() {
        let mut g = SplitMix64::new(7);
        for _ in 0..100_000 {
            let u = g.unit();
            assert!((0.0..1.0).contains(&u), "{u}");
        }
        assert!(unit(u64::MAX) < 1.0);
        assert_eq!(unit(0), 0.0);
    }

    #[test]
    fn exponential_and_bernoulli_means_match_their_parameters() {
        let mut g = SplitMix64::new(11);
        let n = 100_000;
        let rate = 4.0;
        let mean = (0..n).map(|_| g.exponential(rate)).sum::<f64>() / n as f64;
        assert!((mean * rate - 1.0).abs() < 0.02, "{mean}");
        let p = 0.3;
        let hits = (0..n).filter(|_| g.bernoulli(p)).count() as f64 / n as f64;
        assert!((hits / p - 1.0).abs() < 0.02, "{hits}");
    }

    #[test]
    fn uniform_stays_in_range_and_reaches_both_ends() {
        let mut g = SplitMix64::new(3);
        let draws: Vec<u64> = (0..10_000).map(|_| g.uniform(5, 8)).collect();
        assert!(draws.iter().all(|d| (5..=8).contains(d)));
        assert!(draws.contains(&5) && draws.contains(&8));
        assert_eq!(g.uniform(9, 9), 9);
    }

    fn first_eight(g: &SplitMix64) -> Vec<u64> {
        let mut g = g.clone();
        (0..8).map(|_| g.next_u64()).collect()
    }

    fn all(s: &Streams) -> Vec<Vec<u64>> {
        [&s.arrivals, &s.truth, &s.ci, &s.churn, &s.stacks]
            .into_iter()
            .map(first_eight)
            .collect()
    }

    #[test]
    fn streams_differ_within_and_across_replications() {
        for k in 0..4 {
            let this = all(&Streams::for_replication(1, k));
            let next = all(&Streams::for_replication(1, k + 1));
            for (i, a) in this.iter().enumerate() {
                for (j, b) in this.iter().enumerate() {
                    if i != j {
                        assert_ne!(a, b, "replication {k}, streams {i} and {j}");
                    }
                }
                for b in &next {
                    assert_ne!(a, b, "replications {k} and {}", k + 1);
                }
            }
        }
    }

    #[test]
    fn streams_are_a_function_of_seed_and_replication() {
        assert_eq!(
            Streams::for_replication(9, 2),
            Streams::for_replication(9, 2)
        );
        assert_ne!(
            Streams::for_replication(9, 2),
            Streams::for_replication(10, 2)
        );
    }

    #[test]
    fn mix_is_symmetric() {
        assert_eq!(mix(3, 99), mix(99, 3));
        assert_ne!(mix(3, 99), mix(3, 98));
    }
}
