//! Energy per trial from a power-reading command (a profile's `power`).
//!
//! The command runs for the whole profile and prints one reading in watts
//! per line (e.g. `ssh thor mima/serve.sh --power`, which reads the board's
//! input power sensor once a second). A trial's energy is the mean of the
//! readings taken during it times its duration. This measures the whole
//! device, idle draw included, so compare models on the same device only.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

type Samples = Arc<Mutex<Vec<(Instant, f64)>>>;

pub struct PowerMeter {
    child: Child,
    samples: Samples,
}

/// Energy over an interval.
pub struct Energy {
    pub joules: f64,
    pub mean_watts: f64,
}

impl PowerMeter {
    pub fn start(cmd: &str) -> Result<Self, String> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("power command: {e}"))?;
        let out = child.stdout.take().ok_or("power command: no stdout")?;
        let samples: Samples = Arc::default();
        let sink = samples.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if let Ok(w) = line.trim().parse::<f64>()
                    && w.is_finite()
                    && w >= 0.0
                    && let Ok(mut v) = sink.lock()
                {
                    v.push((Instant::now(), w));
                }
            }
        });
        Ok(Self { child, samples })
    }

    /// Energy between `from` and `to`; `None` with fewer than two readings.
    pub fn energy(&self, from: Instant, to: Instant) -> Option<Energy> {
        let v = self.samples.lock().ok()?;
        let inside: Vec<f64> = v
            .iter()
            .filter(|(t, _)| *t >= from && *t <= to)
            .map(|(_, w)| *w)
            .collect();
        if inside.len() < 2 {
            return None;
        }
        let mean_watts = inside.iter().sum::<f64>() / inside.len() as f64;
        Some(Energy {
            joules: mean_watts * to.duration_since(from).as_secs_f64(),
            mean_watts,
        })
    }
}

impl Drop for PowerMeter {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn integrates_readings_over_the_interval() {
        let m = PowerMeter::start("while :; do echo 20; echo junk; sleep 0.05; done").unwrap();
        let from = Instant::now();
        std::thread::sleep(Duration::from_millis(400));
        let e = m.energy(from, Instant::now()).expect("readings");
        assert!((e.mean_watts - 20.0).abs() < 1e-9);
        assert!(e.joules > 7.0 && e.joules < 10.0, "{}", e.joules);
    }
}
