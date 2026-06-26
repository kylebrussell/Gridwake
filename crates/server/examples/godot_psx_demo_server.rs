use std::collections::HashMap;
use std::env;
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use gridwake_core::{ClientId, EntityId, Vec3};
use gridwake_protocol::{
    decode_client_message, encode_server_message, ClientMessage, RoutedClientMessage, ServerMessage,
};
use gridwake_server::{NetworkLod, NetworkLodPayloads, ServerConfig, ServerRuntime, Transport};

const RECV_BUFFER_BYTES: usize = 65_536;
const CLIENT_INPUT_MAGIC: &[u8; 4] = b"GWCI";
const DEMO_PAYLOAD_MAGIC: &[u8; 4] = b"GWPD";

const KIND_BOT: u8 = 0;
const KIND_PLAYER: u8 = 1;
const KIND_EFFECT: u8 = 2;

const LOD_FULL: u8 = 0;
const LOD_REDUCED: u8 = 1;
const LOD_MINIMAL: u8 = 2;

const BOT_ENTITY_BASE: u64 = 1;
const EFFECT_ENTITY_BASE: u64 = 2_000_000;
const PLAYER_ENTITY_BASE: u64 = 10_000_000;

#[derive(Clone, Debug)]
struct Args {
    bind: String,
    bots: u64,
    effects: u64,
    tick_rate_hz: u16,
    world_size: f32,
    interest_radius: f32,
    byte_budget: usize,
    max_datagram_bytes: usize,
    log_every_ticks: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3456".to_owned(),
            bots: 2_000,
            effects: 350,
            tick_rate_hz: 20,
            world_size: 512.0,
            interest_radius: 128.0,
            byte_budget: 700,
            max_datagram_bytes: 1_200,
            log_every_ticks: 20,
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

#[derive(Clone, Debug)]
struct ClientState {
    player_entity: EntityId,
    position: Vec3,
    yaw: f32,
}

#[derive(Clone, Copy, Debug)]
struct ClientInput {
    position: Vec3,
    yaw: f32,
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

        match encode_server_message(&message) {
            Ok(bytes) => {
                if bytes.len() > self.max_datagram_bytes {
                    self.oversized_datagrams = self.oversized_datagrams.saturating_add(1);
                    if self.oversized_datagrams <= 3 || self.oversized_datagrams % 100 == 0 {
                        eprintln!(
                            "drop oversized datagram to client {} at {addr}: bytes={} max={}",
                            client.raw(),
                            bytes.len(),
                            self.max_datagram_bytes
                        );
                    }
                    return;
                }
                if let Err(error) = self.socket.send_to(&bytes, addr) {
                    eprintln!("send error to client {} at {addr}: {error}", client.raw());
                }
            }
            Err(error) => eprintln!("encode error for client {}: {error}", client.raw()),
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
    let mut clients = HashMap::new();

    println!(
        "gridwake Godot PSX demo server listening on {} bots={} effects={} tick_rate={} radius={} budget={} max_datagram={}",
        transport.local_addr()?,
        args.bots,
        args.effects,
        args.tick_rate_hz,
        args.interest_radius,
        args.byte_budget,
        args.max_datagram_bytes
    );

    let tick_interval = Duration::from_secs_f64(1.0 / f64::from(args.tick_rate_hz));
    let started = Instant::now();
    let mut next_tick = Instant::now();

    loop {
        handle_inbound(&mut runtime, &mut transport, &mut clients);

        let now = Instant::now();
        if now >= next_tick {
            let seconds = started.elapsed().as_secs_f32();
            update_demo_entities(&mut runtime, &mut demo_entities, seconds);
            let metrics = runtime.advance_tick(&mut transport);

            if metrics.tick.raw() % args.log_every_ticks == 0 {
                println!(
                    "tick={} clients={} entities={} aoi={} selected={} lod={}/{}/{} deferred={} bytes={} messages={}",
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
                    metrics.messages_sent
                );
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

fn handle_inbound(
    runtime: &mut ServerRuntime,
    transport: &mut DemoUdpTransport,
    clients: &mut HashMap<ClientId, ClientState>,
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

fn update_demo_entities(runtime: &mut ServerRuntime, entities: &mut [DemoEntity], seconds: f32) {
    for entity in entities {
        let position = entity.position_at(seconds);
        runtime.move_entity(entity.entity, position);
        let payloads = entity.payloads_at(seconds);
        runtime.set_entity_lod_payloads(
            entity.entity,
            payloads.full,
            payloads.reduced,
            payloads.minimal,
        );
    }
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
    if payload.len() < 21 || &payload[0..4] != CLIENT_INPUT_MAGIC || payload[4] != 1 {
        return None;
    }
    Some(ClientInput {
        position: Vec3::new(
            read_f32(payload, 5)?,
            read_f32(payload, 9)?,
            read_f32(payload, 13)?,
        ),
        yaw: read_f32(payload, 17)?,
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
            "--tick-rate" => parsed.tick_rate_hz = parse_positive(&arg, &value)?,
            "--world-size" => parsed.world_size = parse_positive_f32(&arg, &value)?,
            "--radius" => parsed.interest_radius = parse_positive_f32(&arg, &value)?,
            "--budget" => parsed.byte_budget = parse_positive(&arg, &value)?,
            "--max-datagram" => parsed.max_datagram_bytes = parse_positive(&arg, &value)?,
            "--log-every" => parsed.log_every_ticks = parse_positive(&arg, &value)?,
            _ => return Err(invalid_input(format!("unknown argument {arg}"))),
        }
    }
    Ok(parsed)
}

fn print_usage() {
    eprintln!(
        "usage: cargo run -p gridwake-server --example godot_psx_demo_server -- [--bind 127.0.0.1:3456] [--bots N] [--effects N] [--tick-rate HZ] [--world-size N] [--radius N] [--budget BYTES] [--max-datagram BYTES] [--log-every TICKS]"
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

fn hash_unit(value: u64) -> f32 {
    let mixed = value
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17)
        .wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (mixed >> 40) as f32 / (1u64 << 24) as f32
}
