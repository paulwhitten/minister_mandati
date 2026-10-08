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
        // The reply reserve may take at most half the window. Without this, a
        // max_tokens close to the window leaves almost no room for context
        // (an 8k window with max_tokens 8192 gave a budget of 1 token and
        // every tool output was dropped at once). Requests then ask for less
        // than max_tokens when the prompt is large (`reply_limit`).
        let reserve = cfg.agent.max_tokens.min(window / 2);
        if reserve < cfg.agent.max_tokens {
            tracing::warn!(
                window,
                max_tokens = cfg.agent.max_tokens,
                reserve,
                "max_tokens is more than half the context window; reserving half the window for replies"
            );
        }
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

/// Where an exact token count came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CountSource {
    /// The server's `/tokenize` endpoint, before sending.
    Tokenize,
    /// The server's reported `usage.prompt_tokens`, after a response.
    Usage,
}

/// An exact count for a known request size.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    chars: usize,
    tokens: usize,
    source: CountSource,
}

/// Token counting anchored on exact server counts. The current size is the
/// last exact count plus an estimate of the characters added or removed since
/// (converted with a ratio calibrated from exact counts). With no change since
/// the last count it is exact. Before any exact count it falls back to a
/// conservative characters-per-token default.
#[derive(Debug, Clone, Copy)]
pub struct Estimator {
    chars_per_token: f64,
    anchor: Option<Anchor>,
}

impl Default for Estimator {
    fn default() -> Self {
        Self {
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            anchor: None,
        }
    }
}

impl Estimator {
    /// Estimated tokens for a fragment (e.g. one message) of `chars` characters.
    pub fn tokens(&self, chars: usize) -> usize {
        (chars as f64 / self.chars_per_token).ceil() as usize
    }

    pub fn chars(&self, tokens: usize) -> usize {
        (tokens as f64 * self.chars_per_token) as usize
    }

    /// Tokens for a whole request of `chars` characters: exact at the anchor,
    /// anchor plus the estimated difference elsewhere.
    pub fn total(&self, chars: usize) -> usize {
        match self.anchor {
            Some(a) => {
                let delta = (chars as f64 - a.chars as f64) / self.chars_per_token;
                (a.tokens as f64 + delta).ceil().max(0.0) as usize
            }
            None => self.tokens(chars),
        }
    }

    /// Records an exact count: a request of `chars` characters is `tokens`
    /// tokens. Also recalibrates the ratio used for differences. Ignores
    /// empty counts.
    pub fn record_exact(&mut self, chars: usize, tokens: usize, source: CountSource) {
        if chars == 0 || tokens == 0 {
            return;
        }
        let ratio = chars as f64 / tokens as f64;
        self.chars_per_token = ratio.clamp(MIN_CHARS_PER_TOKEN, MAX_CHARS_PER_TOKEN);
        self.anchor = Some(Anchor {
            chars,
            tokens,
            source,
        });
    }

    /// How `total(chars)` was obtained, for logs and transcripts.
    pub fn source(&self, chars: usize) -> &'static str {
        match self.anchor {
            None => "default-ratio",
            Some(a) if a.chars == chars => match a.source {
                CountSource::Tokenize => "tokenize",
                CountSource::Usage => "usage",
            },
            Some(a) => match a.source {
                CountSource::Tokenize => "tokenize+estimate",
                CountSource::Usage => "usage+estimate",
            },
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
    fn reply_reserve_is_at_most_half_the_window() {
        // An 8k window with max_tokens 8192 used to leave a budget of 1.
        let b = Budget::new(8_192, &cfg(8192, None));
        assert_eq!(b.usable, 8_192 - 4_096 - 512);
        let b = Budget::new(32_768, &cfg(8192, None));
        assert_eq!(b.usable, 32_768 - 8_192 - 983);
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
    fn estimator_is_exact_at_the_anchor_and_estimates_the_difference() {
        let mut e = Estimator::default();
        assert_eq!(e.source(300), "default-ratio");
        assert_eq!(e.total(300), 100);

        e.record_exact(4_000, 1_000, CountSource::Tokenize);
        assert_eq!(e.total(4_000), 1_000);
        assert_eq!(e.source(4_000), "tokenize");
        // 400 more characters at the calibrated 4 chars/token.
        assert_eq!(e.total(4_400), 1_100);
        assert_eq!(e.source(4_400), "tokenize+estimate");
        // Masking removed 2,000 characters.
        assert_eq!(e.total(2_000), 500);

        e.record_exact(5_000, 1_234, CountSource::Usage);
        assert_eq!(e.total(5_000), 1_234);
        assert_eq!(e.source(5_000), "usage");
    }

    #[test]
    fn estimator_clamps_odd_ratios_and_ignores_empty_counts() {
        let mut e = Estimator::default();
        e.record_exact(1_000_000, 1, CountSource::Usage);
        assert_eq!(e.chars_per_token(), MAX_CHARS_PER_TOKEN);
        e.record_exact(0, 5, CountSource::Usage);
        assert_eq!(e.chars_per_token(), MAX_CHARS_PER_TOKEN);
    }
}
