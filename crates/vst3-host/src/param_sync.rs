use plugin_host_api::{ParamId, ParamInfo};
use std::sync::atomic::{AtomicU64, Ordering};

/// Completed DSP values for main-thread display; intermediate automation points may coalesce.
pub(crate) struct ParamFeedback(Vec<(ParamId, AtomicU64)>);

impl ParamFeedback {
    pub(crate) fn new(params: &[ParamInfo]) -> Self {
        let mut values: Vec<_> = params
            .iter()
            .map(|p| (p.id, AtomicU64::new(f64::NAN.to_bits())))
            .collect();
        values.sort_unstable_by_key(|(id, _)| *id);
        Self(values)
    }

    pub(crate) fn publish(&self, id: u32, value: f64) {
        if let Ok(index) = self.0.binary_search_by_key(&ParamId(id), |(id, _)| *id) {
            self.0[index].1.store(value.to_bits(), Ordering::Release);
        }
    }

    pub(crate) fn drain(&self, mut apply: impl FnMut(ParamId, f64)) {
        for (id, value) in &self.0 {
            let value = f64::from_bits(value.swap(f64::NAN.to_bits(), Ordering::AcqRel));
            if value.is_finite() {
                apply(*id, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A newer publication during main-thread delivery remains available for the next tick.
    #[test]
    fn delivery_does_not_clear_a_later_value() {
        let feedback = ParamFeedback(vec![(ParamId(1), AtomicU64::new(f64::NAN.to_bits()))]);
        feedback.publish(1, 0.2);
        feedback.publish(1, 0.4);
        feedback.drain(|id, value| {
            assert_eq!(value, 0.4);
            feedback.publish(id.0, 0.8);
        });
        let mut values = Vec::new();
        feedback.drain(|_, value| values.push(value));
        feedback.drain(|_, _| panic!("already delivered"));
        assert_eq!(values, [0.8]);
    }
}
