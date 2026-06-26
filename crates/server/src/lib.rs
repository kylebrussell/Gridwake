use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use gridwake_aoi::{CellCoord, GridAoi, GridAoiConfig, InterestIndex};
use gridwake_core::{ByteBudget, ClientId, EntityId, RegionId, SnapshotId, Tick, Vec3};
use gridwake_protocol::{
    decode_client_message_with_config, encode_client_message, encode_server_message, ClientMessage,
    CodecError, DecodeConfig, MetricsFrame, RoutedClientMessage, ServerMessage,
};
pub use gridwake_replication::NetworkLod;
use gridwake_replication::{NetworkLodBytes, ReplicationGraph, VisibilityChange};
use gridwake_snapshot::{build_delta, AckTracker, DeltaOp, SnapshotFrame, SnapshotHistory};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetworkLodPolicy {
    pub full_distance_ratio: f32,
    pub reduced_distance_ratio: f32,
    pub hysteresis_ratio: f32,
}

impl Default for NetworkLodPolicy {
    fn default() -> Self {
        Self {
            full_distance_ratio: 0.35,
            reduced_distance_ratio: 0.75,
            hysteresis_ratio: 0.05,
        }
    }
}

impl NetworkLodPolicy {
    pub fn lod_for_distance_squared(
        self,
        distance_squared: f32,
        interest_radius: f32,
    ) -> NetworkLod {
        self.lod_for_distance_squared_with_previous(distance_squared, interest_radius, None)
    }

    pub fn lod_for_distance_squared_with_previous(
        self,
        distance_squared: f32,
        interest_radius: f32,
        previous_lod: Option<NetworkLod>,
    ) -> NetworkLod {
        if !distance_squared.is_finite()
            || distance_squared < 0.0
            || !interest_radius.is_finite()
            || interest_radius <= 0.0
        {
            return NetworkLod::Full;
        }

        let full_ratio = normalized_ratio(self.full_distance_ratio, 0.35);
        let reduced_ratio = normalized_ratio(self.reduced_distance_ratio, 0.75).max(full_ratio);
        let radius_squared = interest_radius * interest_radius;
        let distance_squared = distance_squared.min(radius_squared);
        let Some(previous_lod) = previous_lod else {
            return classify_network_lod_squared(
                distance_squared,
                interest_radius,
                full_ratio,
                reduced_ratio,
            );
        };
        let hysteresis = normalized_ratio(self.hysteresis_ratio, 0.05);

        match previous_lod {
            NetworkLod::Full => {
                if distance_squared <= threshold_squared(interest_radius, full_ratio + hysteresis) {
                    NetworkLod::Full
                } else if distance_squared
                    <= threshold_squared(interest_radius, reduced_ratio + hysteresis)
                {
                    NetworkLod::Reduced
                } else {
                    NetworkLod::Minimal
                }
            }
            NetworkLod::Reduced => {
                if distance_squared <= threshold_squared(interest_radius, full_ratio - hysteresis) {
                    NetworkLod::Full
                } else if distance_squared
                    > threshold_squared(interest_radius, reduced_ratio + hysteresis)
                {
                    NetworkLod::Minimal
                } else {
                    NetworkLod::Reduced
                }
            }
            NetworkLod::Minimal => {
                if distance_squared
                    > threshold_squared(
                        interest_radius,
                        (reduced_ratio - hysteresis).max(full_ratio),
                    )
                {
                    NetworkLod::Minimal
                } else if distance_squared
                    <= threshold_squared(interest_radius, full_ratio - hysteresis)
                {
                    NetworkLod::Full
                } else {
                    NetworkLod::Reduced
                }
            }
        }
    }
}

fn classify_network_lod_squared(
    distance_squared: f32,
    interest_radius: f32,
    full_ratio: f32,
    reduced_ratio: f32,
) -> NetworkLod {
    if distance_squared <= threshold_squared(interest_radius, full_ratio) {
        NetworkLod::Full
    } else if distance_squared <= threshold_squared(interest_radius, reduced_ratio) {
        NetworkLod::Reduced
    } else {
        NetworkLod::Minimal
    }
}

fn threshold_squared(interest_radius: f32, ratio: f32) -> f32 {
    let distance = interest_radius * ratio.clamp(0.0, 1.0);
    distance * distance
}

fn normalized_ratio(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

fn cap_network_lod(default_lod: NetworkLod, selected_lod: NetworkLod) -> NetworkLod {
    match (default_lod, selected_lod) {
        (NetworkLod::Minimal, _) | (_, NetworkLod::Minimal) => NetworkLod::Minimal,
        (NetworkLod::Reduced, _) | (_, NetworkLod::Reduced) => NetworkLod::Reduced,
        (NetworkLod::Full, NetworkLod::Full) => NetworkLod::Full,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetworkLodContext {
    pub client: ClientId,
    pub entity: EntityId,
    pub client_position: Vec3,
    pub entity_position: Vec3,
    pub interest_radius: f32,
    pub distance_squared: f32,
    pub entity_lod_cap: NetworkLod,
    pub previous_lod: Option<NetworkLod>,
    pub policy_lod: NetworkLod,
}

#[derive(Clone, Copy, Debug)]
pub struct ServerConfig {
    pub tick_rate_hz: u16,
    pub cell_size: f32,
    pub default_interest_radius: f32,
    pub per_client_byte_budget: usize,
    pub snapshot_history: usize,
    pub lag_history_ticks: usize,
    pub network_lod: NetworkLodPolicy,
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
            network_lod: NetworkLodPolicy::default(),
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
    pub reduced_payload: Vec<u8>,
    pub minimal_payload: Vec<u8>,
    pub estimated_bytes: usize,
    pub base_priority: f32,
    pub lod: NetworkLod,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkLodPayloads {
    pub full: Vec<u8>,
    pub reduced: Vec<u8>,
    pub minimal: Vec<u8>,
}

impl NetworkLodPayloads {
    pub fn new(
        full_payload: impl Into<Vec<u8>>,
        reduced_payload: impl Into<Vec<u8>>,
        minimal_payload: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            full: full_payload.into(),
            reduced: reduced_payload.into(),
            minimal: minimal_payload.into(),
        }
    }

    fn lod_bytes(&self) -> NetworkLodBytes {
        NetworkLodBytes::new(self.full.len(), self.reduced.len(), self.minimal.len())
    }
}

impl ServerEntity {
    fn payload_for_lod(&self, lod: NetworkLod) -> Vec<u8> {
        match lod {
            NetworkLod::Full => self.payload.clone(),
            NetworkLod::Reduced => self.reduced_payload.clone(),
            NetworkLod::Minimal => self.minimal_payload.clone(),
        }
    }

    fn lod_bytes(&self) -> NetworkLodBytes {
        NetworkLodBytes::new(
            self.payload.len(),
            self.reduced_payload.len(),
            self.minimal_payload.len(),
        )
    }
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

impl CellRoute {
    pub const fn source_owner(self) -> Option<RegionId> {
        match self {
            Self::Local { owner } => Some(owner),
            Self::CrossRegion { source_owner, .. } => Some(source_owner),
            Self::Unowned { source_owner, .. } => source_owner,
        }
    }

    pub const fn target_owner(self) -> Option<RegionId> {
        match self {
            Self::Local { owner } => Some(owner),
            Self::CrossRegion { target_owner, .. } => Some(target_owner),
            Self::Unowned { target_owner, .. } => target_owner,
        }
    }

    pub const fn is_cross_region(self) -> bool {
        matches!(self, Self::CrossRegion { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedCellEvent {
    pub event: CellEvent,
    pub route: CellRoute,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionEventBatch {
    pub target: RegionId,
    pub events: Vec<RoutedCellEvent>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionDispatchMetrics {
    pub batches: usize,
    pub events: usize,
}

pub trait RegionEventSink {
    fn send_region_events(&mut self, batch: RegionEventBatch);
}

#[derive(Debug, Default)]
pub struct MemoryRegionEventSink {
    pub sent: Vec<RegionEventBatch>,
}

impl MemoryRegionEventSink {
    pub fn clear(&mut self) {
        self.sent.clear();
    }
}

impl RegionEventSink for MemoryRegionEventSink {
    fn send_region_events(&mut self, batch: RegionEventBatch) {
        self.sent.push(batch);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagSample {
    pub tick: Tick,
    pub position: Vec3,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagRay {
    pub origin: Vec3,
    pub direction: Vec3,
    pub max_distance: f32,
}

impl LagRay {
    pub const fn new(origin: Vec3, direction: Vec3, max_distance: f32) -> Self {
        Self {
            origin,
            direction,
            max_distance,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LagSphereHit {
    pub entity: EntityId,
    pub tick: Tick,
    pub sub_tick: f32,
    pub center: Vec3,
    pub radius: f32,
    pub distance_along_ray: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickMetrics {
    pub tick: Tick,
    pub clients: usize,
    pub entities: usize,
    pub aoi_candidates: usize,
    pub selected_updates: usize,
    pub selected_full_lod_updates: usize,
    pub selected_reduced_lod_updates: usize,
    pub selected_minimal_lod_updates: usize,
    pub deferred_updates: usize,
    pub exit_updates: usize,
    pub bytes_scheduled: usize,
    pub deferred_bytes: usize,
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
            selected_full_lod_updates: metrics.selected_full_lod_updates,
            selected_reduced_lod_updates: metrics.selected_reduced_lod_updates,
            selected_minimal_lod_updates: metrics.selected_minimal_lod_updates,
            deferred_updates: metrics.deferred_updates,
            bytes_scheduled: metrics.bytes_scheduled,
            deferred_bytes: metrics.deferred_bytes,
        }
    }
}

pub trait Transport {
    fn send(&mut self, client: ClientId, message: ServerMessage);

    fn drain_received(&mut self) -> Vec<RoutedClientMessage> {
        Vec::new()
    }
}

pub trait ByteTransport {
    fn send_bytes(&mut self, client: ClientId, bytes: Vec<u8>);

    fn drain_received_bytes(&mut self) -> Vec<RoutedClientBytes> {
        Vec::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedClientBytes {
    pub client: ClientId,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportCodecError {
    EncodeServer { client: ClientId, error: CodecError },
    DecodeClient { client: ClientId, error: CodecError },
}

#[derive(Debug)]
pub struct CodecTransport<T> {
    inner: T,
    decode_config: DecodeConfig,
    errors: Vec<TransportCodecError>,
}

impl<T> CodecTransport<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            decode_config: DecodeConfig::default(),
            errors: Vec::new(),
        }
    }

    pub fn with_decode_config(inner: T, decode_config: DecodeConfig) -> Self {
        Self {
            inner,
            decode_config,
            errors: Vec::new(),
        }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }

    pub fn errors(&self) -> &[TransportCodecError] {
        &self.errors
    }

    pub fn take_errors(&mut self) -> Vec<TransportCodecError> {
        std::mem::take(&mut self.errors)
    }
}

impl<T: ByteTransport> Transport for CodecTransport<T> {
    fn send(&mut self, client: ClientId, message: ServerMessage) {
        match encode_server_message(&message) {
            Ok(bytes) => self.inner.send_bytes(client, bytes),
            Err(error) => self
                .errors
                .push(TransportCodecError::EncodeServer { client, error }),
        }
    }

    fn drain_received(&mut self) -> Vec<RoutedClientMessage> {
        let mut messages = Vec::new();
        for RoutedClientBytes { client, bytes } in self.inner.drain_received_bytes() {
            match decode_client_message_with_config(&bytes, self.decode_config) {
                Ok(message) => messages.push(RoutedClientMessage { client, message }),
                Err(error) => self
                    .errors
                    .push(TransportCodecError::DecodeClient { client, error }),
            }
        }
        messages
    }
}

#[derive(Debug, Default)]
pub struct MemoryByteTransport {
    pub sent: Vec<(ClientId, Vec<u8>)>,
    pub received: Vec<RoutedClientBytes>,
}

impl MemoryByteTransport {
    pub fn clear(&mut self) {
        self.sent.clear();
    }

    pub fn push_received_bytes(&mut self, client: ClientId, bytes: Vec<u8>) {
        self.received.push(RoutedClientBytes { client, bytes });
    }

    pub fn push_received_message(
        &mut self,
        client: ClientId,
        message: &ClientMessage,
    ) -> Result<(), CodecError> {
        self.push_received_bytes(client, encode_client_message(message)?);
        Ok(())
    }
}

impl ByteTransport for MemoryByteTransport {
    fn send_bytes(&mut self, client: ClientId, bytes: Vec<u8>) {
        self.sent.push((client, bytes));
    }

    fn drain_received_bytes(&mut self) -> Vec<RoutedClientBytes> {
        self.received.drain(..).collect()
    }
}

const UDP_RECV_BUFFER_BYTES: usize = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UdpTransportError {
    UnknownClient { client: ClientId, bytes: usize },
    UnknownPeer { peer: SocketAddr, bytes: usize },
    Send { client: ClientId, error: ErrorKind },
    Receive { error: ErrorKind },
}

#[derive(Debug)]
pub struct UdpByteTransport {
    socket: UdpSocket,
    clients: HashMap<ClientId, SocketAddr>,
    peers: HashMap<SocketAddr, ClientId>,
    errors: Vec<UdpTransportError>,
    recv_buffer: Vec<u8>,
}

impl UdpByteTransport {
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        Self::from_socket(UdpSocket::bind(addr)?)
    }

    pub fn from_socket(socket: UdpSocket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            clients: HashMap::new(),
            peers: HashMap::new(),
            errors: Vec::new(),
            recv_buffer: vec![0; UDP_RECV_BUFFER_BYTES],
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn register_client(&mut self, client: ClientId, addr: SocketAddr) -> Option<SocketAddr> {
        let previous_addr = self.clients.insert(client, addr);
        if let Some(previous_addr) = previous_addr {
            self.peers.remove(&previous_addr);
        }
        if let Some(previous_client) = self.peers.insert(addr, client) {
            if previous_client != client {
                self.clients.remove(&previous_client);
            }
        }
        previous_addr
    }

    pub fn remove_client(&mut self, client: ClientId) -> Option<SocketAddr> {
        let addr = self.clients.remove(&client)?;
        if self.peers.get(&addr) == Some(&client) {
            self.peers.remove(&addr);
        }
        Some(addr)
    }

    pub fn client_addr(&self, client: ClientId) -> Option<SocketAddr> {
        self.clients.get(&client).copied()
    }

    pub fn errors(&self) -> &[UdpTransportError] {
        &self.errors
    }

    pub fn take_errors(&mut self) -> Vec<UdpTransportError> {
        std::mem::take(&mut self.errors)
    }
}

impl ByteTransport for UdpByteTransport {
    fn send_bytes(&mut self, client: ClientId, bytes: Vec<u8>) {
        let Some(addr) = self.clients.get(&client).copied() else {
            self.errors.push(UdpTransportError::UnknownClient {
                client,
                bytes: bytes.len(),
            });
            return;
        };

        if let Err(error) = self.socket.send_to(&bytes, addr) {
            self.errors.push(UdpTransportError::Send {
                client,
                error: error.kind(),
            });
        }
    }

    fn drain_received_bytes(&mut self) -> Vec<RoutedClientBytes> {
        let mut received = Vec::new();
        loop {
            match self.socket.recv_from(&mut self.recv_buffer) {
                Ok((len, peer)) => {
                    if let Some(client) = self.peers.get(&peer).copied() {
                        received.push(RoutedClientBytes {
                            client,
                            bytes: self.recv_buffer[..len].to_vec(),
                        });
                    } else {
                        self.errors
                            .push(UdpTransportError::UnknownPeer { peer, bytes: len });
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.errors.push(UdpTransportError::Receive {
                        error: error.kind(),
                    });
                    break;
                }
            }
        }
        received
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
        if let Some(target_owner) = route.target_owner() {
            self.region_cell_events
                .entry(target_owner)
                .or_default()
                .push(routed);
        } else {
            self.unowned_cell_events.push(routed);
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

    pub fn pending_region_event_targets(&self) -> Vec<RegionId> {
        let mut targets: Vec<_> = self.region_cell_events.keys().copied().collect();
        targets.sort_unstable();
        targets
    }

    pub fn pending_unowned_event_count(&self) -> usize {
        self.unowned_cell_events.len()
    }

    pub fn drain_region_events(&mut self, owner: RegionId) -> Vec<RoutedCellEvent> {
        self.region_cell_events.remove(&owner).unwrap_or_default()
    }

    pub fn drain_region_event_batches(&mut self) -> Vec<RegionEventBatch> {
        let mut batches: Vec<_> = std::mem::take(&mut self.region_cell_events)
            .into_iter()
            .filter(|(_, events)| !events.is_empty())
            .map(|(target, events)| RegionEventBatch { target, events })
            .collect();
        batches.sort_by_key(|batch| batch.target);
        batches
    }

    pub fn dispatch_region_events<S: RegionEventSink>(
        &mut self,
        sink: &mut S,
    ) -> RegionDispatchMetrics {
        let batches = self.drain_region_event_batches();
        let metrics = RegionDispatchMetrics {
            batches: batches.len(),
            events: batches.iter().map(|batch| batch.events.len()).sum(),
        };
        for batch in batches {
            sink.send_region_events(batch);
        }
        metrics
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

    pub fn rewind_entity_position_interpolated(
        &self,
        entity: EntityId,
        tick: Tick,
        sub_tick: f32,
    ) -> Option<Vec3> {
        let sub_tick = normalized_sub_tick(sub_tick);
        let samples = self.lag_history.get(&entity)?;
        let start = samples.iter().find(|sample| sample.tick == tick)?;
        if sub_tick == 0.0 || tick.raw() == u64::MAX {
            return Some(start.position);
        }

        let next_tick = Tick::new(tick.raw().saturating_add(1));
        let end = samples.iter().find(|sample| sample.tick == next_tick)?;
        Some(start.position.lerp(end.position, sub_tick))
    }

    pub fn validate_rewound_sphere_hit(
        &self,
        entity: EntityId,
        tick: Tick,
        sub_tick: f32,
        ray: LagRay,
        radius: f32,
    ) -> Option<LagSphereHit> {
        let sub_tick = normalized_sub_tick(sub_tick);
        let center = self.rewind_entity_position_interpolated(entity, tick, sub_tick)?;
        let distance_along_ray =
            ray_sphere_hit_distance(ray.origin, ray.direction, ray.max_distance, center, radius)?;

        Some(LagSphereHit {
            entity,
            tick,
            sub_tick,
            center,
            radius,
            distance_along_ray,
        })
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
        let reduced_payload = payload.clone();
        let minimal_payload = payload.clone();
        let entity = ServerEntity {
            id,
            position,
            payload: payload.clone(),
            reduced_payload,
            minimal_payload,
            estimated_bytes,
            base_priority,
            lod: NetworkLod::Full,
        };
        self.entities.insert(id, entity);
        self.aoi.insert_entity(id, position);
        self.replication
            .upsert_entity(id, estimated_bytes, base_priority);
    }

    pub fn spawn_entity_with_lod_payloads(
        &mut self,
        id: EntityId,
        position: Vec3,
        payloads: NetworkLodPayloads,
        base_priority: f32,
        lod: NetworkLod,
    ) {
        let lod_bytes = payloads.lod_bytes();
        let entity = ServerEntity {
            id,
            position,
            payload: payloads.full,
            reduced_payload: payloads.reduced,
            minimal_payload: payloads.minimal,
            estimated_bytes: lod_bytes.full,
            base_priority,
            lod,
        };
        self.entities.insert(id, entity);
        self.aoi.insert_entity(id, position);
        self.replication
            .upsert_entity_with_lod_bytes(id, lod_bytes, base_priority, lod);
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
        let payload = payload.into();
        entity.payload = payload.clone();
        entity.reduced_payload = payload.clone();
        entity.minimal_payload = payload;
        entity.estimated_bytes = entity.payload.len();
        self.replication.upsert_entity_with_lod_bytes(
            id,
            entity.lod_bytes(),
            entity.base_priority,
            entity.lod,
        );
        true
    }

    pub fn set_entity_lod_payloads(
        &mut self,
        id: EntityId,
        full_payload: impl Into<Vec<u8>>,
        reduced_payload: impl Into<Vec<u8>>,
        minimal_payload: impl Into<Vec<u8>>,
    ) -> bool {
        let Some(entity) = self.entities.get_mut(&id) else {
            return false;
        };
        entity.payload = full_payload.into();
        entity.reduced_payload = reduced_payload.into();
        entity.minimal_payload = minimal_payload.into();
        entity.estimated_bytes = entity.payload.len();
        self.replication.upsert_entity_with_lod_bytes(
            id,
            entity.lod_bytes(),
            entity.base_priority,
            entity.lod,
        );
        true
    }

    pub fn set_entity_lod(&mut self, id: EntityId, lod: NetworkLod) -> bool {
        let Some(entity) = self.entities.get_mut(&id) else {
            return false;
        };
        entity.lod = lod;
        self.replication.set_entity_lod(id, lod)
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
        self.advance_tick_with_network_lod_selector(transport, |context| context.policy_lod)
    }

    pub fn advance_tick_with_network_lod_selector<T: Transport>(
        &mut self,
        transport: &mut T,
        mut selector: impl FnMut(NetworkLodContext) -> NetworkLod,
    ) -> TickMetrics {
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
        let mut visible_entities = Vec::new();

        for client in clients {
            if !self.aoi.query_observer_into(client, &mut visible_entities) {
                continue;
            }
            metrics.aoi_candidates += visible_entities.len();
            let visibility = self
                .replication
                .set_visibility(client, visible_entities.drain(..));
            let Some(client_state) = self.clients.get(&client) else {
                continue;
            };
            let client_position = client_state.position;
            let interest_radius = client_state.interest_radius;
            let network_lod_policy = self.config.network_lod;
            let entities = &self.entities;
            let selection = self.replication.select_for_client_with_lod_context(
                client,
                ByteBudget::new(self.config.per_client_byte_budget),
                |context| {
                    let Some(entity) = entities.get(&context.entity) else {
                        return context.default_lod;
                    };
                    let distance_squared = client_position.distance_squared(entity.position);
                    let policy_lod = network_lod_policy.lod_for_distance_squared_with_previous(
                        distance_squared,
                        interest_radius,
                        context.last_sent_lod,
                    );
                    let lod = selector(NetworkLodContext {
                        client,
                        entity: context.entity,
                        client_position,
                        entity_position: entity.position,
                        interest_radius,
                        distance_squared,
                        entity_lod_cap: context.default_lod,
                        previous_lod: context.last_sent_lod,
                        policy_lod,
                    });
                    cap_network_lod(context.default_lod, lod)
                },
            );

            metrics.selected_updates += selection.updates.len();
            for update in &selection.updates {
                match update.lod {
                    NetworkLod::Full => metrics.selected_full_lod_updates += 1,
                    NetworkLod::Reduced => metrics.selected_reduced_lod_updates += 1,
                    NetworkLod::Minimal => metrics.selected_minimal_lod_updates += 1,
                }
            }
            metrics.deferred_updates += selection.deferred_updates;
            metrics.deferred_bytes = metrics
                .deferred_bytes
                .saturating_add(selection.deferred_bytes);
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
                frame.insert(update.entity, entity.payload_for_lod(update.lod));
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

fn normalized_sub_tick(sub_tick: f32) -> f32 {
    if sub_tick.is_finite() {
        sub_tick.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn ray_sphere_hit_distance(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    center: Vec3,
    radius: f32,
) -> Option<f32> {
    if !origin.is_finite()
        || !direction.is_finite()
        || !center.is_finite()
        || !max_distance.is_finite()
        || !radius.is_finite()
        || max_distance < 0.0
        || radius < 0.0
    {
        return None;
    }

    let direction_len_sq = direction.distance_squared(Vec3::ZERO);
    if direction_len_sq <= f32::EPSILON {
        return None;
    }

    let direction_len = direction_len_sq.sqrt();
    let unit_direction = Vec3::new(
        direction.x / direction_len,
        direction.y / direction_len,
        direction.z / direction_len,
    );
    let to_center = Vec3::new(
        center.x - origin.x,
        center.y - origin.y,
        center.z - origin.z,
    );
    let projection = dot(to_center, unit_direction);
    let center_distance_sq = to_center.distance_squared(Vec3::ZERO);
    let perpendicular_distance_sq = (center_distance_sq - projection * projection).max(0.0);
    let radius_sq = radius * radius;

    if perpendicular_distance_sq > radius_sq {
        return None;
    }

    let half_chord = (radius_sq - perpendicular_distance_sq).sqrt();
    let near_distance = projection - half_chord;
    let far_distance = projection + half_chord;
    if far_distance < 0.0 || near_distance > max_distance {
        return None;
    }

    Some(near_distance.max(0.0))
}

fn dot(left: Vec3, right: Vec3) -> f32 {
    left.x
        .mul_add(right.x, left.y.mul_add(right.y, left.z * right.z))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::thread;
    use std::time::Instant;

    use gridwake_protocol::{decode_server_message, encode_client_message};
    use gridwake_snapshot::DeltaOp;

    fn payload(id: u64) -> Vec<u8> {
        id.to_le_bytes().to_vec()
    }

    fn poll_until_nonempty<T>(mut poll: impl FnMut() -> Vec<T>) -> Vec<T> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let values = poll();
            if !values.is_empty() || Instant::now() >= deadline {
                return values;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn snapshot_payload_for(
        transport: &FakeTransport,
        client: ClientId,
        entity: EntityId,
    ) -> Vec<u8> {
        let (_, message) = transport
            .sent
            .iter()
            .find(|(target, _)| *target == client)
            .expect("expected snapshot for client");
        let ServerMessage::SnapshotDelta(delta) = message else {
            panic!("expected snapshot delta");
        };
        delta
            .ops
            .iter()
            .find_map(|op| match op {
                DeltaOp::SpawnOrEnter {
                    entity: candidate,
                    payload,
                }
                | DeltaOp::Update {
                    entity: candidate,
                    payload,
                } if *candidate == entity => Some(payload.clone()),
                DeltaOp::SpawnOrEnter { .. }
                | DeltaOp::Update { .. }
                | DeltaOp::DespawnOrExit { .. } => None,
            })
            .expect("expected entity payload")
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
        assert_eq!(metrics.deferred_updates, 1);
        assert_eq!(metrics.deferred_bytes, 16);
        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(delta.ops[0].entity(), high);
    }

    #[test]
    fn tick_uses_selected_network_lod_payload() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 8,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity_with_lod_payloads(
            entity,
            Vec3::ZERO,
            NetworkLodPayloads::new(
                b"full-payload-too-large".to_vec(),
                b"reduced".to_vec(),
                b"min".to_vec(),
            ),
            1.0,
            NetworkLod::Reduced,
        );

        let metrics = runtime.advance_tick(&mut transport);

        assert_eq!(metrics.selected_updates, 1);
        assert_eq!(metrics.selected_full_lod_updates, 0);
        assert_eq!(metrics.selected_reduced_lod_updates, 1);
        assert_eq!(metrics.selected_minimal_lod_updates, 0);
        assert_eq!(metrics.bytes_scheduled, b"reduced".len());
        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(
            delta.ops,
            vec![DeltaOp::SpawnOrEnter {
                entity,
                payload: b"reduced".to_vec()
            }]
        );
    }

    #[test]
    fn tick_falls_back_to_lower_lod_when_desired_payload_exceeds_budget() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 8,
            network_lod: NetworkLodPolicy {
                full_distance_ratio: 1.0,
                reduced_distance_ratio: 1.0,
                hysteresis_ratio: 0.0,
            },
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity_with_lod_payloads(
            entity,
            Vec3::ZERO,
            NetworkLodPayloads::new(
                b"full-payload-too-large".to_vec(),
                b"reduced".to_vec(),
                b"min".to_vec(),
            ),
            1.0,
            NetworkLod::Full,
        );

        let metrics = runtime.advance_tick(&mut transport);

        assert_eq!(metrics.selected_updates, 1);
        assert_eq!(metrics.selected_full_lod_updates, 0);
        assert_eq!(metrics.selected_reduced_lod_updates, 1);
        assert_eq!(metrics.selected_minimal_lod_updates, 0);
        assert_eq!(metrics.deferred_updates, 0);
        assert_eq!(metrics.bytes_scheduled, b"reduced".len());
        assert_eq!(
            snapshot_payload_for(&transport, client, entity),
            b"reduced".to_vec()
        );
    }

    #[test]
    fn changing_server_entity_lod_marks_it_dirty() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 64,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity_with_lod_payloads(
            entity,
            Vec3::ZERO,
            NetworkLodPayloads::new(b"full".to_vec(), b"reduced".to_vec(), b"min".to_vec()),
            1.0,
            NetworkLod::Minimal,
        );
        runtime.advance_tick(&mut transport);
        transport.clear();

        assert!(runtime.set_entity_lod(entity, NetworkLod::Full));
        runtime.advance_tick(&mut transport);

        let ServerMessage::SnapshotDelta(delta) = &transport.sent[0].1 else {
            panic!("expected snapshot delta");
        };
        assert_eq!(
            delta.ops,
            vec![DeltaOp::SpawnOrEnter {
                entity,
                payload: b"full".to_vec()
            }]
        );
    }

    #[test]
    fn tick_applies_distance_based_lod_per_client() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 64,
            network_lod: NetworkLodPolicy {
                full_distance_ratio: 0.25,
                reduced_distance_ratio: 0.50,
                ..NetworkLodPolicy::default()
            },
            ..ServerConfig::default()
        });
        let near_client = ClientId::new(1);
        let mid_client = ClientId::new(2);
        let far_client = ClientId::new(3);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(near_client, Vec3::ZERO, None);
        runtime.connect_client(mid_client, Vec3::new(40.0, 0.0, 0.0), None);
        runtime.connect_client(far_client, Vec3::new(90.0, 0.0, 0.0), None);
        runtime.spawn_entity_with_lod_payloads(
            entity,
            Vec3::ZERO,
            NetworkLodPayloads::new(b"full".to_vec(), b"reduced".to_vec(), b"min".to_vec()),
            1.0,
            NetworkLod::Full,
        );

        let metrics = runtime.advance_tick(&mut transport);

        assert_eq!(metrics.selected_updates, 3);
        assert_eq!(metrics.selected_full_lod_updates, 1);
        assert_eq!(metrics.selected_reduced_lod_updates, 1);
        assert_eq!(metrics.selected_minimal_lod_updates, 1);
        assert_eq!(transport.sent.len(), 3);
        assert_eq!(
            snapshot_payload_for(&transport, near_client, entity),
            b"full".to_vec()
        );
        assert_eq!(
            snapshot_payload_for(&transport, mid_client, entity),
            b"reduced".to_vec()
        );
        assert_eq!(
            snapshot_payload_for(&transport, far_client, entity),
            b"min".to_vec()
        );
    }

    #[test]
    fn custom_network_lod_selector_overrides_distance_policy() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 64,
            network_lod: NetworkLodPolicy {
                full_distance_ratio: 1.0,
                reduced_distance_ratio: 1.0,
                hysteresis_ratio: 0.0,
            },
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity_with_lod_payloads(
            entity,
            Vec3::ZERO,
            NetworkLodPayloads::new(b"full".to_vec(), b"reduced".to_vec(), b"min".to_vec()),
            1.0,
            NetworkLod::Full,
        );

        let metrics = runtime.advance_tick_with_network_lod_selector(&mut transport, |context| {
            assert_eq!(context.client, client);
            assert_eq!(context.entity, entity);
            assert_eq!(context.client_position, Vec3::ZERO);
            assert_eq!(context.entity_position, Vec3::ZERO);
            assert_eq!(context.interest_radius, 100.0);
            assert_eq!(context.distance_squared, 0.0);
            assert_eq!(context.entity_lod_cap, NetworkLod::Full);
            assert_eq!(context.previous_lod, None);
            assert_eq!(context.policy_lod, NetworkLod::Full);
            NetworkLod::Minimal
        });

        assert_eq!(metrics.selected_updates, 1);
        assert_eq!(metrics.selected_full_lod_updates, 0);
        assert_eq!(metrics.selected_reduced_lod_updates, 0);
        assert_eq!(metrics.selected_minimal_lod_updates, 1);
        assert_eq!(
            snapshot_payload_for(&transport, client, entity),
            b"min".to_vec()
        );
    }

    #[test]
    fn custom_network_lod_selector_still_respects_entity_lod_cap() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 64,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity_with_lod_payloads(
            entity,
            Vec3::ZERO,
            NetworkLodPayloads::new(b"full".to_vec(), b"reduced".to_vec(), b"min".to_vec()),
            1.0,
            NetworkLod::Reduced,
        );

        let metrics =
            runtime.advance_tick_with_network_lod_selector(&mut transport, |_| NetworkLod::Full);

        assert_eq!(metrics.selected_full_lod_updates, 0);
        assert_eq!(metrics.selected_reduced_lod_updates, 1);
        assert_eq!(metrics.selected_minimal_lod_updates, 0);
        assert_eq!(
            snapshot_payload_for(&transport, client, entity),
            b"reduced".to_vec()
        );
    }

    #[test]
    fn client_lod_band_change_reselects_clean_entity() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 64,
            network_lod: NetworkLodPolicy {
                full_distance_ratio: 0.25,
                reduced_distance_ratio: 0.50,
                ..NetworkLodPolicy::default()
            },
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::new(90.0, 0.0, 0.0), None);
        runtime.spawn_entity_with_lod_payloads(
            entity,
            Vec3::ZERO,
            NetworkLodPayloads::new(b"full".to_vec(), b"reduced".to_vec(), b"min".to_vec()),
            1.0,
            NetworkLod::Full,
        );
        runtime.advance_tick(&mut transport);
        assert_eq!(
            snapshot_payload_for(&transport, client, entity),
            b"min".to_vec()
        );
        transport.clear();

        assert!(runtime.update_client_position(client, Vec3::ZERO));
        let metrics = runtime.advance_tick(&mut transport);

        assert_eq!(metrics.selected_updates, 1);
        assert_eq!(
            snapshot_payload_for(&transport, client, entity),
            b"full".to_vec()
        );
    }

    #[test]
    fn network_lod_hysteresis_prevents_boundary_flapping() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 100.0,
            per_client_byte_budget: 64,
            network_lod: NetworkLodPolicy {
                full_distance_ratio: 0.25,
                reduced_distance_ratio: 0.50,
                hysteresis_ratio: 0.10,
            },
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.connect_client(client, Vec3::new(20.0, 0.0, 0.0), None);
        runtime.spawn_entity_with_lod_payloads(
            entity,
            Vec3::ZERO,
            NetworkLodPayloads::new(b"full".to_vec(), b"reduced".to_vec(), b"min".to_vec()),
            1.0,
            NetworkLod::Full,
        );
        runtime.advance_tick(&mut transport);
        assert_eq!(
            snapshot_payload_for(&transport, client, entity),
            b"full".to_vec()
        );
        transport.clear();

        assert!(runtime.update_client_position(client, Vec3::new(30.0, 0.0, 0.0)));
        let stable_metrics = runtime.advance_tick(&mut transport);
        assert_eq!(stable_metrics.selected_updates, 0);
        assert!(transport.sent.is_empty());

        assert!(runtime.update_client_position(client, Vec3::new(40.0, 0.0, 0.0)));
        let changed_metrics = runtime.advance_tick(&mut transport);
        assert_eq!(changed_metrics.selected_updates, 1);
        assert_eq!(
            snapshot_payload_for(&transport, client, entity),
            b"reduced".to_vec()
        );
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
    fn codec_transport_decodes_inbound_bytes_and_encodes_outbound_messages() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            tick_rate_hz: 10,
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let client = ClientId::new(1);
        let entity = EntityId::new(1);
        let byte_transport = MemoryByteTransport::default();
        let mut transport = CodecTransport::new(byte_transport);
        let mut metrics = VecMetrics::default();
        let mut scheduler = TickScheduler::from_hz(10, 4);

        runtime.connect_client(client, Vec3::ZERO, None);
        runtime.spawn_entity(entity, Vec3::ZERO, payload(entity.raw()), 16, 1.0);
        runtime.advance_tick(&mut transport);

        assert_eq!(transport.inner().sent.len(), 1);
        let outbound = decode_server_message(&transport.inner().sent[0].1).unwrap();
        assert!(matches!(outbound, ServerMessage::SnapshotDelta(_)));
        transport.inner_mut().clear();

        transport
            .inner_mut()
            .push_received_message(
                client,
                &ClientMessage::AckSnapshot {
                    sequence: SnapshotId::new(1),
                },
            )
            .unwrap();
        runtime.set_entity_payload(entity, b"changed".to_vec());

        runtime.advance_elapsed(
            &mut scheduler,
            Duration::from_millis(100),
            &mut transport,
            &mut metrics,
        );

        assert!(transport.errors().is_empty());
        assert_eq!(transport.inner().sent.len(), 1);
        let outbound = decode_server_message(&transport.inner().sent[0].1).unwrap();
        let ServerMessage::SnapshotDelta(delta) = outbound else {
            panic!("expected snapshot delta");
        };
        assert_eq!(delta.baseline, Some(SnapshotId::new(1)));
    }

    #[test]
    fn udp_byte_transport_routes_registered_client_datagrams(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let server_socket = UdpSocket::bind("127.0.0.1:0")?;
        let server_addr = server_socket.local_addr()?;
        let client_socket = UdpSocket::bind("127.0.0.1:0")?;
        client_socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        let client_addr = client_socket.local_addr()?;
        let client = ClientId::new(7);
        let mut transport = UdpByteTransport::from_socket(server_socket)?;

        assert_eq!(transport.register_client(client, client_addr), None);
        assert_eq!(transport.client_addr(client), Some(client_addr));

        client_socket.send_to(b"inbound", server_addr)?;
        let received = poll_until_nonempty(|| transport.drain_received_bytes());
        assert_eq!(
            received,
            vec![RoutedClientBytes {
                client,
                bytes: b"inbound".to_vec()
            }]
        );

        transport.send_bytes(client, b"outbound".to_vec());
        let mut buffer = [0; 64];
        let (len, peer) = client_socket.recv_from(&mut buffer)?;

        assert_eq!(peer, server_addr);
        assert_eq!(&buffer[..len], b"outbound");
        assert!(transport.errors().is_empty());
        Ok(())
    }

    #[test]
    fn udp_byte_transport_records_unknown_routes() -> Result<(), Box<dyn std::error::Error>> {
        let server_socket = UdpSocket::bind("127.0.0.1:0")?;
        let server_addr = server_socket.local_addr()?;
        let unknown_socket = UdpSocket::bind("127.0.0.1:0")?;
        let unknown_addr = unknown_socket.local_addr()?;
        let mut transport = UdpByteTransport::from_socket(server_socket)?;
        let client = ClientId::new(99);

        transport.send_bytes(client, b"missing-client".to_vec());
        unknown_socket.send_to(b"unknown-peer", server_addr)?;
        assert!(poll_until_nonempty(|| transport.drain_received_bytes()).is_empty());

        assert_eq!(
            transport.take_errors(),
            vec![
                UdpTransportError::UnknownClient {
                    client,
                    bytes: b"missing-client".len()
                },
                UdpTransportError::UnknownPeer {
                    peer: unknown_addr,
                    bytes: b"unknown-peer".len()
                }
            ]
        );
        Ok(())
    }

    #[test]
    fn codec_transport_round_trips_typed_messages_over_udp(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let server_socket = UdpSocket::bind("127.0.0.1:0")?;
        let server_addr = server_socket.local_addr()?;
        let client_socket = UdpSocket::bind("127.0.0.1:0")?;
        client_socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        let client_addr = client_socket.local_addr()?;
        let client = ClientId::new(3);
        let mut transport = CodecTransport::new(UdpByteTransport::from_socket(server_socket)?);

        transport.inner_mut().register_client(client, client_addr);

        let inbound = ClientMessage::AckSnapshot {
            sequence: SnapshotId::new(42),
        };
        client_socket.send_to(&encode_client_message(&inbound)?, server_addr)?;
        let received = poll_until_nonempty(|| transport.drain_received());
        assert_eq!(
            received,
            vec![RoutedClientMessage {
                client,
                message: inbound
            }]
        );

        let outbound = ServerMessage::Metrics(MetricsFrame {
            tick: Tick::new(7),
            clients: 1,
            entities: 2,
            aoi_candidates: 3,
            selected_updates: 4,
            selected_full_lod_updates: 1,
            selected_reduced_lod_updates: 2,
            selected_minimal_lod_updates: 1,
            deferred_updates: 5,
            bytes_scheduled: 6,
            deferred_bytes: 7,
        });
        transport.send(client, outbound.clone());

        let mut buffer = [0; 512];
        let (len, peer) = client_socket.recv_from(&mut buffer)?;
        assert_eq!(peer, server_addr);
        assert_eq!(decode_server_message(&buffer[..len])?, outbound);
        assert!(transport.errors().is_empty());
        assert!(transport.inner().errors().is_empty());
        Ok(())
    }

    #[test]
    fn codec_transport_records_decode_errors() {
        let client = ClientId::new(1);
        let mut transport = CodecTransport::new(MemoryByteTransport::default());
        transport
            .inner_mut()
            .push_received_bytes(client, b"not-gridwake".to_vec());

        assert!(transport.drain_received().is_empty());
        assert_eq!(
            transport.take_errors(),
            vec![TransportCodecError::DecodeClient {
                client,
                error: CodecError::InvalidMagic
            }]
        );
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
    fn lag_history_interpolates_entity_positions_between_ticks() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.spawn_entity(
            entity,
            Vec3::new(0.0, 0.0, 0.0),
            payload(entity.raw()),
            16,
            1.0,
        );
        runtime.advance_tick(&mut transport);
        runtime.move_entity(entity, Vec3::new(10.0, 5.0, -5.0));
        runtime.advance_tick(&mut transport);

        assert_eq!(
            runtime.rewind_entity_position_interpolated(entity, Tick::new(1), 0.5),
            Some(Vec3::new(5.0, 2.5, -2.5))
        );
        assert_eq!(
            runtime.rewind_entity_position_interpolated(entity, Tick::new(1), -1.0),
            Some(Vec3::new(0.0, 0.0, 0.0))
        );
        assert_eq!(
            runtime.rewind_entity_position_interpolated(entity, Tick::new(1), 2.0),
            Some(Vec3::new(10.0, 5.0, -5.0))
        );
        assert_eq!(
            runtime.rewind_entity_position_interpolated(entity, Tick::new(1), f32::NAN),
            Some(Vec3::new(0.0, 0.0, 0.0))
        );
        assert_eq!(
            runtime.rewind_entity_position_interpolated(entity, Tick::new(2), 0.5),
            None
        );
    }

    #[test]
    fn lag_history_validates_rewound_sphere_hits() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.spawn_entity(
            entity,
            Vec3::new(0.0, 0.0, 0.0),
            payload(entity.raw()),
            16,
            1.0,
        );
        runtime.advance_tick(&mut transport);
        runtime.move_entity(entity, Vec3::new(10.0, 0.0, 0.0));
        runtime.advance_tick(&mut transport);

        assert_eq!(
            runtime.validate_rewound_sphere_hit(
                entity,
                Tick::new(1),
                0.5,
                LagRay::new(Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), 10.0),
                0.5,
            ),
            Some(LagSphereHit {
                entity,
                tick: Tick::new(1),
                sub_tick: 0.5,
                center: Vec3::new(5.0, 0.0, 0.0),
                radius: 0.5,
                distance_along_ray: 4.5,
            })
        );

        assert_eq!(
            runtime.validate_rewound_sphere_hit(
                entity,
                Tick::new(1),
                2.0,
                LagRay::new(Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), 10.0),
                0.5,
            ),
            Some(LagSphereHit {
                entity,
                tick: Tick::new(1),
                sub_tick: 1.0,
                center: Vec3::new(10.0, 0.0, 0.0),
                radius: 0.5,
                distance_along_ray: 9.5,
            })
        );
    }

    #[test]
    fn lag_history_rejects_rewound_sphere_misses_and_invalid_queries() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            default_interest_radius: 10.0,
            per_client_byte_budget: 100,
            ..ServerConfig::default()
        });
        let entity = EntityId::new(1);
        let mut transport = FakeTransport::default();

        runtime.spawn_entity(
            entity,
            Vec3::new(5.0, 2.0, 0.0),
            payload(entity.raw()),
            16,
            1.0,
        );
        runtime.advance_tick(&mut transport);

        assert_eq!(
            runtime.validate_rewound_sphere_hit(
                entity,
                Tick::new(1),
                0.0,
                LagRay::new(Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), 10.0),
                0.5,
            ),
            None
        );
        assert_eq!(
            runtime.validate_rewound_sphere_hit(
                entity,
                Tick::new(1),
                0.0,
                LagRay::new(Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), 2.0),
                3.0,
            ),
            None
        );
        assert_eq!(
            runtime.validate_rewound_sphere_hit(
                entity,
                Tick::new(1),
                0.0,
                LagRay::new(Vec3::ZERO, Vec3::ZERO, 10.0),
                1.0,
            ),
            None
        );
        assert_eq!(
            runtime.validate_rewound_sphere_hit(
                entity,
                Tick::new(1),
                0.0,
                LagRay::new(Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), 10.0),
                -1.0,
            ),
            None
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

        let local = runtime
            .route_cell_event(left, right, b"same".to_vec())
            .route;
        assert_eq!(local, CellRoute::Local { owner: region_a });
        assert_eq!(local.source_owner(), Some(region_a));
        assert_eq!(local.target_owner(), Some(region_a));
        assert!(!local.is_cross_region());

        let cross = runtime.route_cell_event(left, far, b"cross".to_vec()).route;
        assert_eq!(
            cross,
            CellRoute::CrossRegion {
                source_owner: region_a,
                target_owner: region_b
            }
        );
        assert_eq!(cross.source_owner(), Some(region_a));
        assert_eq!(cross.target_owner(), Some(region_b));
        assert!(cross.is_cross_region());

        runtime.clear_cell_owner(far);
        let unowned = runtime
            .route_cell_event(left, far, b"missing".to_vec())
            .route;
        assert_eq!(
            unowned,
            CellRoute::Unowned {
                source_owner: Some(region_a),
                target_owner: None
            }
        );
        assert_eq!(unowned.source_owner(), Some(region_a));
        assert_eq!(unowned.target_owner(), None);
        assert!(!unowned.is_cross_region());
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
        assert_eq!(
            runtime.pending_region_event_targets(),
            vec![region_a, region_b]
        );
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

    #[test]
    fn region_event_batches_dispatch_to_sink_and_drain_outboxes() {
        let mut runtime = ServerRuntime::new(ServerConfig {
            cell_size: 10.0,
            ..ServerConfig::default()
        });
        let left = runtime.cell_for_position(Vec3::new(1.0, 0.0, 0.0));
        let right = runtime.cell_for_position(Vec3::new(15.0, 0.0, 0.0));
        let far = runtime.cell_for_position(Vec3::new(35.0, 0.0, 0.0));
        let region_a = RegionId::new(1);
        let region_b = RegionId::new(2);
        let mut sink = MemoryRegionEventSink::default();

        runtime.set_cell_owner(left, region_a);
        runtime.set_cell_owner(right, region_a);
        runtime.set_cell_owner(far, region_b);
        runtime.enqueue_cell_event(left, far, b"to-b".to_vec());
        runtime.enqueue_cell_event(left, right, b"to-a".to_vec());
        runtime.clear_cell_owner(far);
        runtime.enqueue_cell_event(left, far, b"unowned".to_vec());

        let metrics = runtime.dispatch_region_events(&mut sink);

        assert_eq!(
            metrics,
            RegionDispatchMetrics {
                batches: 2,
                events: 2
            }
        );
        assert_eq!(sink.sent.len(), 2);
        assert_eq!(sink.sent[0].target, region_a);
        assert_eq!(sink.sent[0].events[0].event.payload, b"to-a".to_vec());
        assert_eq!(sink.sent[1].target, region_b);
        assert_eq!(sink.sent[1].events[0].event.payload, b"to-b".to_vec());
        assert_eq!(
            runtime.pending_region_event_targets(),
            Vec::<RegionId>::new()
        );
        assert_eq!(runtime.pending_unowned_event_count(), 1);

        sink.clear();
        assert_eq!(
            runtime.dispatch_region_events(&mut sink),
            RegionDispatchMetrics::default()
        );
        assert!(sink.sent.is_empty());
    }
}
