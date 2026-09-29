/// Arithmetic mean of the values.
pub fn average(xs: &[f64]) -> f64 {
    let sum: f64 = xs.iter().sum();
    sum / xs.len() as f64
}

/// Values above the mean.
pub fn above_average(xs: &[f64]) -> Vec<f64> {
    let m = average(xs);
    xs.iter().copied().filter(|&x| x > m).collect()
}
