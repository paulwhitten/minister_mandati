#!/bin/sh
set -e
cat > src/lib.rs <<'RS'
/// Arithmetic mean of the values, or `None` for an empty slice.
pub fn average(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let sum: f64 = xs.iter().sum();
    Some(sum / xs.len() as f64)
}

/// Values above the mean.
pub fn above_average(xs: &[f64]) -> Vec<f64> {
    match average(xs) {
        Some(m) => xs.iter().copied().filter(|&x| x > m).collect(),
        None => Vec::new(),
    }
}
RS
