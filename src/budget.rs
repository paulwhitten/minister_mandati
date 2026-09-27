//! Context budget derived from the model's window, and a self-calibrating
//! token estimate. See `docs/context.md`.

use crate::config::Config;

/// Window assumed when neither config nor the server provides one.
pub const FALLBACK_WINDOW: usize = 32_768;
/// Characters per token before the first server-reported usage calibrates it.
/// Deliberately low (conservative): it overestimates tokens.
const DEFAULT_CHARS_PER_TOKEN: f64 = 3.0;
/// Bounds on the calibrated ratio, guarding against odd usage reports.
const MIN_CHARS_PER_TOKEN: f64 = 1.5;
const MAX_CHARS_PER_TOKEN: f64 = 8.0;
/// Per-output cap bounds in tokens (stage 0).
const MIN_OUTPUT_CAP: usize = 1_000;
const MAX_OUTPUT_CAP: usize = 16_000;
/// Masking is skipped unless it frees at least this fraction of E.
pub const MIN_MASK_GAIN: f64 = 0.10;

/// Token limits for one model, all in tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// Context window W.
    pub window: usize,
    /// Hard prompt limit U = W - reply reserve - safety margin.
    pub usable: usize,
    /// Operating budget E = min(U, configured budget).
    pub operating: usize,
}

impl Budget {
    pub fn new(window: usize, cfg: &Config) -> Self {
        let reserve = cfg.agent.max_tokens;
        let margin = 512.max(window * 3 / 100);
        let usable = window.saturating_sub(reserve + margin).max(1);
        let operating = cfg.context.budget.map_or(usable, |b| b.min(usable)).max(1);
        Self {
            window,
            usable,
            operating,
        }
    }

    /// `fraction` of the operating budget, in tokens.
    pub fn of(&self, fraction: f64) -> usize {
        (self.operating as f64 * fraction) as usize
    }

    /// Stage 0: largest single tool output kept verbatim, in tokens.
    pub fn output_cap_tokens(&self) -> usize {
        self.of(0.08).clamp(MIN_OUTPUT_CAP, MAX_OUTPUT_CAP)
    }
}

/// Converts characters to tokens with a ratio calibrated from the server's
/// reported `prompt_tokens` for the characters actually sent.
#[derive(Debug, Clone, Copy)]
pub struct Estimator {
    chars_per_token: f64,
    calibrated: bool,
}

impl Default for Estimator {
    fn default() -> Self {
        Self {
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            calibrated: false,
        }
    }
}

impl Estimator {
    pub fn tokens(&self, chars: usize) -> usize {
        (chars as f64 / self.chars_per_token).ceil() as usize
    }

    pub fn chars(&self, tokens: usize) -> usize {
        (tokens as f64 * self.chars_per_token) as usize
    }

    /// Updates the ratio from one request: `chars_sent` characters were billed
    /// as `prompt_tokens`. Ignores empty reports.
    pub fn calibrate(&mut self, chars_sent: usize, prompt_tokens: u64) {
        if chars_sent == 0 || prompt_tokens == 0 {
            return;
        }
        let ratio = chars_sent as f64 / prompt_tokens as f64;
        self.chars_per_token = ratio.clamp(MIN_CHARS_PER_TOKEN, MAX_CHARS_PER_TOKEN);
        self.calibrated = true;
    }

    /// Where the ratio came from, for audit logs.
    pub fn source(&self) -> &'static str {
        if self.calibrated {
            "usage-calibrated"
        } else {
            "default-ratio"
        }
    }

    #[cfg(test)]
    pub fn chars_per_token(&self) -> f64 {
        self.chars_per_token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_tokens: usize, budget: Option<usize>) -> Config {
        let mut c = Config::default();
        c.agent.max_tokens = max_tokens;
        c.context.budget = budget;
        c
    }

    #[test]
    fn budget_matches_documented_table() {
        // docs/context.md, "Behavior across window sizes" (R = 4096).
        let b = Budget::new(32_768, &cfg(4096, None));
        assert_eq!(b.usable, 27_689);
        assert_eq!(b.operating, 27_689);
        assert_eq!(b.output_cap_tokens(), 2_215);
        assert_eq!(b.of(0.6), 16_613);

        let small = Budget::new(8_192, &cfg(4096, None));
        assert_eq!(small.usable, 3_584);
        assert_eq!(small.output_cap_tokens(), 1_000); // floor

        let capped = Budget::new(262_144, &cfg(4096, Some(96_000)));
        assert_eq!(capped.operating, 96_000);
        assert_eq!(capped.output_cap_tokens(), 7_680);
    }

    #[test]
    fn budget_never_exceeds_usable_or_hits_zero() {
        let b = Budget::new(32_768, &cfg(4096, Some(1_000_000)));
        assert_eq!(b.operating, b.usable);
        let tiny = Budget::new(1_000, &cfg(4096, None));
        assert_eq!(tiny.usable, 1);
    }

    #[test]
    fn estimator_calibrates_and_clamps() {
        let mut e = Estimator::default();
        assert_eq!(e.source(), "default-ratio");
        assert_eq!(e.tokens(300), 100);
        e.calibrate(4_000, 1_000);
        assert_eq!(e.tokens(4_000), 1_000);
        assert_eq!(e.source(), "usage-calibrated");
        e.calibrate(1_000_000, 1);
        assert_eq!(e.chars_per_token(), MAX_CHARS_PER_TOKEN);
        e.calibrate(0, 5); // ignored
        assert_eq!(e.chars_per_token(), MAX_CHARS_PER_TOKEN);
    }
}
