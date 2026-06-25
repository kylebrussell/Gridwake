use gridwake_core::{ClientId, SnapshotId, Tick};
use gridwake_snapshot::DeltaSnapshot;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMessage {
    AckSnapshot { sequence: SnapshotId },
    Input { payload: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerMessage {
    SnapshotDelta(DeltaSnapshot),
    Metrics(MetricsFrame),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetricsFrame {
    pub tick: Tick,
    pub clients: usize,
    pub entities: usize,
    pub aoi_candidates: usize,
    pub selected_updates: usize,
    pub bytes_scheduled: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedClientMessage {
    pub client: ClientId,
    pub message: ClientMessage,
}
