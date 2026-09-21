use crate::core::types::Usage;

#[derive(Default)]
pub(crate) struct ContextEstimate {
    extra_input: usize,
}

impl ContextEstimate {
    pub fn observe(&mut self, usage: Usage, estimated_input: usize) {
        let reported = usage
            .input_tokens
            .saturating_add(usage.cache_creation_input_tokens)
            .saturating_add(usage.cache_read_input_tokens);
        if reported > 0 {
            self.extra_input = usize::try_from(reported)
                .unwrap_or(usize::MAX)
                .saturating_sub(estimated_input);
        }
    }

    pub fn used(&self, estimated_input: usize) -> usize {
        estimated_input.saturating_add(self.extra_input)
    }

    pub fn reset(&mut self) {
        self.extra_input = 0;
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::context::ContextEstimate;
    use crate::core::types::Usage;

    #[test]
    fn observations_replace_the_correction_instead_of_accumulating() {
        let mut estimate = ContextEstimate::default();
        assert_eq!(estimate.used(100), 100);
        for (reported, expected) in [(900, 1000), (700, 800), (300, 700), (600, 700)] {
            estimate.observe(
                Usage {
                    input_tokens: reported,
                    ..Usage::default()
                },
                600,
            );
            assert_eq!(estimate.used(700), expected);
        }
    }

    #[test]
    fn output_only_usage_does_not_change_input_correction() {
        let mut estimate = ContextEstimate::default();
        estimate.observe(
            Usage {
                cache_read_input_tokens: 900,
                ..Usage::default()
            },
            600,
        );
        estimate.observe(
            Usage {
                output_tokens: u64::MAX,
                ..Usage::default()
            },
            1000,
        );
        assert_eq!(estimate.used(1000), 1300);
        estimate.reset();
        estimate.observe(Usage::default(), 1000);
        assert_eq!(estimate.used(1000), 1000);
    }

    #[test]
    fn extreme_usage_and_estimated_growth_saturate_without_wrapping() {
        let mut estimate = ContextEstimate::default();
        estimate.observe(
            Usage {
                input_tokens: u64::MAX,
                cache_creation_input_tokens: u64::MAX,
                cache_read_input_tokens: u64::MAX,
                ..Usage::default()
            },
            10,
        );
        assert_eq!(estimate.used(0), usize::MAX - 10);
        assert_eq!(estimate.used(11), usize::MAX);
        estimate.reset();
        assert_eq!(estimate.used(1), 1);
    }

    #[test]
    fn observed_input_includes_cached_tokens_and_unsampled_growth() {
        let mut estimate = ContextEstimate::default();
        estimate.observe(
            Usage {
                input_tokens: 400,
                output_tokens: 500,
                cache_creation_input_tokens: 200,
                cache_read_input_tokens: 400,
            },
            600,
        );
        assert_eq!(estimate.used(800), 1200);
        assert_eq!(estimate.used(500), 900);
        estimate.reset();
        assert_eq!(estimate.used(500), 500);
    }

    #[test]
    fn missing_usage_does_not_erase_observation() {
        let mut estimate = ContextEstimate::default();
        estimate.observe(
            Usage {
                input_tokens: 1000,
                ..Usage::default()
            },
            600,
        );
        estimate.observe(Usage::default(), 600);
        assert_eq!(estimate.used(600), 1000);
        estimate.observe(
            Usage {
                input_tokens: 500,
                ..Usage::default()
            },
            600,
        );
        assert_eq!(estimate.used(600), 600);
    }
}
