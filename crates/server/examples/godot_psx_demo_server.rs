use std::collections::HashMap;
use std::env;
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use gridwake_core::{ClientId, EntityId, Vec3};
use gridwake_protocol::{
    decode_client_message, encode_server_message, encoded_server_message_len, ClientMessage,
    RoutedClientMessage, ServerMessage, SnapshotFragment,
};
use gridwake_server::{NetworkLod, NetworkLodPayloads, ServerConfig, ServerRuntime, Transport};
use gridwake_snapshot::{DeltaOp, DeltaSnapshot};

const RECV_BUFFER_BYTES: usize = 65_536;
const CLIENT_INPUT_MAGIC: &[u8; 4] = b"GWCI";
const DEMO_PAYLOAD_MAGIC: &[u8; 4] = b"GWPD";

const KIND_BOT: u8 = 0;
const KIND_PLAYER: u8 = 1;
const KIND_EFFECT: u8 = 2;
const KIND_COVER: u8 = 3;

const LOD_FULL: u8 = 0;
const LOD_REDUCED: u8 = 1;
const LOD_MINIMAL: u8 = 2;

const BOT_ENTITY_BASE: u64 = 1;
const EFFECT_ENTITY_BASE: u64 = 2_000_000;
const COVER_ENTITY_BASE: u64 = 4_000_000;
const PLAYER_ENTITY_BASE: u64 = 10_000_000;

const COVER_MAX_HEALTH: f32 = 100.0;
const COVER_DAMAGE_RADIUS: f32 = 10.0;
const COVER_DAMAGE_PER_HIT: f32 = 18.0;
const COVER_DAMAGE_INTERVAL_TICKS: u64 = 4;
const PLAYER_BLAST_RADIUS: f32 = 15.0;
const PLAYER_BLAST_DAMAGE: f32 = 42.0;
const PLAYER_FIRE_COOLDOWN_TICKS: u64 = 8;
const COVER_INDEX_CELL_SIZE: f32 = 16.0;

#[derive(Clone, Debug)]
struct Args {
    bind: String,
    bots: u64,
    effects: u64,
    cover: u64,
    tick_rate_hz: u16,
    world_size: f32,
    interest_radius: f32,
    byte_budget: usize,
    max_datagram_bytes: usize,
    log_every_ticks: u64,
    run_ticks: Option<u64>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3456".to_owned(),
            bots: 2_000,
            effects: 350,
            cover: 900,
            tick_rate_hz: 20,
            world_size: 512.0,
            interest_radius: 128.0,
            byte_budget: 700,
            max_datagram_bytes: 1_200,
            log_every_ticks: 20,
            run_ticks: None,
        }
    }
}

#[derive(Clone, Debug)]
struct DemoEntity {
    entity: EntityId,
    kind: u8,
    team: u8,
    anchor: Vec3,
    orbit_radius: f32,
    speed: f32,
    phase: f32,
    radius: f32,
    style: u32,
}

impl DemoEntity {
    fn position_at(&self, seconds: f32) -> Vec3 {
        let angle = self.phase + seconds * self.speed;
        match self.kind {
            KIND_EFFECT => Vec3::new(
                self.anchor.x + angle.cos() * self.orbit_radius,
                1.5 + (angle * 1.7).sin().abs() * 4.0,
                self.anchor.z + angle.sin() * self.orbit_radius,
            ),
            _ => Vec3::new(
                self.anchor.x + angle.cos() * self.orbit_radius,
                0.5,
                self.anchor.z + angle.sin() * self.orbit_radius,
            ),
        }
    }

    fn yaw_at(&self, seconds: f32) -> f32 {
        self.phase + seconds * self.speed
    }

    fn payloads_at(&self, seconds: f32) -> NetworkLodPayloads {
        let position = self.position_at(seconds);
        self.payloads_at_position(seconds, position)
    }

    fn payloads_at_position(&self, seconds: f32, position: Vec3) -> NetworkLodPayloads {
        payloads_for_entity(
            self.kind,
            self.team,
            position,
            self.yaw_at(seconds),
            self.radius,
            seconds + self.phase,
            self.style,
        )
    }
}

#[derive(Debug)]
struct CoverIndex {
    cell_size: f32,
    cells: HashMap<(i32, i32), Vec<usize>>,
}

impl CoverIndex {
    fn new(covers: &[DemoCover], cell_size: f32) -> Self {
        let mut cells: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (index, cover) in covers.iter().enumerate() {
            cells
                .entry(Self::cell_for_position(cell_size, cover.position))
                .or_default()
                .push(index);
        }
        Self { cell_size, cells }
    }

    fn query_indices(&self, center: Vec3, radius: f32, out: &mut Vec<usize>) {
        out.clear();
        let min_x = self.cell_for_coord(center.x - radius);
        let max_x = self.cell_for_coord(center.x + radius);
        let min_z = self.cell_for_coord(center.z - radius);
        let max_z = self.cell_for_coord(center.z + radius);
        for x in min_x..=max_x {
            for z in min_z..=max_z {
                if let Some(indices) = self.cells.get(&(x, z)) {
                    out.extend(indices.iter().copied());
                }
            }
        }
    }

    fn cell_for_position(cell_size: f32, position: Vec3) -> (i32, i32) {
        (
            (position.x / cell_size).floor() as i32,
            (position.z / cell_size).floor() as i32,
        )
    }

    fn cell_for_coord(&self, value: f32) -> i32 {
        (value / self.cell_size).floor() as i32
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PerfTotals {
    ticks: u64,
    update_micros: u128,
    world_micros: u128,
    runtime_micros: u128,
    tick_micros: u128,
    max_tick_micros: u128,
}

impl PerfTotals {
    fn record(&mut self, sample: TickPerfSample) {
        self.ticks = self.ticks.saturating_add(1);
        self.update_micros = self.update_micros.saturating_add(sample.update_micros);
        self.world_micros = self.world_micros.saturating_add(sample.world_micros);
        self.runtime_micros = self.runtime_micros.saturating_add(sample.runtime_micros);
        self.tick_micros = self.tick_micros.saturating_add(sample.tick_micros);
        self.max_tick_micros = self.max_tick_micros.max(sample.tick_micros);
    }

    fn avg_ms(total_micros: u128, ticks: u64) -> f64 {
        if ticks == 0 {
            0.0
        } else {
            total_micros as f64 / ticks as f64 / 1_000.0
        }
    }

    fn avg_update_ms(self) -> f64 {
        Self::avg_ms(self.update_micros, self.ticks)
    }

    fn avg_world_ms(self) -> f64 {
        Self::avg_ms(self.world_micros, self.ticks)
    }

    fn avg_runtime_ms(self) -> f64 {
        Self::avg_ms(self.runtime_micros, self.ticks)
    }

    fn avg_tick_ms(self) -> f64 {
        Self::avg_ms(self.tick_micros, self.ticks)
    }

    fn max_tick_ms(self) -> f64 {
        self.max_tick_micros as f64 / 1_000.0
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct TickPerfSample {
    update_micros: u128,
    world_micros: u128,
    runtime_micros: u128,
    tick_micros: u128,
}

#[derive(Clone, Debug)]
struct DemoCover {
    entity: EntityId,
    position: Vec3,
    radius: f32,
    health: f32,
    material: u8,
}

impl DemoCover {
    fn health_ratio(&self) -> f32 {
        (self.health / COVER_MAX_HEALTH).clamp(0.0, 1.0)
    }

    fn is_destroyed(&self) -> bool {
        self.health <= 0.0
    }

    fn payloads(&self) -> NetworkLodPayloads {
        payloads_for_entity(
            KIND_COVER,
            self.material,
            self.position,
            self.health_ratio(),
            self.radius,
            0.0,
            cover_style(self.material, self.health_ratio()),
        )
    }
}

#[derive(Clone, Debug)]
struct ClientState {
    player_entity: EntityId,
    position: Vec3,
    yaw: f32,
    next_fire_tick: u64,
}

#[derive(Clone, Copy, Debug)]
struct ClientInput {
    position: Vec3,
    yaw: f32,
    fire: bool,
}

#[derive(Clone, Copy, Debug)]
struct DemoPayload {
    kind: u8,
    team: u8,
    lod: u8,
    position: Vec3,
    yaw: f32,
    radius: f32,
    phase: f32,
    style: u32,
}

#[derive(Debug)]
struct InboundClientMessage {
    client: ClientId,
    message: ClientMessage,
    newly_connected: bool,
}

#[derive(Debug)]
struct DemoUdpTransport {
    socket: UdpSocket,
    clients: HashMap<ClientId, SocketAddr>,
    peers: HashMap<SocketAddr, ClientId>,
    next_client: u64,
    recv_buffer: Vec<u8>,
    max_datagram_bytes: usize,
    oversized_datagrams: u64,
    datagrams_sent: u64,
    bytes_sent: u64,
    snapshot_fragments_sent: u64,
}

impl DemoUdpTransport {
    fn bind(addr: &str, max_datagram_bytes: usize) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            clients: HashMap::new(),
            peers: HashMap::new(),
            next_client: 1,
            recv_buffer: vec![0; RECV_BUFFER_BYTES],
            max_datagram_bytes,
            oversized_datagrams: 0,
            datagrams_sent: 0,
            bytes_sent: 0,
            snapshot_fragments_sent: 0,
        })
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn drain_client_messages(&mut self) -> Vec<InboundClientMessage> {
        let mut messages = Vec::new();
        loop {
            match self.socket.recv_from(&mut self.recv_buffer) {
                Ok((len, peer)) => {
                    let (client, newly_connected) = self.client_for_peer(peer);
                    match decode_client_message(&self.recv_buffer[..len]) {
                        Ok(message) => messages.push(InboundClientMessage {
                            client,
                            message,
                            newly_connected,
                        }),
                        Err(error) => eprintln!(
                            "decode error from {peer} for client {}: {error}",
                            client.raw()
                        ),
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => {
                    eprintln!("UDP receive error: {error}");
                    break;
                }
            }
        }
        messages
    }

    fn client_for_peer(&mut self, peer: SocketAddr) -> (ClientId, bool) {
        if let Some(client) = self.peers.get(&peer).copied() {
            return (client, false);
        }

        let client = ClientId::new(self.next_client);
        self.next_client = self.next_client.saturating_add(1);
        self.peers.insert(peer, client);
        self.clients.insert(client, peer);
        (client, true)
    }
}

impl Transport for DemoUdpTransport {
    fn send(&mut self, client: ClientId, message: ServerMessage) {
        let Some(addr) = self.clients.get(&client).copied() else {
            return;
        };

        match message {
            ServerMessage::SnapshotDelta(delta) => self.send_snapshot_delta(client, addr, delta),
            message => self.send_encoded_message(client, addr, &message),
        }
    }

    fn drain_received(&mut self) -> Vec<RoutedClientMessage> {
        self.drain_client_messages()
            .into_iter()
            .map(|inbound| RoutedClientMessage {
                client: inbound.client,
                message: inbound.message,
            })
            .collect()
    }
}

impl DemoUdpTransport {
    fn send_snapshot_delta(&mut self, client: ClientId, addr: SocketAddr, delta: DeltaSnapshot) {
        let message = ServerMessage::SnapshotDelta(delta);
        let Ok(wire_len) = encoded_server_message_len(&message) else {
            self.send_encoded_message(client, addr, &message);
            return;
        };
        if wire_len <= self.max_datagram_bytes {
            self.send_encoded_message(client, addr, &message);
            return;
        }

        let ServerMessage::SnapshotDelta(delta) = message else {
            unreachable!("snapshot delta message was just constructed");
        };
        let chunks = self.snapshot_chunks_for_datagram(client, addr, delta);
        let fragment_count = chunks.len();
        if fragment_count == 0 {
            return;
        }
        if fragment_count > u16::MAX as usize {
            eprintln!(
                "drop snapshot for client {} at {addr}: fragments={} max={}",
                client.raw(),
                fragment_count,
                u16::MAX
            );
            return;
        }

        for (index, chunk) in chunks.into_iter().enumerate() {
            let fragment = SnapshotFragment {
                sequence: chunk.sequence,
                baseline: chunk.baseline,
                fragment_index: index as u16,
                fragment_count: fragment_count as u16,
                ops: chunk.ops,
            };
            self.snapshot_fragments_sent = self.snapshot_fragments_sent.saturating_add(1);
            self.send_encoded_message(client, addr, &ServerMessage::SnapshotFragment(fragment));
        }
    }

    fn snapshot_chunks_for_datagram(
        &mut self,
        client: ClientId,
        addr: SocketAddr,
        delta: DeltaSnapshot,
    ) -> Vec<DeltaSnapshot> {
        let overhead = snapshot_fragment_overhead(delta.baseline);
        if overhead >= self.max_datagram_bytes {
            self.log_oversized_datagram(client, addr, overhead);
            return Vec::new();
        }

        let mut chunks = Vec::new();
        let mut current_ops = Vec::new();
        let mut current_len = overhead;
        for op in delta.ops {
            let op_len = delta_op_wire_len(&op);
            if overhead.saturating_add(op_len) > self.max_datagram_bytes {
                self.log_oversized_datagram(client, addr, overhead.saturating_add(op_len));
                continue;
            }

            if current_len.saturating_add(op_len) > self.max_datagram_bytes
                && !current_ops.is_empty()
            {
                chunks.push(DeltaSnapshot::new(
                    delta.sequence,
                    delta.baseline,
                    std::mem::take(&mut current_ops),
                ));
                current_len = overhead;
            }

            current_len = current_len.saturating_add(op_len);
            current_ops.push(op);
        }

        if !current_ops.is_empty() {
            chunks.push(DeltaSnapshot::new(
                delta.sequence,
                delta.baseline,
                current_ops,
            ));
        }

        chunks
    }

    fn send_encoded_message(
        &mut self,
        client: ClientId,
        addr: SocketAddr,
        message: &ServerMessage,
    ) {
        match encode_server_message(message) {
            Ok(bytes) => {
                self.send_datagram(client, addr, bytes);
            }
            Err(error) => eprintln!("encode error for client {}: {error}", client.raw()),
        }
    }

    fn send_datagram(&mut self, client: ClientId, addr: SocketAddr, bytes: Vec<u8>) {
        if bytes.len() > self.max_datagram_bytes {
            self.log_oversized_datagram(client, addr, bytes.len());
            return;
        }
        self.datagrams_sent = self.datagrams_sent.saturating_add(1);
        self.bytes_sent = self.bytes_sent.saturating_add(bytes.len() as u64);
        if let Err(error) = self.socket.send_to(&bytes, addr) {
            eprintln!("send error to client {} at {addr}: {error}", client.raw());
        }
    }

    fn log_oversized_datagram(&mut self, client: ClientId, addr: SocketAddr, len: usize) {
        self.oversized_datagrams = self.oversized_datagrams.saturating_add(1);
        if self.oversized_datagrams <= 3 || self.oversized_datagrams % 100 == 0 {
            eprintln!(
                "drop oversized datagram to client {} at {addr}: bytes={} max={}",
                client.raw(),
                len,
                self.max_datagram_bytes
            );
        }
    }
}

fn snapshot_fragment_overhead(baseline: Option<gridwake_core::SnapshotId>) -> usize {
    let header = 4;
    let sequence = 8;
    let baseline_flag = 1;
    let baseline_sequence = if baseline.is_some() { 8 } else { 0 };
    let fragment_index = 2;
    let fragment_count = 2;
    let op_count = 4;
    header
        + sequence
        + baseline_flag
        + baseline_sequence
        + fragment_index
        + fragment_count
        + op_count
}

fn delta_op_wire_len(op: &DeltaOp) -> usize {
    match op {
        DeltaOp::SpawnOrEnter { payload, .. } | DeltaOp::Update { payload, .. } => {
            1 + 8 + 4 + payload.len()
        }
        DeltaOp::DespawnOrExit { .. } => 1 + 8,
    }
}

fn main() -> io::Result<()> {
    let args = parse_args(env::args().skip(1))?;
    let mut runtime = ServerRuntime::new(ServerConfig {
        tick_rate_hz: args.tick_rate_hz,
        cell_size: 32.0,
        default_interest_radius: args.interest_radius,
        per_client_byte_budget: args.byte_budget,
        ..ServerConfig::default()
    });
    let mut transport = DemoUdpTransport::bind(&args.bind, args.max_datagram_bytes)?;
    let mut demo_entities = seed_demo_entities(&mut runtime, &args);
    let mut covers = seed_demo_covers(&mut runtime, &args);
    let cover_index = CoverIndex::new(&covers, COVER_INDEX_CELL_SIZE);
    let mut cover_candidates = Vec::new();
    let mut clients = HashMap::new();
    let mut perf_totals = PerfTotals::default();

    println!(
        "gridwake Godot PSX demo server listening on {} bots={} effects={} cover={} tick_rate={} radius={} budget={} max_datagram={} run_ticks={}",
        transport.local_addr()?,
        args.bots,
        args.effects,
        args.cover,
        args.tick_rate_hz,
        args.interest_radius,
        args.byte_budget,
        args.max_datagram_bytes,
        args.run_ticks
            .map(|ticks| ticks.to_string())
            .unwrap_or_else(|| "infinite".to_owned())
    );

    let tick_interval = Duration::from_secs_f64(1.0 / f64::from(args.tick_rate_hz));
    let started = Instant::now();
    let mut next_tick = Instant::now();
    let mut tick_count = 0_u64;

    loop {
        handle_inbound(
            &mut runtime,
            &mut transport,
            &mut clients,
            &mut covers,
            &cover_index,
            &mut cover_candidates,
            tick_count,
        );

        let now = Instant::now();
        if now >= next_tick {
            let tick_started = Instant::now();
            tick_count = tick_count.saturating_add(1);
            let seconds = started.elapsed().as_secs_f32();
            let update_started = Instant::now();
            update_demo_entities(&mut runtime, &mut demo_entities, seconds);
            let update_micros = update_started.elapsed().as_micros();
            let world_started = Instant::now();
            let world_events = update_destructible_world(
                &mut runtime,
                &demo_entities,
                &mut covers,
                &cover_index,
                &mut cover_candidates,
                seconds,
                tick_count,
            );
            let world_micros = world_started.elapsed().as_micros();
            let runtime_started = Instant::now();
            let metrics = runtime.advance_tick(&mut transport);
            let runtime_micros = runtime_started.elapsed().as_micros();
            let sample = TickPerfSample {
                update_micros,
                world_micros,
                runtime_micros,
                tick_micros: tick_started.elapsed().as_micros(),
            };
            perf_totals.record(sample);

            if metrics.tick.raw() % args.log_every_ticks == 0 {
                println!(
                    "tick={} clients={} entities={} aoi={} selected={} lod={}/{}/{} deferred={} bytes={} messages={} cover_hits={} cover_destroyed={} ms={:.3}/{:.3}/{:.3}/{:.3} datagrams={} net_bytes={} fragments={}",
                    metrics.tick.raw(),
                    metrics.clients,
                    metrics.entities,
                    metrics.aoi_candidates,
                    metrics.selected_updates,
                    metrics.selected_full_lod_updates,
                    metrics.selected_reduced_lod_updates,
                    metrics.selected_minimal_lod_updates,
                    metrics.deferred_updates,
                    metrics.bytes_scheduled,
                    metrics.messages_sent,
                    world_events.cover_hits,
                    world_events.cover_destroyed,
                    sample.tick_micros as f64 / 1_000.0,
                    sample.update_micros as f64 / 1_000.0,
                    sample.world_micros as f64 / 1_000.0,
                    sample.runtime_micros as f64 / 1_000.0,
                    transport.datagrams_sent,
                    transport.bytes_sent,
                    transport.snapshot_fragments_sent
                );
            }

            if args
                .run_ticks
                .is_some_and(|run_ticks| tick_count >= run_ticks)
            {
                println!(
                    "summary ticks={} avg_tick_ms={:.3} max_tick_ms={:.3} avg_update_ms={:.3} avg_world_ms={:.3} avg_runtime_ms={:.3} datagrams={} net_bytes={} fragments={} oversized={}",
                    perf_totals.ticks,
                    perf_totals.avg_tick_ms(),
                    perf_totals.max_tick_ms(),
                    perf_totals.avg_update_ms(),
                    perf_totals.avg_world_ms(),
                    perf_totals.avg_runtime_ms(),
                    transport.datagrams_sent,
                    transport.bytes_sent,
                    transport.snapshot_fragments_sent,
                    transport.oversized_datagrams
                );
                return Ok(());
            }

            next_tick += tick_interval;
            if next_tick < now {
                next_tick = now + tick_interval;
            }
        } else {
            thread::sleep((next_tick - now).min(Duration::from_millis(2)));
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct WorldEventStats {
    cover_hits: usize,
    cover_destroyed: usize,
}

fn handle_inbound(
    runtime: &mut ServerRuntime,
    transport: &mut DemoUdpTransport,
    clients: &mut HashMap<ClientId, ClientState>,
    covers: &mut [DemoCover],
    cover_index: &CoverIndex,
    cover_candidates: &mut Vec<usize>,
    tick: u64,
) {
    for inbound in transport.drain_client_messages() {
        if inbound.newly_connected {
            connect_demo_client(runtime, clients, inbound.client);
        }

        match inbound.message {
            ClientMessage::AckSnapshot { sequence } => {
                runtime.receive(inbound.client, ClientMessage::AckSnapshot { sequence });
            }
            ClientMessage::Input { payload } => {
                if let Some(input) = parse_client_input(&payload) {
                    if let Some(client) = clients.get_mut(&inbound.client) {
                        client.position = input.position;
                        client.yaw = input.yaw;
                        runtime.update_client_position(inbound.client, input.position);
                        runtime.move_entity(client.player_entity, input.position);
                        runtime.set_entity_lod_payloads(
                            client.player_entity,
                            player_payload(LOD_FULL, input.position, input.yaw),
                            player_payload(LOD_REDUCED, input.position, input.yaw),
                            player_payload(LOD_MINIMAL, input.position, input.yaw),
                        );
                        if input.fire && tick >= client.next_fire_tick {
                            let forward = Vec3::new(input.yaw.sin(), 0.0, input.yaw.cos());
                            let blast = Vec3::new(
                                input.position.x + forward.x * 18.0,
                                0.0,
                                input.position.z + forward.z * 18.0,
                            );
                            damage_covers(
                                runtime,
                                covers,
                                cover_index,
                                cover_candidates,
                                blast,
                                PLAYER_BLAST_RADIUS,
                                PLAYER_BLAST_DAMAGE,
                            );
                            client.next_fire_tick = tick.saturating_add(PLAYER_FIRE_COOLDOWN_TICKS);
                        }
                    }
                }
            }
        }
    }
}

fn connect_demo_client(
    runtime: &mut ServerRuntime,
    clients: &mut HashMap<ClientId, ClientState>,
    client: ClientId,
) {
    if clients.contains_key(&client) {
        return;
    }

    let spawn = player_spawn(client);
    let player_entity = EntityId::new(PLAYER_ENTITY_BASE + client.raw());
    runtime.connect_client(client, spawn, None);
    runtime.spawn_entity_with_lod_payloads(
        player_entity,
        spawn,
        payloads_for_entity(KIND_PLAYER, 3, spawn, 0.0, 1.2, 0.0, 0),
        10.0,
        NetworkLod::Full,
    );
    clients.insert(
        client,
        ClientState {
            player_entity,
            position: spawn,
            yaw: 0.0,
            next_fire_tick: 0,
        },
    );
    println!(
        "client {} connected as entity {}",
        client.raw(),
        player_entity.raw()
    );
}

fn seed_demo_entities(runtime: &mut ServerRuntime, args: &Args) -> Vec<DemoEntity> {
    let mut entities = Vec::with_capacity((args.bots + args.effects) as usize);
    for index in 0..args.bots {
        let entity = demo_entity(BOT_ENTITY_BASE + index, KIND_BOT, index, args.world_size);
        let position = entity.position_at(0.0);
        runtime.spawn_entity_with_lod_payloads(
            entity.entity,
            position,
            entity.payloads_at(0.0),
            1.0 + f32::from(entity.team) * 0.25,
            NetworkLod::Full,
        );
        entities.push(entity);
    }

    for index in 0..args.effects {
        let entity = demo_entity(
            EFFECT_ENTITY_BASE + index,
            KIND_EFFECT,
            index,
            args.world_size,
        );
        let position = entity.position_at(0.0);
        runtime.spawn_entity_with_lod_payloads(
            entity.entity,
            position,
            entity.payloads_at(0.0),
            0.8,
            NetworkLod::Reduced,
        );
        entities.push(entity);
    }
    entities
}

fn seed_demo_covers(runtime: &mut ServerRuntime, args: &Args) -> Vec<DemoCover> {
    let mut covers = Vec::with_capacity(args.cover as usize);
    let side = (args.cover as f32).sqrt().ceil() as u64;
    if side == 0 {
        return covers;
    }

    let spacing = (args.world_size / side as f32).max(6.0);
    let origin = -args.world_size * 0.5 + spacing * 0.5;
    for index in 0..args.cover {
        let x = index % side;
        let z = index / side;
        let offset_x = (hash_unit(index.wrapping_mul(41)) - 0.5) * spacing * 0.35;
        let offset_z = (hash_unit(index.wrapping_mul(59)) - 0.5) * spacing * 0.35;
        let position = Vec3::new(
            origin + x as f32 * spacing + offset_x,
            0.0,
            origin + z as f32 * spacing + offset_z,
        );
        let material = (index % 4) as u8;
        let radius = 1.6 + hash_unit(index.wrapping_mul(83)) * 1.4;
        let cover = DemoCover {
            entity: EntityId::new(COVER_ENTITY_BASE + index),
            position,
            radius,
            health: COVER_MAX_HEALTH,
            material,
        };
        runtime.spawn_entity_with_lod_payloads(
            cover.entity,
            cover.position,
            cover.payloads(),
            2.0,
            NetworkLod::Full,
        );
        covers.push(cover);
    }
    covers
}

fn update_demo_entities(runtime: &mut ServerRuntime, entities: &mut [DemoEntity], seconds: f32) {
    for entity in entities {
        let position = entity.position_at(seconds);
        let payloads = entity.payloads_at_position(seconds, position);
        runtime.move_entity_with_lod_payloads(entity.entity, position, payloads);
    }
}

fn update_destructible_world(
    runtime: &mut ServerRuntime,
    emitters: &[DemoEntity],
    covers: &mut [DemoCover],
    cover_index: &CoverIndex,
    cover_candidates: &mut Vec<usize>,
    seconds: f32,
    tick: u64,
) -> WorldEventStats {
    if tick % COVER_DAMAGE_INTERVAL_TICKS != 0 {
        return WorldEventStats::default();
    }

    let mut stats = WorldEventStats::default();
    for (emitter_index, emitter) in emitters
        .iter()
        .filter(|entity| entity.kind == KIND_EFFECT)
        .enumerate()
    {
        if (tick / COVER_DAMAGE_INTERVAL_TICKS + emitter_index as u64) % 9 != 0 {
            continue;
        }

        let blast = emitter.position_at(seconds);
        stats += damage_covers(
            runtime,
            covers,
            cover_index,
            cover_candidates,
            blast,
            COVER_DAMAGE_RADIUS,
            COVER_DAMAGE_PER_HIT,
        );
    }

    stats
}

impl std::ops::AddAssign for WorldEventStats {
    fn add_assign(&mut self, rhs: Self) {
        self.cover_hits += rhs.cover_hits;
        self.cover_destroyed += rhs.cover_destroyed;
    }
}

fn damage_covers(
    runtime: &mut ServerRuntime,
    covers: &mut [DemoCover],
    cover_index: &CoverIndex,
    cover_candidates: &mut Vec<usize>,
    center: Vec3,
    radius: f32,
    damage_amount: f32,
) -> WorldEventStats {
    let mut stats = WorldEventStats::default();
    let radius_squared = radius * radius;
    cover_index.query_indices(center, radius, cover_candidates);
    for &cover_slot in cover_candidates.iter() {
        let Some(cover) = covers.get_mut(cover_slot) else {
            continue;
        };
        if cover.is_destroyed() {
            continue;
        }
        let distance_squared = distance_squared_xz(center, cover.position);
        if distance_squared > radius_squared {
            continue;
        }

        let falloff = 1.0 - (distance_squared / radius_squared).sqrt();
        let damage = damage_amount * falloff.max(0.15);
        let previous_health = cover.health;
        cover.health = (cover.health - damage).max(0.0);
        if cover.health < previous_health {
            stats.cover_hits += 1;
            if previous_health > 0.0 && cover.is_destroyed() {
                stats.cover_destroyed += 1;
            }
            let payloads = cover.payloads();
            runtime.set_entity_lod_payloads(
                cover.entity,
                payloads.full,
                payloads.reduced,
                payloads.minimal,
            );
        }
    }

    stats
}

fn demo_entity(raw_entity: u64, kind: u8, index: u64, world_size: f32) -> DemoEntity {
    let lane = (index % 16) as f32;
    let anchor = Vec3::new(
        (hash_unit(index.wrapping_mul(17)) - 0.5) * world_size,
        0.5,
        (hash_unit(index.wrapping_mul(31)) - 0.5) * world_size,
    );
    DemoEntity {
        entity: EntityId::new(raw_entity),
        kind,
        team: (index % 3) as u8,
        anchor,
        orbit_radius: if kind == KIND_EFFECT {
            3.0 + lane * 0.4
        } else {
            1.5 + lane * 0.25
        },
        speed: if kind == KIND_EFFECT {
            1.6 + hash_unit(index) * 1.8
        } else {
            0.2 + hash_unit(index) * 0.8
        },
        phase: hash_unit(index.wrapping_mul(97)) * std::f32::consts::TAU,
        radius: if kind == KIND_EFFECT { 0.65 } else { 1.0 },
        style: (index % 7) as u32,
    }
}

fn payloads_for_entity(
    kind: u8,
    team: u8,
    position: Vec3,
    yaw: f32,
    radius: f32,
    phase: f32,
    style: u32,
) -> NetworkLodPayloads {
    NetworkLodPayloads::new(
        encode_demo_payload(DemoPayload {
            kind,
            team,
            lod: LOD_FULL,
            position,
            yaw,
            radius,
            phase,
            style,
        }),
        encode_demo_payload(DemoPayload {
            kind,
            team,
            lod: LOD_REDUCED,
            position,
            yaw,
            radius,
            phase,
            style,
        }),
        encode_demo_payload(DemoPayload {
            kind,
            team,
            lod: LOD_MINIMAL,
            position,
            yaw,
            radius,
            phase,
            style,
        }),
    )
}

fn player_payload(lod: u8, position: Vec3, yaw: f32) -> Vec<u8> {
    encode_demo_payload(DemoPayload {
        kind: KIND_PLAYER,
        team: 3,
        lod,
        position,
        yaw,
        radius: 1.2,
        phase: 0.0,
        style: 0,
    })
}

fn cover_style(material: u8, health_ratio: f32) -> u32 {
    let stage = if health_ratio <= 0.0 {
        3
    } else if health_ratio < 0.35 {
        2
    } else if health_ratio < 0.7 {
        1
    } else {
        0
    };
    u32::from(material) | ((stage as u32) << 8)
}

fn encode_demo_payload(payload: DemoPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(match payload.lod {
        LOD_FULL => 36,
        LOD_REDUCED => 28,
        _ => 24,
    });
    out.extend_from_slice(DEMO_PAYLOAD_MAGIC);
    out.push(1);
    out.push(payload.kind);
    out.push(payload.team);
    out.push(payload.lod);
    push_f32(&mut out, payload.position.x);
    push_f32(&mut out, payload.position.y);
    push_f32(&mut out, payload.position.z);
    push_f32(&mut out, payload.yaw);

    if payload.lod == LOD_FULL || payload.lod == LOD_REDUCED {
        push_f32(&mut out, payload.radius);
    }
    if payload.lod == LOD_FULL {
        push_f32(&mut out, payload.phase);
        out.extend_from_slice(&payload.style.to_le_bytes());
    }
    out
}

fn parse_client_input(payload: &[u8]) -> Option<ClientInput> {
    if payload.len() < 21 || &payload[0..4] != CLIENT_INPUT_MAGIC {
        return None;
    }
    let version = payload[4];
    if version != 1 && version != 2 {
        return None;
    }
    Some(ClientInput {
        position: Vec3::new(
            read_f32(payload, 5)?,
            read_f32(payload, 9)?,
            read_f32(payload, 13)?,
        ),
        yaw: read_f32(payload, 17)?,
        fire: version >= 2 && payload.get(21).copied().unwrap_or(0) & 1 != 0,
    })
}

fn player_spawn(client: ClientId) -> Vec3 {
    let angle = client.raw() as f32 * 1.618_034;
    Vec3::new(angle.cos() * 12.0, 0.5, angle.sin() * 12.0)
}

fn parse_args(args: impl Iterator<Item = String>) -> io::Result<Args> {
    let mut parsed = Args::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if matches!(arg.as_str(), "--help" | "-h") {
            print_usage();
            std::process::exit(0);
        }
        let Some(value) = args.next() else {
            return Err(invalid_input(format!("missing value for {arg}")));
        };
        match arg.as_str() {
            "--bind" => parsed.bind = value,
            "--bots" => parsed.bots = parse_positive(&arg, &value)?,
            "--effects" => parsed.effects = parse_positive(&arg, &value)?,
            "--cover" => parsed.cover = parse_positive(&arg, &value)?,
            "--tick-rate" => parsed.tick_rate_hz = parse_positive(&arg, &value)?,
            "--world-size" => parsed.world_size = parse_positive_f32(&arg, &value)?,
            "--radius" => parsed.interest_radius = parse_positive_f32(&arg, &value)?,
            "--budget" => parsed.byte_budget = parse_positive(&arg, &value)?,
            "--max-datagram" => parsed.max_datagram_bytes = parse_positive(&arg, &value)?,
            "--log-every" => parsed.log_every_ticks = parse_positive(&arg, &value)?,
            "--run-ticks" => parsed.run_ticks = Some(parse_positive(&arg, &value)?),
            _ => return Err(invalid_input(format!("unknown argument {arg}"))),
        }
    }
    Ok(parsed)
}

fn print_usage() {
    eprintln!(
        "usage: cargo run -p gridwake-server --example godot_psx_demo_server -- [--bind 127.0.0.1:3456] [--bots N] [--effects N] [--cover N] [--tick-rate HZ] [--world-size N] [--radius N] [--budget BYTES] [--max-datagram BYTES] [--log-every TICKS] [--run-ticks TICKS]"
    );
}

fn parse_positive<T>(name: &str, value: &str) -> io::Result<T>
where
    T: std::str::FromStr + PartialOrd + From<u8>,
{
    let parsed = value
        .parse::<T>()
        .map_err(|_| invalid_input(format!("invalid value for {name}: {value}")))?;
    if parsed <= T::from(0) {
        return Err(invalid_input(format!("{name} must be positive")));
    }
    Ok(parsed)
}

fn parse_positive_f32(name: &str, value: &str) -> io::Result<f32> {
    let parsed = value
        .parse::<f32>()
        .map_err(|_| invalid_input(format!("invalid value for {name}: {value}")))?;
    if !parsed.is_finite() || parsed <= 0.0 {
        return Err(invalid_input(format!("{name} must be positive and finite")));
    }
    Ok(parsed)
}

fn invalid_input(message: String) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message)
}

fn push_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_f32(bytes: &[u8], offset: usize) -> Option<f32> {
    Some(f32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn distance_squared_xz(left: Vec3, right: Vec3) -> f32 {
    let dx = left.x - right.x;
    let dz = left.z - right.z;
    dx.mul_add(dx, dz * dz)
}

fn hash_unit(value: u64) -> f32 {
    let mixed = value
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17)
        .wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (mixed >> 40) as f32 / (1u64 << 24) as f32
}
