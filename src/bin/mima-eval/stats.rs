//! Statistics for eval results, following Miller, "Adding Error Bars to
//! Evals" (arXiv:2411.00640) and Bowyer et al. (arXiv:2503.01747):
//! per-task means first (never pooled over trials), standard errors clustered
//! by task family, t intervals, Wilson intervals for small counts, and paired
//! differences between models. See docs/eval.md.

use std::collections::BTreeMap;

/// 95% two-sided normal quantile.
const Z: f64 = 1.959_964;
/// z for 80% power; with `Z`, the minimum detectable effect multiplier.
const Z_POWER: f64 = 0.841_621;

/// Two-sided 95% t quantiles for df = 1..=30; larger df use the normal value.
const T975: [f64; 30] = [
    12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
    2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
    2.052, 2.048, 2.045, 2.042,
];

fn t975(df: usize) -> f64 {
    match df {
        0 => f64::NAN,
        1..=30 => T975[df - 1],
        _ => Z,
    }
}

/// Wilson score 95% interval for `c` successes in `n` trials.
pub fn wilson(c: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let (c, n) = (c as f64, n as f64);
    let p = c / n;
    let z2 = Z * Z;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let half = Z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denom;
    ((center - half).max(0.0), (center + half).min(1.0))
}

/// Unbiased pass@k (Chen et al. 2021): probability that at least one of k
/// samples drawn from the n trials passed.
pub fn pass_at_k(c: usize, n: usize, k: usize) -> f64 {
    if k == 0 || k > n {
        return f64::NAN;
    }
    if n - c < k {
        return 1.0;
    }
    // 1 - C(n-c, k) / C(n, k), as a product to avoid large binomials.
    1.0 - ((n - c + 1)..=n).fold(1.0, |acc, i| acc * (1.0 - k as f64 / i as f64))
}

/// pass^k (tau-bench): probability that all of k samples drawn from the n
/// trials passed, C(c, k) / C(n, k). A consistency measure.
pub fn pass_hat_k(c: usize, n: usize, k: usize) -> f64 {
    if k == 0 || k > n {
        return f64::NAN;
    }
    (0..k).fold(1.0, |acc, i| {
        acc * (c.saturating_sub(i)) as f64 / (n - i) as f64
    })
}

/// One task's result for a model: `c` passing trials of `k`.
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub task: String,
    pub family: String,
    pub c: usize,
    pub k: usize,
}

impl TaskResult {
    pub fn mean(&self) -> f64 {
        if self.k == 0 {
            0.0
        } else {
            self.c as f64 / self.k as f64
        }
    }
}

/// A mean with its clustered standard error and 95% t interval.
#[derive(Debug, Clone, Copy)]
pub struct Estimate {
    pub mean: f64,
    pub se: f64,
    pub lo: f64,
    pub hi: f64,
    pub n: usize,
    pub clusters: usize,
}

/// Mean of `values` with a standard error clustered by `families`
/// (Miller eq. 4, with the C/(C-1) small-sample correction used by Inspect).
/// With every value in its own family this is the ordinary SE of the mean.
pub fn clustered_mean(values: &[f64], families: &[String]) -> Estimate {
    let n = values.len();
    let mean = if n == 0 {
        0.0
    } else {
        values.iter().sum::<f64>() / n as f64
    };
    let mut sums: BTreeMap<&str, f64> = BTreeMap::new();
    for (v, f) in values.iter().zip(families) {
        *sums.entry(f.as_str()).or_default() += v - mean;
    }
    let c = sums.len();
    let (se, lo, hi) = if c < 2 {
        (f64::NAN, f64::NAN, f64::NAN)
    } else {
        let sq: f64 = sums.values().map(|s| s * s).sum();
        let se = ((c as f64 / (c as f64 - 1.0)) * sq).sqrt() / n as f64;
        let half = t975(c - 1) * se;
        (se, mean - half, mean + half)
    };
    Estimate {
        mean,
        se,
        lo,
        hi,
        n,
        clusters: c,
    }
}

/// Suite pass@1 for one model: the mean of per-task pass rates.
pub fn suite(tasks: &[TaskResult]) -> Estimate {
    let values: Vec<f64> = tasks.iter().map(TaskResult::mean).collect();
    let families: Vec<String> = tasks.iter().map(|t| t.family.clone()).collect();
    let mut e = clustered_mean(&values, &families);
    // A pass rate cannot leave [0, 1]; the t interval can, so clip it.
    e.lo = e.lo.max(0.0);
    e.hi = e.hi.min(1.0);
    e
}

/// Paired comparison of model A against model B on the tasks both ran.
#[derive(Debug, Clone)]
pub struct Paired {
    pub diff: Estimate,
    /// Correlation of per-task pass rates between the two models.
    pub corr: f64,
    /// Smallest difference this comparison could detect (alpha 0.05, power 0.8).
    pub mde: f64,
    pub a_better: usize,
    pub b_better: usize,
    pub ties: usize,
}

pub fn paired(a: &[TaskResult], b: &[TaskResult]) -> Paired {
    let b_by_task: BTreeMap<&str, &TaskResult> = b.iter().map(|t| (t.task.as_str(), t)).collect();
    let pairs: Vec<(&TaskResult, &TaskResult)> = a
        .iter()
        .filter_map(|ta| b_by_task.get(ta.task.as_str()).map(|tb| (ta, *tb)))
        .collect();
    let d: Vec<f64> = pairs.iter().map(|(x, y)| x.mean() - y.mean()).collect();
    let fam: Vec<String> = pairs.iter().map(|(x, _)| x.family.clone()).collect();
    let diff = clustered_mean(&d, &fam);
    let xa: Vec<f64> = pairs.iter().map(|(x, _)| x.mean()).collect();
    let xb: Vec<f64> = pairs.iter().map(|(_, y)| y.mean()).collect();
    let (mut a_better, mut b_better, mut ties) = (0, 0, 0);
    for v in &d {
        match v.partial_cmp(&0.0) {
            Some(std::cmp::Ordering::Greater) => a_better += 1,
            Some(std::cmp::Ordering::Less) => b_better += 1,
            _ => ties += 1,
        }
    }
    Paired {
        mde: (Z + Z_POWER) * diff.se,
        diff,
        corr: pearson(&xa, &xb),
        a_better,
        b_better,
        ties,
    }
}

fn pearson(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    if n < 2.0 {
        return f64::NAN;
    }
    let (mx, my) = (x.iter().sum::<f64>() / n, y.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (a, b) in x.iter().zip(y) {
        sxy += (a - mx) * (b - my);
        sxx += (a - mx) * (a - mx);
        syy += (b - my) * (b - my);
    }
    if sxx == 0.0 || syy == 0.0 {
        f64::NAN
    } else {
        sxy / (sxx * syy).sqrt()
    }
}

/// Median of a sample (NaN when empty).
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let m = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[m - 1] + v[m]) / 2.0
    } else {
        v[m]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn wilson_matches_known_values() {
        // Values quoted in the research note (3/5 and 5/5).
        let (lo, hi) = wilson(3, 5);
        assert!(close(lo, 0.231) && close(hi, 0.882), "{lo} {hi}");
        let e = suite(&[
            TaskResult {
                task: "a".into(),
                family: "a".into(),
                c: 3,
                k: 3,
            },
            TaskResult {
                task: "b".into(),
                family: "b".into(),
                c: 3,
                k: 3,
            },
            TaskResult {
                task: "c".into(),
                family: "c".into(),
                c: 2,
                k: 3,
            },
        ]);
        assert!(e.hi <= 1.0 && e.lo >= 0.0, "{} {}", e.lo, e.hi);
        let (lo, hi) = wilson(5, 5);
        assert!(close(lo, 0.566) && close(hi, 1.0), "{lo} {hi}");
    }

    #[test]
    fn pass_at_k_and_pass_hat_k() {
        assert!(close(pass_at_k(1, 5, 1), 0.2));
        assert!(close(pass_at_k(1, 5, 5), 1.0));
        assert!(close(pass_at_k(0, 5, 3), 0.0));
        // 3 of 5: at least one of 2 = 1 - C(2,2)/C(5,2) = 0.9
        assert!(close(pass_at_k(3, 5, 2), 0.9));
        // all of 2 = C(3,2)/C(5,2) = 0.3
        assert!(close(pass_hat_k(3, 5, 2), 0.3));
        assert!(close(pass_hat_k(5, 5, 5), 1.0));
        assert!(close(pass_hat_k(4, 5, 5), 0.0));
    }

    fn task(id: &str, fam: &str, c: usize, k: usize) -> TaskResult {
        TaskResult {
            task: id.into(),
            family: fam.into(),
            c,
            k,
        }
    }

    #[test]
    fn suite_mean_is_mean_of_task_means_not_pooled() {
        // 1/1 and 0/4: pooled would be 1/5 = 0.2; mean of means is 0.5.
        let s = suite(&[task("a", "a", 1, 1), task("b", "b", 0, 4)]);
        assert!(close(s.mean, 0.5));
    }

    #[test]
    fn singleton_clusters_give_the_ordinary_standard_error() {
        let v = [1.0, 0.0, 1.0, 1.0];
        let fam: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let e = clustered_mean(&v, &fam);
        // sample sd = 0.5, se = 0.5 / 2 = 0.25
        assert!(close(e.se, 0.25), "{}", e.se);
        assert!(close(e.hi - e.mean, 3.182 * 0.25));
    }

    #[test]
    fn clustering_widens_the_error_for_correlated_tasks() {
        // Two families whose members move together.
        let v = [1.0, 1.0, 0.0, 0.0];
        let iid: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let fam: Vec<String> = ["x", "x", "y", "y"].iter().map(|s| s.to_string()).collect();
        assert!(clustered_mean(&v, &fam).se > clustered_mean(&v, &iid).se);
        // One cluster: no standard error can be estimated.
        let one = vec!["z".to_string(); 4];
        assert!(clustered_mean(&v, &one).se.is_nan());
    }

    #[test]
    fn paired_difference_counts_and_mde() {
        let a = [
            task("t1", "t1", 5, 5),
            task("t2", "t2", 3, 5),
            task("t3", "t3", 0, 5),
        ];
        let b = [
            task("t1", "t1", 4, 5),
            task("t2", "t2", 3, 5),
            task("t3", "t3", 1, 5),
        ];
        let p = paired(&a, &b);
        assert!(close(p.diff.mean, 0.0));
        assert_eq!((p.a_better, p.b_better, p.ties), (1, 1, 1));
        assert!(close(p.mde, 2.8016 * p.diff.se));
    }

    #[test]
    fn median_of_even_and_odd() {
        assert!(close(median(&[3.0, 1.0, 2.0]), 2.0));
        assert!(close(median(&[4.0, 1.0, 2.0, 3.0]), 2.5));
    }
}
