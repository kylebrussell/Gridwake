use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use gridwake_aoi::{CellCoord, GridAoi, GridAoiConfig, InterestIndex};
use gridwake_core::{ByteBudget, ClientId, EntityId, RegionId, SnapshotId, Tick, Vec3};
use gridwake_protocol::{ClientMessage, MetricsFrame, RoutedClientMessage, ServerMessage};
use gridwake_replication::{ReplicationGraph, VisibilityChange};
use gridwake_snapshot::{build_delta, AckTracker, DeltaOp, SnapshotFrame, SnapshotHistory};

#[derive(Clone, Copy, Debug)]
pub struct ServerConfig {
    pub tick_rate_hz: u16,
    pub cell_size: f32,
    pub default_interest_radius: f32,
    pub per_client_byte_budget: usize,
    pub snapshot_history: usize,
    pub lag_history_ticks: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tick_rate_hz: 20,
            cell_size: 32.0,
            default_interest_radius: 96.0,
            per_client_byte_budget: 1_200,
            snapshot_history: 32,
            lag_history_ticks: 64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerClient {
    pub id: ClientId,
    pub position: Vec3,
    pub interest_radius: f32,
}

#[derive(Clone, Debug)]
pub struct ServerEntity {
    pub id: EntityId,
    pub position: Vec3,
    pub payload: Vec<u8>,
    pub estimated_bytes: usize,
    pub base_priority: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellEvent {
    pub source: CellCoord,
    pub target: CellCoord,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellRoute {
    Local {
        owner: RegionId,
    },
    CrossRegion {
        source_owner: RegionId,
        target_owner: RegionId,
    },
    Unowned {
        source_owner: Option<RegionId>,
        target_owner: Option<RegionId>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedCellEvent {
    pub event: CellEvent,
    pub route: CellRoute,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagSample {
    pub tick: Tick,
    pub position: Vec3,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickMetrics {
    pub tick: Tick,
    pub clients: usize,
    pub entities: usize,
    pub aoi_candidates: usize,
    pub selected_updates: usize,
    pub exit_updates: usize,
    pub bytes_scheduled: usize,
    pub messages_sent: usize,
}

impl From<TickMetrics> for MetricsFrame {
    fn from(metrics: TickMetrics) -> Self {
        Self {
            tick: metrics.tick,
            clients: metrics.clients,
            entities: metrics.entities,
            aoi_candidates: metrics.aoi_candidates,
            selected_updates: metrics.selected_updates,
            bytes_scheduled: metrics.bytes_scheduled,
        }
    }
}

pub trait Transport {
    fn send(&mut self, client: ClientId, message: ServerMessage);

    fn drain_received(&mut self) -> Vec<RoutedClientMessage> {
        Vec::new()
    }
}

pub trait MetricsSink {
    fn record_tick(&mut self, metrics: &TickMetrics);
}

#[derive(Debug, Default)]
pub struct NoopMetrics;

impl MetricsSink for NoopMetrics {
    fn record_tick(&mut self, _metrics: &TickMetrics) {}
}

#[derive(Debug, Default)]
pub struct VecMetrics {
    pub ticks: Vec<TickMetrics>,
}

impl VecMetrics {
    pub fn clear(&mut self) {
        self.ticks.clear();
    }
}

impl MetricsSink for VecMetrics {
    fn record_tick(&mut self, metrics: &TickMetrics) {
        self.ticks.push(metrics.clone());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerStep {
    pub ticks_due: u32,
    pub saturated: bool,
}

#[derive(Clone, Debug)]
pub struct TickScheduler {
    tick_interval: Duration,
    max_ticks_per_step: u32,
    accumulated: Duration,
}

impl TickScheduler {
    pub fn from_hz(tick_rate_hz: u16, max_ticks_per_step: u32) -> Self {
        assert!(tick_rate_hz > 0);
        assert!(max_ticks_per_step > 0);
        Self {
            tick_interval: Duration::from_secs_f64(1.0 / f64::from(tick_rate_hz)),
            max_ticks_per_step,
            accumulated: Duration::ZERO,
        }
    }

    pub fn tick_interval(&self) -> Duration {
        self.tick_interval
    }

    pub fn accumulated(&self) -> Duration {
        self.accumulated
    }

    pub fn advance(&mut self, elapsed: Duration) -> SchedulerStep {
        self.accumulated += elapsed;
        let mut ticks_due = 0;

        while self.accumulated >= self.tick_interval && ticks_due < self.max_ticks_per_step {
            self.accumulated = self.accumulated.saturating_sub(self.tick_interval);
            ticks_due += 1;
        }

        let saturated = self.accumulated >= self.tick_interval;
        if saturated {
            self.accumulated = Duration::ZERO;
        }

        SchedulerStep {
            ticks_due,
            saturated,
        }
    }
}

#[derive(Debug, Default)]
pub struct FakeTransport {
    pub sent: Vec<(ClientId, ServerMessage)>,
    pub received: Vec<RoutedClientMessage>,
}

impl FakeTransport {
    pub fn clear(&mut self) {
        self.sent.clear();
    }

    pub fn push_received(&mut self, client: ClientId, message: ClientMessage) {
        self.received.push(RoutedClientMessage { client, message });
    }
}

impl Transport for FakeTransport {
    fn send(&mut self, client: ClientId, message: ServerMessage) {
        self.sent.push((client, message));
    }

    fn drain_received(&mut self) -> Vec<RoutedClientMessage> {
        self.received.drain(..).collect()
    }
}

#[derive(Debug)]
pub struct ServerRuntime {
    config: ServerConfig,
    tick: Tick,
    clients: HashMap<ClientId, ServerClient>,
    entities: HashMap<EntityId, ServerEntity>,
    cell_owners: HashMap<CellCoord, RegionId>,
    region_cell_events: HashMap<RegionId, Vec<RoutedCellEvent>>,
    unowned_cell_events: Vec<RoutedCellEvent>,
    aoi: GridAoi,
    replication: ReplicationGraph,
    history: SnapshotHistory,
    lag_history: HashMap<EntityId, VecDeque<LagSample>>,
    acks: AckTracker,
}

impl ServerRuntime {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            aoi: GridAoi::new(GridAoiConfig::new(config.cell_size)),
            history: SnapshotHistory::new(config.snapshot_history),
            config,
            tick: Tick::ZERO,
            clients: HashMap::new(),
            entities: HashMap::new(),
            cell_owners: HashMap::new(),
            region_cell_events: HashMap::new(),
            unowned_cell_events: Vec::new(),
            replication: ReplicationGraph::new(),
            lag_history: HashMap::new(),
            acks: AckTracker::default(),
        }
    }

    pub fn tick(&self) -> Tick {
        self.tick
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    pub fn cell_for_position(&self, position: Vec3) -> CellCoord {
        self.aoi.cell_for_position(position)
    }

    pub fn set_cell_owner(&mut self, cell: CellCoord, owner: RegionId) -> Option<RegionId> {
        self.cell_owners.insert(cell, owner)
    }

    pub fn clear_cell_owner(&mut self, cell: CellCoord) -> Option<RegionId> {
        self.cell_owners.remove(&cell)
    }

    pub fn cell_owner(&self, cell: CellCoord) -> Option<RegionId> {
        self.cell_owners.get(&cell).copied()
    }

    pub fn route_cell_event(
        &self,
        source: CellCoord,
        target: CellCoord,
        payload: impl Into<Vec<u8>>,
    ) -> RoutedCellEvent {
        let source_owner = self.cell_owner(source);
        let target_owner = self.cell_owner(target);
        let route = match (source_owner, target_owner) {
            (Some(source_owner), Some(target_owner)) if source_owner == target_owner => {
                CellRoute::Local {
                    owner: source_owner,
                }
            }
            (Some(source_owner), Some(target_owner)) => CellRoute::CrossRegion {
                source_owner,
                target_owner,
            },
            (source_owner, target_owner) => CellRoute::Unowned {
                source_owner,
                target_owner,
            },
        };

        RoutedCellEvent {
            event: CellEvent {
                source,
                target,
                payload: payload.into(),
            },
            route,
        }
    }

    pub fn route_event_between_positions(
        &self,
        source: Vec3,
        target: Vec3,
        payload: impl Into<Vec<u8>>,
    ) -> RoutedCellEvent {
        self.route_cell_event(
            self.cell_for_position(source),
            self.cell_for_position(target),
            payload,
        )
    }

    pub fn enqueue_cell_event(
        &mut self,
        source: CellCoord,
        target: CellCoord,
        payload: impl Into<Vec<u8>>,
    ) -> CellRoute {
        let routed = self.route_cell_event(source, target, payload);
        let route = routed.route;
        match route {
            CellRoute::Local { owner } => {
                self.region_cell_events
                    .entry(owner)
                    .or_default()
                    .push(routed);
            }
            CellRoute::CrossRegion { target_owner, .. } => {
                self.region_cell_events
                    .entry(target_owner)
                    .or_default()
                    .push(routed);
            }
            CellRoute::Unowned { .. } => {
                self.unowned_cell_events.push(routed);
            }
        }
        route
    }

    pub fn enqueue_event_between_positions(
        &mut self,
        source: Vec3,
        target: Vec3,
        payload: impl Into<Vec<u8>>,
    ) -> CellRoute {
        self.enqueue_cell_event(
            self.cell_for_position(source),
            self.cell_for_position(target),
            payload,
        )
    }

    pub fn pending_region_event_count(&self, owner: RegionId) -> usize {
        self.region_cell_events.get(&owner).map_or(0, Vec::len)
    }

    pub fn pending_unowned_event_count(&self) -> usize {
        self.unowned_cell_events.len()
    }

    pub fn drain_region_events(&mut self, owner: RegionId) -> Vec<RoutedCellEvent> {
        self.region_cell_events.remove(&owner).unwrap_or_default()
    }

    pub fn drain_unowned_cell_events(&mut self) -> Vec<RoutedCellEvent> {
        std::mem::take(&mut self.unowned_cell_events)
    }

    pub fn rewind_entity_position(&self, entity: EntityId, tick: Tick) -> Option<Vec3> {
        self.lag_history
            .get(&entity)?
            .iter()
            .find(|sample| sample.tick == tick)
            .map(|sample| sample.position)
    }

    pub fn latest_entity_position_sample(&self, entity: EntityId) -> Option<LagSample> {
        self.lag_history.get(&entity)?.back().copied()
    }

    pub fn connect_client(&mut self, id: ClientId, position: Vec3, interest_radius: Option<f32>) {
        let interest_radius = interest_radius.unwrap_or(self.config.default_interest_radius);
        let client = ServerClient {
            id,
            position,
            interest_radius,
        };
        self.clients.insert(id, client);
        self.aoi.insert_observer(id, position, interest_radius);
        self.replication.register_client(id);
    }

    pub fn disconnect_client(&mut self, id: ClientId) -> bool {
        self.aoi.remove_observer(id);
        self.replication.remove_client(id);
        self.clients.remove(&id).is_some()
    }

    pub fn update_client_position(&mut self, id: ClientId, position: Vec3) -> bool {
        let Some(client) = self.clients.get_mut(&id) else {
            return false;
        };
        client.position = position;
        self.aoi
            .update_observer(id, position, client.interest_radius)
    }

    pub fn spawn_entity(
        &mut self,
        id: EntityId,
        position: Vec3,
        payload: impl Into<Vec<u8>>,
        estimated_bytes: usize,
        base_priority: f32,
    ) {
        let payload = payload.into();
        let estimated_bytes = estimated_bytes.max(payload.len());
        let entity = ServerEntity {
            id,
            position,
            payload,
            estimated_bytes,
            base_priority,
        };
        self.entities.insert(id, entity);
        self.aoi.insert_entity(id, position);
        self.replication
            .upsert_entity(id, estimated_bytes, base_priority);
    }

    pub fn despawn_entity(&mut self, id: EntityId) -> bool {
        self.aoi.remove_entity(id);
        self.replication.remove_entity(id);
        self.lag_history.remove(&id);
        self.entities.remove(&id).is_some()
    }

    pub fn move_entity(&mut self, id: EntityId, position: Vec3) -> bool {
        let Some(entity) = self.entities.get_mut(&id) else {
            return false;
        };
        entity.position = position;
        self.aoi.update_entity(id, position);
        self.replication.mark_dirty(id)
    }

    pub fn set_entity_payload(&mut self, id: EntityId, payload: impl Into<Vec<u8>>) -> bool {
        let Some(entity) = self.entities.get_mut(&id) else {
            return false;
        };
        entity.payload = payload.into();
        entity.estimated_bytes = entity.estimated_bytes.max(entity.payload.len());
        self.replication.mark_dirty(id)
    }

    pub fn receive(&mut self, client: ClientId, message: ClientMessage) {
        match message {
            ClientMessage::AckSnapshot { sequence } => self.acks.ack(client, sequence),
            ClientMessage::Input { .. } => {}
        }
    }

    pub fn pump_transport<T: Transport>(&mut self, transport: &mut T) -> usize {
        let messages = transport.drain_received();
        let count = messages.len();
        for RoutedClientMessage { client, message } in messages {
            if self.clients.contains_key(&client) {
                self.receive(client, message);
            }
        }
        count
    }

    pub fn advance_tick<T: Transport>(&mut self, transport: &mut T) -> TickMetrics {
        let tick = self.tick.advance();
        let snapshot_id = SnapshotId::new(tick.raw());
        let mut metrics = TickMetrics {
            tick,
            clients: self.clients.len(),
            entities: self.entities.len(),
            ..TickMetrics::default()
        };

        let mut clients: Vec<_> = self.clients.keys().copied().collect();
        clients.sort_unstable();

        for client in clients {
            let visible = self.aoi.query_observer(client).unwrap_or_default();
            metrics.aoi_candidates += visible.len();
            let visibility = self.replication.set_visibility(client, visible);
            let selection = self
                .replication
                .select_for_client(client, ByteBudget::new(self.config.per_client_byte_budget));

            metrics.selected_updates += selection.updates.len();
            metrics.exit_updates += visibility.exited.len();

            if selection.updates.is_empty() && visibility.exited.is_empty() {
                continue;
            }

            let frame = self.snapshot_frame_for_selection(
                client,
                snapshot_id,
                &visibility,
                &selection.updates,
            );
            let baseline = self
                .acks
                .latest(client)
                .and_then(|sequence| self.history.get(client, sequence));
            let mut delta = build_delta(baseline, &frame);
            if baseline.is_none() {
                delta.ops.extend(
                    visibility
                        .exited
                        .iter()
                        .copied()
                        .map(|entity| DeltaOp::DespawnOrExit { entity }),
                );
            }

            metrics.bytes_scheduled += delta.estimated_bytes();

            if delta.ops.is_empty() {
                continue;
            }

            self.history.insert(client, frame);
            transport.send(client, ServerMessage::SnapshotDelta(delta));
            metrics.messages_sent += 1;
        }

        self.record_lag_history(tick);

        metrics
    }

    pub fn advance_tick_with_metrics<T: Transport, M: MetricsSink>(
        &mut self,
        transport: &mut T,
        metrics_sink: &mut M,
    ) -> TickMetrics {
        let metrics = self.advance_tick(transport);
        metrics_sink.record_tick(&metrics);
        metrics
    }

    pub fn advance_elapsed<T: Transport, M: MetricsSink>(
        &mut self,
        scheduler: &mut TickScheduler,
        elapsed: Duration,
        transport: &mut T,
        metrics_sink: &mut M,
    ) -> Vec<TickMetrics> {
        self.pump_transport(transport);
        let step = scheduler.advance(elapsed);
        let mut metrics = Vec::with_capacity(step.ticks_due as usize);

        for _ in 0..step.ticks_due {
            metrics.push(self.advance_tick_with_metrics(transport, metrics_sink));
        }

        metrics
    }

    fn snapshot_frame_for_selection(
        &self,
        client: ClientId,
        snapshot_id: SnapshotId,
        visibility: &VisibilityChange,
        updates: &[gridwake_replication::SelectedUpdate],
    ) -> SnapshotFrame {
        let mut frame = self
            .history
            .latest(client)
            .cloned()
            .unwrap_or_else(|| SnapshotFrame::new(snapshot_id));
        frame.sequence = snapshot_id;

        for entity in &visibility.exited {
            frame.entities.remove(entity);
        }

        for update in updates {
            if let Some(entity) = self.entities.get(&update.entity) {
                frame.insert(update.entity, entity.payload.clone());
            }
        }
        frame
    }

    fn record_lag_history(&mut self, tick: Tick) {
        for entity in self.entities.values() {
            let samples = self.lag_history.entry(entity.id).or_default();
            samples.push_back(LagSample {
                tick,
                position: entity.position,
            });
            while samples.len() > self.config.lag_history_ticks {
                samples.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridwake_snapshot::DeltaOp;

    fn payload(id: u64) -> Vec<u8> {
        id.to_le_bytes().to_vec()
    }

    #[test]
    fn tick_sends_only_aoi_relevant_entities() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 20.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let near = EntityId::new(10);
        let far = EntityId::new(11);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity(near, Vec3::new(5.0, 0.0, 0.0), payload(near.raw()), 16, 1.0);
        runtime.spawn_entity(far, Vec3::new(100.0, 0.0, 0.0), payload(far.raw()), 16, 1.0);

        let metrics = runtime.advance_tick(&mut transport);

        assert_eq!(metrics.selected_updates, 1);
        assert_eq!(transport.sent.len(), 1);
        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(delta.ops[0].entity(), near);
    }

    #[test]
    fn tick_respects_client_budget() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 16,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let high = EntityId::new(1);
        let low = EntityId::new(2);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity(low, Vec3::ZERO, payload(low.raw()), 16, 1.0);
        runtime.spawn_entity(high, Vec3::ZERO, payload(high.raw()), 16, 10.0);

        let metrics = runtime.advance_tick(&mut transport);

        assert_eq!(metrics.selected_updates, 1);
        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(delta.ops[0].entity(), high);
    }

    #[test]
    fn movement_out_of_interest_sends_exit() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity(entity, Vec3::ZERO, payload(entity.raw()), 16, 1.0);
        runtime.advance_tick(&mut transport);
        transport.clear();

        runtime.move_entity(entity, Vec3::new(50.0, 0.0, 0.0));
        let metrics = runtime.advance_tick(&mut transport);

        assert_eq!(metrics.exit_updates, 1);
        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(delta.ops, vec![DeltaOp::DespawnOrExit { entity }]);
    }

    #[test]
    fn acked_retained_snapshot_is_advertised_as_baseline() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity(entity, Vec3::ZERO, payload(entity.raw()), 16, 1.0);
        runtime.advance_tick(&mut transport);
        runtime.receive(
            client,
            ClientMessage::AckSnapshot {
                sequence: SnapshotId::new(1),
            },
        );
        transport.clear();

        runtime.set_entity_payload(entity, b"changed".to_vec());
        runtime.advance_tick(&mut transport);

        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(delta.baseline, Some(SnapshotId::new(1)));
    }

    #[test]
    fn acked_baseline_delta_includes_changes_from_dropped_snapshots() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let first = EntityId::new(1);
        let second = EntityId::new(2);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity(first, Vec3::ZERO, b"first-0".to_vec(), 16, 1.0);
        runtime.spawn_entity(second, Vec3::ZERO, b"second-0".to_vec(), 16, 1.0);
        runtime.advance_tick(&mut transport);
        runtime.receive(
            client,
            ClientMessage::AckSnapshot {
                sequence: SnapshotId::new(1),
            },
        );
        transport.clear();

        runtime.set_entity_payload(first, b"first-1".to_vec());
        runtime.advance_tick(&mut transport);
        transport.clear();

        runtime.set_entity_payload(second, b"second-1".to_vec());
        runtime.advance_tick(&mut transport);

        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(delta.baseline, Some(SnapshotId::new(1)));
        assert_eq!(
            delta.ops,
            vec![
                DeltaOp::Update {
                    entity: first,
                    payload: b"first-1".to_vec()
                },
                DeltaOp::Update {
                    entity: second,
                    payload: b"second-1".to_vec()
                }
            ]
        );
    }

    #[test]
    fn inbound_transport_ack_is_pumped_before_due_ticks() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            tick_rate_hz: 10,
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();
        let mut metrics = VecMetrics::default();
        let mut scheduler = TickScheduler::from_hz(10, 4);

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity(entity, Vec3::ZERO, payload(entity.raw()), 16, 1.0);
        runtime.advance_tick(&mut transport);
        transport.clear();
        transport.push_received(
            client,
            ClientMessage::AckSnapshot {
                sequence: SnapshotId::new(1),
            },
        );
        runtime.set_entity_payload(entity, b"changed".to_vec());

        let due = runtime.advance_elapsed(
            &mut scheduler,
            Duration::from_millis(100),
            &mut transport,
            &mut metrics,
        );

        assert_eq!(due.len(), 1);
        assert_eq!(metrics.ticks.len(), 1);
        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(delta.baseline, Some(SnapshotId::new(1)));
    }

    #[test]
    fn tick_scheduler_accumulates_partial_time_and_caps_catch_up() {
        let mut scheduler = TickScheduler::from_hz(20, 3);

        assert_eq!(
            scheduler.advance(Duration::from_millis(25)),
            SchedulerStep {
                ticks_due: 0,
                saturated: false
            }
        );
        assert_eq!(
            scheduler.advance(Duration::from_millis(25)),
            SchedulerStep {
                ticks_due: 1,
                saturated: false
            }
        );
        assert_eq!(
            scheduler.advance(Duration::from_millis(1_000)),
            SchedulerStep {
                ticks_due: 3,
                saturated: true
            }
        );
        assert_eq!(scheduler.accumulated(), Duration::ZERO);
    }

    #[test]
    fn elapsed_advance_records_one_metric_per_due_tick() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            tick_rate_hz: 20,
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();
        let mut metrics = VecMetrics::default();
        let mut scheduler = TickScheduler::from_hz(20, 8);

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity(entity, Vec3::ZERO, payload(entity.raw()), 16, 1.0);

        let due = runtime.advance_elapsed(
            &mut scheduler,
            Duration::from_millis(150),
            &mut transport,
            &mut metrics,
        );

        assert_eq!(due.len(), 3);
        assert_eq!(metrics.ticks.len(), 3);
        assert_eq!(runtime.tick(), Tick::new(3));
    }

    #[test]
    fn lag_history_can_rewind_entity_positions_by_tick() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.spawn_entity(
            entity,
            Vec3::new(1.0, 0.0, 0.0),
            payload(entity.raw()),
            16,
            1.0,
        );
        runtime.advance_tick(&mut transport);
        runtime.move_entity(entity, Vec3::new(2.0, 0.0, 0.0));
        runtime.advance_tick(&mut transport);
        runtime.move_entity(entity, Vec3::new(3.0, 0.0, 0.0));
        runtime.advance_tick(&mut transport);

        assert_eq!(
            runtime.rewind_entity_position(entity, Tick::new(1)),
            Some(Vec3::new(1.0, 0.0, 0.0))
        );
        assert_eq!(
            runtime.rewind_entity_position(entity, Tick::new(2)),
            Some(Vec3::new(2.0, 0.0, 0.0))
        );
        assert_eq!(
            runtime.latest_entity_position_sample(entity),
            Some(LagSample {
                tick: Tick::new(3),
                position: Vec3::new(3.0, 0.0, 0.0)
            })
        );
    }

    #[test]
    fn lag_history_drops_stale_samples_and_clears_on_despawn() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            lag_history_ticks: 2,
            ..ServerConfig::default()
        });
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.spawn_entity(
            entity,
            Vec3::new(1.0, 0.0, 0.0),
            payload(entity.raw()),
            16,
            1.0,
        );
        runtime.advance_tick(&mut transport);
        runtime.move_entity(entity, Vec3::new(2.0, 0.0, 0.0));
        runtime.advance_tick(&mut transport);
        runtime.move_entity(entity, Vec3::new(3.0, 0.0, 0.0));
        runtime.advance_tick(&mut transport);

        assert_eq!(runtime.rewind_entity_position(entity, Tick::new(1)), None);
        assert_eq!(
            runtime.rewind_entity_position(entity, Tick::new(2)),
            Some(Vec3::new(2.0, 0.0, 0.0))
        );

        assert!(runtime.despawn_entity(entity));
        assert_eq!(runtime.latest_entity_position_sample(entity), None);
    }

    #[test]
    fn cell_event_routing_distinguishes_local_cross_region_and_unowned() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            cell_size: 10.0,
            ..ServerConfig::default()
        });
        let left = runtime.cell_for_position(Vec3::new(1.0, 0.0, 0.0));
        let right = runtime.cell_for_position(Vec3::new(15.0, 0.0, 0.0));
        let far = runtime.cell_for_position(Vec3::new(35.0, 0.0, 0.0));
        let region_a = RegionId::new(1);
        let region_b = RegionId::new(2);

        runtime.set_cell_owner(left, region_a);
        runtime.set_cell_owner(right, region_a);
        runtime.set_cell_owner(far, region_b);

        assert_eq!(
            runtime
                .route_cell_event(left, right, b"same".to_vec())
                .route,
            CellRoute::Local { owner: region_a }
        );
        assert_eq!(
            runtime.route_cell_event(left, far, b"cross".to_vec()).route,
            CellRoute::CrossRegion {
                source_owner: region_a,
                target_owner: region_b
            }
        );

        runtime.clear_cell_owner(far);
        assert_eq!(
            runtime
                .route_cell_event(left, far, b"missing".to_vec())
                .route,
            CellRoute::Unowned {
                source_owner: Some(region_a),
                target_owner: None
            }
        );
    }

    #[test]
    fn routed_cell_events_are_enqueued_for_target_region_or_unowned_queue() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            cell_size: 10.0,
            ..ServerConfig::default()
        });
        let left = runtime.cell_for_position(Vec3::new(1.0, 0.0, 0.0));
        let right = runtime.cell_for_position(Vec3::new(15.0, 0.0, 0.0));
        let far = runtime.cell_for_position(Vec3::new(35.0, 0.0, 0.0));
        let region_a = RegionId::new(1);
        let region_b = RegionId::new(2);

        runtime.set_cell_owner(left, region_a);
        runtime.set_cell_owner(right, region_a);
        runtime.set_cell_owner(far, region_b);

        assert_eq!(
            runtime.enqueue_cell_event(left, right, b"local".to_vec()),
            CellRoute::Local { owner: region_a }
        );
        assert_eq!(
            runtime.enqueue_cell_event(left, far, b"cross".to_vec()),
            CellRoute::CrossRegion {
                source_owner: region_a,
                target_owner: region_b
            }
        );
        runtime.clear_cell_owner(far);
        assert_eq!(
            runtime.enqueue_cell_event(left, far, b"unowned".to_vec()),
            CellRoute::Unowned {
                source_owner: Some(region_a),
                target_owner: None
            }
        );

        assert_eq!(runtime.pending_region_event_count(region_a), 1);
        assert_eq!(runtime.pending_region_event_count(region_b), 1);
        assert_eq!(runtime.pending_unowned_event_count(), 1);

        let region_a_events = runtime.drain_region_events(region_a);
        let region_b_events = runtime.drain_region_events(region_b);
        let unowned_events = runtime.drain_unowned_cell_events();

        assert_eq!(region_a_events[0].event.payload, b"local".to_vec());
        assert_eq!(region_b_events[0].event.payload, b"cross".to_vec());
        assert_eq!(unowned_events[0].event.payload, b"unowned".to_vec());
        assert_eq!(runtime.pending_region_event_count(region_a), 0);
        assert_eq!(runtime.pending_region_event_count(region_b), 0);
        assert_eq!(runtime.pending_unowned_event_count(), 0);
    }
}
