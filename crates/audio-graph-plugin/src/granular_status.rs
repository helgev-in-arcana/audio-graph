use audio_graph_engine::{GranularStatus, MAX_GRANULARS, NodeId};
use std::sync::atomic::{AtomicU64, Ordering};

/// The upper word identifies the node; fourteen bits per duration cover 0..10 seconds in ms.
const EMPTY: u64 = (u32::MAX as u64) << 32;

struct Cell {
    identity: AtomicU64,
    selected: [AtomicU64; 8],
    revisions: [AtomicU64; 8],
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            identity: AtomicU64::new(EMPTY),
            selected: std::array::from_fn(|_| AtomicU64::new(f64::NAN.to_bits())),
            revisions: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

pub struct LiveGranular {
    expected: AtomicU64,
    publication: AtomicU64,
    sequence: AtomicU64,
    values: [Cell; MAX_GRANULARS],
}

impl Default for LiveGranular {
    fn default() -> Self {
        Self {
            expected: AtomicU64::new(u64::MAX),
            publication: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            values: std::array::from_fn(|_| Cell::default()),
        }
    }
}

impl LiveGranular {
    pub fn expect(&self, publication: u64) {
        self.expected.store(publication, Ordering::SeqCst);
    }

    pub fn report(
        &self,
        publication: u64,
        statuses: impl Iterator<Item = (NodeId, GranularStatus)>,
    ) {
        // One audio writer brackets the cells so a reader cannot pair a value with another edit's revision.
        self.sequence.fetch_add(1, Ordering::SeqCst);
        let mut statuses = statuses;
        for cell in &self.values {
            let packed = statuses.next().map_or(EMPTY, |(node, status)| {
                for target in 0..8 {
                    cell.selected[target].store(
                        status.selected[target].unwrap_or(f64::NAN).to_bits(),
                        Ordering::SeqCst,
                    );
                    cell.revisions[target].store(status.revisions[target], Ordering::SeqCst);
                }
                let ms = |v: f64| (v * 1000.0).round().clamp(0.0, 16_383.0) as u64;
                ((node as u64) << 32)
                    | ms(status.history_seconds)
                    | (ms(status.recorded_seconds) << 14)
                    | ((status.recording as u64) << 28)
                    | ((status.playing as u64) << 29)
                    | ((status.loop_inverted as u64) << 30)
            });
            cell.identity.store(packed, Ordering::SeqCst);
        }
        self.publication.store(publication, Ordering::SeqCst);
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }

    pub fn read(&self) -> Vec<(NodeId, GranularStatus)> {
        for _ in 0..3 {
            let sequence = self.sequence.load(Ordering::SeqCst);
            if sequence & 1 != 0 {
                continue;
            }
            let publication = self.publication.load(Ordering::SeqCst);
            if publication == 0 || publication != self.expected.load(Ordering::SeqCst) {
                return Vec::new();
            }
            let result = self
                .values
                .iter()
                .filter_map(|cell| {
                    let packed = cell.identity.load(Ordering::SeqCst);
                    let node = (packed >> 32) as u32;
                    (node != u32::MAX).then(|| {
                        (
                            node,
                            GranularStatus {
                                history_seconds: (packed & 0x3fff) as f64 / 1000.0,
                                recorded_seconds: ((packed >> 14) & 0x3fff) as f64 / 1000.0,
                                recording: packed & (1 << 28) != 0,
                                playing: packed & (1 << 29) != 0,
                                loop_inverted: packed & (1 << 30) != 0,
                                selected: std::array::from_fn(|target| {
                                    let value = f64::from_bits(
                                        cell.selected[target].load(Ordering::SeqCst),
                                    );
                                    value.is_finite().then_some(value)
                                }),
                                revisions: std::array::from_fn(|target| {
                                    cell.revisions[target].load(Ordering::SeqCst)
                                }),
                            },
                        )
                    })
                })
                .collect();
            if self.sequence.load(Ordering::SeqCst) == sequence
                && self.expected.load(Ordering::SeqCst) == publication
            {
                return result;
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_node_identity_are_reported_only_for_the_expected_publication() {
        let live = LiveGranular::default();
        let status = GranularStatus {
            history_seconds: 10.0,
            recorded_seconds: 1.234,
            recording: true,
            playing: false,
            ..Default::default()
        };
        live.expect(1);
        live.report(1, [(42, status)].into_iter());
        assert_eq!(live.read(), vec![(42, status)]);
        live.expect(2);
        assert!(live.read().is_empty());
        live.report(2, [(7, status)].into_iter());
        assert_eq!(live.read(), vec![(7, status)]);
        live.report(2, std::iter::empty());
        assert!(live.read().is_empty());
    }
    #[test]
    fn reports_keep_selected_values_and_edit_revisions_in_the_same_snapshot() {
        let live = std::sync::Arc::new(LiveGranular::default());
        live.expect(1);
        let writer = live.clone();
        let thread = std::thread::spawn(move || {
            for revision in 1..2000 {
                writer.report(
                    1,
                    [(
                        7,
                        GranularStatus {
                            revisions: [revision; 8],
                            selected: [Some(revision as f64); 8],
                            loop_inverted: revision % 2 == 0,
                            ..Default::default()
                        },
                    )]
                    .into_iter(),
                );
            }
        });
        while !thread.is_finished() {
            for (node, status) in live.read() {
                assert_eq!(node, 7);
                for target in 0..8 {
                    assert_eq!(status.selected[target], Some(status.revisions[0] as f64));
                    assert_eq!(status.revisions[target], status.revisions[0]);
                }
                assert_eq!(status.loop_inverted, status.revisions[0] % 2 == 0);
            }
        }
        thread.join().unwrap();
        assert_eq!(live.read()[0].1.selected[3], Some(1999.0));
    }
}
