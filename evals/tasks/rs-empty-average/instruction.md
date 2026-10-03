`average` in `src/lib.rs` returns NaN for an empty slice. Change it to return
`Option<f64>`: `None` for an empty slice and `Some(mean)` otherwise. Update any
callers in the crate. Do not change `Cargo.toml`.
