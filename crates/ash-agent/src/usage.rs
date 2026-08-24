use ash_core::{ModelUsage, Usage};

/// Reconciles cumulative provider usage chunks for one model request.
#[derive(Default)]
pub(crate) struct UsageAccumulator {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl UsageAccumulator {
    pub(crate) fn record(&mut self, usage: ModelUsage) {
        if usage.input_tokens > 0 {
            self.input_tokens = Some(self.input_tokens.map_or(usage.input_tokens, |current| {
                current.max(usage.input_tokens)
            }));
        }
        if usage.output_tokens > 0 {
            self.output_tokens = Some(self.output_tokens.map_or(usage.output_tokens, |current| {
                current.max(usage.output_tokens)
            }));
        }
    }

    pub(crate) fn finish(&self, input_tokens: u64, output_tokens: u64) -> Usage {
        Usage {
            input_tokens: self.input_tokens.unwrap_or(input_tokens),
            output_tokens: self.output_tokens.unwrap_or(output_tokens),
            tool_calls: 0,
            estimated: self.input_tokens.is_none()
                || (self.output_tokens.is_none() && output_tokens > 0),
        }
    }
}
