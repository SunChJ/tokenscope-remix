//! Codex-specific token accounting.
//!
//! Codex `token_count` events expose two different views of usage:
//! `last_token_usage` is the exact latest upstream response, while
//! `total_token_usage` is the accumulated session snapshot. The same positive
//! `last_token_usage` can be emitted again when only rate limits change, so the
//! accumulated snapshot is the authority for whether a new response occurred.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: i64,
    pub(crate) cached_input_tokens: i64,
    pub(crate) cache_write_input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) reasoning_output_tokens: i64,
    pub(crate) total_tokens: i64,
}

impl TokenUsage {
    fn from_value(value: &Value) -> Self {
        let number = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_i64)
                .unwrap_or_default()
                .max(0)
        };
        Self {
            input_tokens: number("input_tokens"),
            cached_input_tokens: number("cached_input_tokens"),
            cache_write_input_tokens: number("cache_write_input_tokens"),
            output_tokens: number("output_tokens"),
            reasoning_output_tokens: number("reasoning_output_tokens"),
            total_tokens: number("total_tokens"),
        }
    }

    fn has_response_usage(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }

    fn event_id(&self, turn_id: &str) -> String {
        if turn_id.is_empty() {
            return String::new();
        }
        format!(
            "codex:{turn_id}@{}:{}:{}:{}:{}:{}",
            self.input_tokens,
            self.cached_input_tokens,
            self.cache_write_input_tokens,
            self.output_tokens,
            self.reasoning_output_tokens,
            self.total_tokens,
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct State {
    last_total: Option<TokenUsage>,
}

impl State {
    /// Advance the cumulative cursor without producing usage. Fork heads need
    /// this while their replayed lines are otherwise ignored.
    pub(crate) fn remember_total(&mut self, info: Option<&Value>) {
        if let Some(total) = info
            .and_then(|value| value.get("total_token_usage"))
            .map(TokenUsage::from_value)
        {
            self.last_total = Some(total);
        }
    }

    pub(crate) fn observe(&mut self, info: Option<&Value>, turn_id: &str) -> TokenCountOutcome {
        let Some(info) = info else {
            return TokenCountOutcome::NoUsage;
        };
        let last = info
            .get("last_token_usage")
            .map(TokenUsage::from_value)
            .unwrap_or_default();
        let total = info.get("total_token_usage").map(TokenUsage::from_value);
        let previous = total
            .as_ref()
            .and_then(|current| self.last_total.replace(current.clone()));

        // Local context estimates populate only total_tokens. They are useful
        // for compaction, but are not billable upstream usage.
        if !last.has_response_usage() {
            return TokenCountOutcome::NoUsage;
        }

        // Rate-limit-only TokenCount events retain the previous positive
        // last_token_usage. An unchanged accumulated snapshot proves that no
        // new upstream response completed.
        if previous
            .as_ref()
            .zip(total.as_ref())
            .is_some_and(|(before, current)| before == current)
        {
            return TokenCountOutcome::RepeatedSnapshot;
        }

        let event_id = total
            .as_ref()
            .map(|snapshot| snapshot.event_id(turn_id))
            // Old/incomplete logs without a total snapshot cannot be safely
            // deduplicated across files. Keep their usage rather than risk
            // silently dropping two equally-sized genuine responses.
            .unwrap_or_default();

        let raw_input = last.input_tokens.max(0);
        let cache_read = last.cached_input_tokens.clamp(0, raw_input);
        let cache_write = last
            .cache_write_input_tokens
            .clamp(0, raw_input - cache_read);
        let input = raw_input - cache_read - cache_write;
        let output = last.output_tokens.max(0);
        let reasoning = last.reasoning_output_tokens.clamp(0, output);

        TokenCountOutcome::Usage(Usage {
            event_id,
            input_tokens: input,
            cache_write_input_tokens: cache_write,
            cache_read_input_tokens: cache_read,
            output_tokens: output,
            reasoning_output_tokens: reasoning,
            raw_input_tokens: raw_input,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Usage {
    pub(crate) event_id: String,
    pub(crate) input_tokens: i64,
    pub(crate) cache_write_input_tokens: i64,
    pub(crate) cache_read_input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) reasoning_output_tokens: i64,
    pub(crate) raw_input_tokens: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TokenCountOutcome {
    NoUsage,
    RepeatedSnapshot,
    Usage(Usage),
}

/// Stable across Codex fork/replay restamping because the outer JSONL
/// timestamp is intentionally not part of the fingerprint.
pub(crate) fn token_count_fingerprint(
    turn_id: &str,
    info: Option<&Value>,
    rate_limits: Option<&Value>,
) -> String {
    let encoded = serde_json::to_vec(&(turn_id, info, rate_limits)).unwrap_or_default();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in encoded {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("codex-token-count:{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(last_input: i64, last_cached: i64, last_write: i64, total_input: i64) -> Value {
        json!({
            "last_token_usage": {
                "input_tokens": last_input,
                "cached_input_tokens": last_cached,
                "cache_write_input_tokens": last_write,
                "output_tokens": 10,
                "reasoning_output_tokens": 4,
                "total_tokens": last_input + 10
            },
            "total_token_usage": {
                "input_tokens": total_input,
                "cached_input_tokens": last_cached,
                "cache_write_input_tokens": last_write,
                "output_tokens": 10,
                "reasoning_output_tokens": 4,
                "total_tokens": total_input + 10
            }
        })
    }

    #[test]
    fn splits_cache_read_and_write_out_of_codex_input() {
        let mut state = State::default();
        let TokenCountOutcome::Usage(usage) =
            state.observe(Some(&info(100, 40, 50, 100)), "turn-a")
        else {
            panic!("expected usage");
        };

        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_read_input_tokens, 40);
        assert_eq!(usage.cache_write_input_tokens, 50);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.reasoning_output_tokens, 4);
        assert_eq!(usage.raw_input_tokens, 100);
        assert_eq!(
            usage.input_tokens
                + usage.cache_read_input_tokens
                + usage.cache_write_input_tokens
                + usage.output_tokens,
            110
        );
    }

    #[test]
    fn rejects_positive_last_usage_when_total_snapshot_is_unchanged() {
        let mut state = State::default();
        let first = info(100, 40, 0, 100);
        assert!(matches!(
            state.observe(Some(&first), "turn-a"),
            TokenCountOutcome::Usage(_)
        ));
        assert_eq!(
            state.observe(Some(&first), "turn-b"),
            TokenCountOutcome::RepeatedSnapshot
        );
    }

    #[test]
    fn cumulative_snapshot_makes_ids_stable_but_not_position_based() {
        let first = info(100, 40, 0, 100);
        let second = info(100, 40, 0, 200);

        let mut original = State::default();
        let TokenCountOutcome::Usage(first_original) = original.observe(Some(&first), "turn-a")
        else {
            panic!("expected usage");
        };
        let TokenCountOutcome::Usage(second_original) = original.observe(Some(&second), "turn-a")
        else {
            panic!("expected usage");
        };

        let mut replay = State::default();
        let TokenCountOutcome::Usage(first_replay) = replay.observe(Some(&first), "turn-a") else {
            panic!("expected replay candidate");
        };

        assert_eq!(first_original.event_id, first_replay.event_id);
        assert_ne!(first_original.event_id, second_original.event_id);
    }

    #[test]
    fn remembers_replayed_total_as_the_incremental_baseline() {
        let first = info(100, 40, 0, 100);
        let mut state = State::default();
        state.remember_total(Some(&first));

        assert_eq!(
            state.observe(Some(&first), "child-turn"),
            TokenCountOutcome::RepeatedSnapshot
        );
    }
}
