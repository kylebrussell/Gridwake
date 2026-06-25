use std::collections::{BTreeMap, HashMap, VecDeque};

use gridwake_core::{ClientId, EntityId, SnapshotId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFrame {
    pub sequence: SnapshotId,
    pub entities: BTreeMap<EntityId, Vec<u8>>,
}

impl SnapshotFrame {
    pub fn new(sequence: SnapshotId) -> Self {
        Self {
            sequence,
            entities: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, entity: EntityId, payload: impl Into<Vec<u8>>) {
        self.entities.insert(entity, payload.into());
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaOp {
    SpawnOrEnter { entity: EntityId, payload: Vec<u8> },
    Update { entity: EntityId, payload: Vec<u8> },
    DespawnOrExit { entity: EntityId },
}

impl DeltaOp {
    pub fn entity(&self) -> EntityId {
        match self {
            Self::SpawnOrEnter { entity, .. }
            | Self::Update { entity, .. }
            | Self::DespawnOrExit { entity } => *entity,
        }
    }

    pub fn estimated_bytes(&self) -> usize {
        match self {
            Self::SpawnOrEnter { payload, .. } | Self::Update { payload, .. } => payload.len(),
            Self::DespawnOrExit { .. } => 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaSnapshot {
    pub sequence: SnapshotId,
    pub baseline: Option<SnapshotId>,
    pub ops: Vec<DeltaOp>,
}

impl DeltaSnapshot {
    pub fn new(sequence: SnapshotId, baseline: Option<SnapshotId>, ops: Vec<DeltaOp>) -> Self {
        Self {
            sequence,
            baseline,
            ops,
        }
    }

    pub fn estimated_bytes(&self) -> usize {
        self.ops.iter().map(DeltaOp::estimated_bytes).sum()
    }
}

pub fn build_delta(baseline: Option<&SnapshotFrame>, current: &SnapshotFrame) -> DeltaSnapshot {
    let mut ops = Vec::new();
    let baseline_id = baseline.map(|frame| frame.sequence);

    if let Some(baseline) = baseline {
        for (&entity, payload) in &current.entities {
            match baseline.entities.get(&entity) {
                Some(previous) if previous == payload => {}
                Some(_) => ops.push(DeltaOp::Update {
                    entity,
                    payload: payload.clone(),
                }),
                None => ops.push(DeltaOp::SpawnOrEnter {
                    entity,
                    payload: payload.clone(),
                }),
            }
        }

        for &entity in baseline.entities.keys() {
            if !current.entities.contains_key(&entity) {
                ops.push(DeltaOp::DespawnOrExit { entity });
            }
        }
    } else {
        ops.extend(
            current
                .entities
                .iter()
                .map(|(&entity, payload)| DeltaOp::SpawnOrEnter {
                    entity,
                    payload: payload.clone(),
                }),
        );
    }

    DeltaSnapshot::new(current.sequence, baseline_id, ops)
}

#[derive(Debug)]
pub struct SnapshotHistory {
    capacity_per_client: usize,
    frames: HashMap<ClientId, VecDeque<SnapshotFrame>>,
}

impl SnapshotHistory {
    pub fn new(capacity_per_client: usize) -> Self {
        assert!(capacity_per_client > 0);
        Self {
            capacity_per_client,
            frames: HashMap::new(),
        }
    }

    pub fn insert(&mut self, client: ClientId, frame: SnapshotFrame) {
        let frames = self.frames.entry(client).or_default();
        frames.push_back(frame);
        while frames.len() > self.capacity_per_client {
            frames.pop_front();
        }
    }

    pub fn get(&self, client: ClientId, sequence: SnapshotId) -> Option<&SnapshotFrame> {
        self.frames
            .get(&client)?
            .iter()
            .find(|frame| frame.sequence == sequence)
    }

    pub fn latest(&self, client: ClientId) -> Option<&SnapshotFrame> {
        self.frames.get(&client)?.back()
    }
}

#[derive(Debug, Default)]
pub struct AckTracker {
    latest_acked: HashMap<ClientId, SnapshotId>,
}

impl AckTracker {
    pub fn ack(&mut self, client: ClientId, sequence: SnapshotId) {
        let current = self
            .latest_acked
            .entry(client)
            .or_insert(SnapshotId::new(0));
        if sequence > *current {
            *current = sequence;
        }
    }

    pub fn latest(&self, client: ClientId) -> Option<SnapshotId> {
        self.latest_acked.get(&client).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(sequence: u64, entities: &[(u64, &[u8])]) -> SnapshotFrame {
        let mut frame = SnapshotFrame::new(SnapshotId::new(sequence));
        for (entity, payload) in entities {
            frame.insert(EntityId::new(*entity), *payload);
        }
        frame
    }

    #[test]
    fn delta_without_baseline_spawns_everything() {
        let current = frame(1, &[(1, b"abc"), (2, b"def")]);

        let delta = build_delta(None, &current);

        assert_eq!(delta.baseline, None);
        assert_eq!(delta.ops.len(), 2);
        assert!(matches!(delta.ops[0], DeltaOp::SpawnOrEnter { .. }));
    }

    #[test]
    fn delta_tracks_update_enter_and_exit() {
        let baseline = frame(1, &[(1, b"same"), (2, b"old"), (3, b"gone")]);
        let current = frame(2, &[(1, b"same"), (2, b"new"), (4, b"enter")]);

        let delta = build_delta(Some(&baseline), &current);

        assert_eq!(delta.baseline, Some(SnapshotId::new(1)));
        assert_eq!(
            delta.ops,
            vec![
                DeltaOp::Update {
                    entity: EntityId::new(2),
                    payload: b"new".to_vec()
                },
                DeltaOp::SpawnOrEnter {
                    entity: EntityId::new(4),
                    payload: b"enter".to_vec()
                },
                DeltaOp::DespawnOrExit {
                    entity: EntityId::new(3)
                }
            ]
        );
    }

    #[test]
    fn history_drops_stale_baselines() {
        let client = ClientId::new(1);
        let mut history = SnapshotHistory::new(2);

        history.insert(client, frame(1, &[(1, b"a")]));
        history.insert(client, frame(2, &[(1, b"b")]));
        history.insert(client, frame(3, &[(1, b"c")]));

        assert!(history.get(client, SnapshotId::new(1)).is_none());
        assert!(history.get(client, SnapshotId::new(2)).is_some());
        assert_eq!(history.latest(client).unwrap().sequence, SnapshotId::new(3));
    }

    #[test]
    fn ack_tracker_keeps_latest_sequence() {
        let client = ClientId::new(1);
        let mut tracker = AckTracker::default();

        tracker.ack(client, SnapshotId::new(10));
        tracker.ack(client, SnapshotId::new(9));

        assert_eq!(tracker.latest(client), Some(SnapshotId::new(10)));
    }
}
