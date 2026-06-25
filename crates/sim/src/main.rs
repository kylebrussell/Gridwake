use std::env;
use std::process::ExitCode;
use std::time::Instant;

use gridwake_core::{ClientId, EntityId, Vec3};
use gridwake_server::{FakeTransport, ServerConfig, ServerRuntime, TickScheduler, VecMetrics};

#[derive(Clone, Copy, Debug)]
struct SimArgs {
    scenario: Scenario,
    clients: u64,
    entities: u64,
    ticks: u64,
    tick_rate_hz: u16,
    world_size: f32,
    interest_radius: f32,
    byte_budget: usize,
}

impl Default for SimArgs {
    fn default() -> Self {
        Self {
            scenario: Scenario::Uniform,
            clients: 100,
            entities: 1_000,
            ticks: 10,
            tick_rate_hz: 20,
            world_size: 1_000.0,
            interest_radius: 96.0,
            byte_budget: 1_200,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Uniform,
    DenseHotspot,
    MovingBattlefront,
    SparseOpenWorld,
}

impl Scenario {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "uniform" => Some(Self::Uniform),
            "dense-hotspot" => Some(Self::DenseHotspot),
            "moving-battlefront" => Some(Self::MovingBattlefront),
            "sparse-open-world" => Some(Self::SparseOpenWorld),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Uniform => "uniform",
            Self::DenseHotspot => "dense-hotspot",
            Self::MovingBattlefront => "moving-battlefront",
            Self::SparseOpenWorld => "sparse-open-world",
        }
    }
}

fn main() -> ExitCode {
    let args = match parse_args(env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    let mut runtime = ServerRuntime::new(ServerConfig {
        tick_rate_hz: args.tick_rate_hz,
        default_interest_radius: args.interest_radius,
        per_client_byte_budget: args.byte_budget,
        ..ServerConfig::default()
    });
    seed_clients(&mut runtime, args);
    seed_entities(&mut runtime, args);

    let started = Instant::now();
    let mut transport = FakeTransport::default();
    let mut scheduler = TickScheduler::from_hz(args.tick_rate_hz, 8);
    let tick_interval = scheduler.tick_interval();
    let mut metrics_sink = VecMetrics::default();
    let mut total_selected = 0usize;
    let mut total_bytes = 0usize;
    let mut total_candidates = 0usize;

    println!(
        "gridwake-sim scenario={} clients={} entities={} ticks={} tick_rate_hz={} radius={} budget={}",
        args.scenario.as_str(),
        args.clients,
        args.entities,
        args.ticks,
        args.tick_rate_hz,
        args.interest_radius,
        args.byte_budget
    );

    for step in 0..args.ticks {
        move_entities(&mut runtime, args, step);
        transport.clear();
        let mut due_metrics = runtime.advance_elapsed(
            &mut scheduler,
            tick_interval,
            &mut transport,
            &mut metrics_sink,
        );
        let metrics = due_metrics
            .pop()
            .expect("one fixed scheduler tick should be due per simulation step");
        assert!(due_metrics.is_empty());
        total_selected += metrics.selected_updates;
        total_bytes += metrics.bytes_scheduled;
        total_candidates += metrics.aoi_candidates;

        println!(
            "tick={} candidates={} selected={} exits={} bytes={} messages={}",
            metrics.tick.raw(),
            metrics.aoi_candidates,
            metrics.selected_updates,
            metrics.exit_updates,
            metrics.bytes_scheduled,
            metrics.messages_sent
        );
    }

    let elapsed = started.elapsed();
    println!(
        "summary elapsed_ms={} avg_candidates_per_tick={:.2} avg_selected_per_tick={:.2} avg_bytes_per_tick={:.2}",
        elapsed.as_millis(),
        total_candidates as f64 / args.ticks as f64,
        total_selected as f64 / args.ticks as f64,
        total_bytes as f64 / args.ticks as f64
    );

    ExitCode::SUCCESS
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<SimArgs, String> {
    let mut parsed = SimArgs::default();
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        if matches!(arg.as_str(), "--help" | "-h") {
            print_usage();
            std::process::exit(0);
        }

        let Some(value) = args.next() else {
            return Err(format!("missing value for {arg}"));
        };

        match arg.as_str() {
            "--clients" => parsed.clients = parse_positive(&arg, &value)?,
            "--entities" => parsed.entities = parse_positive(&arg, &value)?,
            "--ticks" => parsed.ticks = parse_positive(&arg, &value)?,
            "--tick-rate" => parsed.tick_rate_hz = parse_positive(&arg, &value)?,
            "--scenario" => {
                parsed.scenario =
                    Scenario::parse(&value).ok_or_else(|| format!("unknown scenario {value}"))?;
            }
            "--world-size" => parsed.world_size = parse_f32(&arg, &value)?,
            "--radius" => parsed.interest_radius = parse_f32(&arg, &value)?,
            "--budget" => parsed.byte_budget = parse_positive(&arg, &value)?,
            _ => return Err(format!("unknown argument {arg}")),
        }
    }

    Ok(parsed)
}

fn parse_positive<T>(name: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + From<u8>,
{
    let parsed = value
        .parse::<T>()
        .map_err(|_| format!("invalid value for {name}: {value}"))?;
    if parsed <= T::from(0) {
        return Err(format!("{name} must be positive"));
    }
    Ok(parsed)
}

fn parse_f32(name: &str, value: &str) -> Result<f32, String> {
    let parsed = value
        .parse::<f32>()
        .map_err(|_| format!("invalid value for {name}: {value}"))?;
    if !parsed.is_finite() || parsed <= 0.0 {
        return Err(format!("{name} must be a positive finite number"));
    }
    Ok(parsed)
}

fn print_usage() {
    eprintln!(
        "usage: cargo run -p gridwake-sim -- [--scenario uniform|dense-hotspot|moving-battlefront|sparse-open-world] [--clients N] [--entities N] [--ticks N] [--tick-rate HZ] [--world-size N] [--radius N] [--budget N]"
    );
}

fn seed_clients(runtime: &mut ServerRuntime, args: SimArgs) {
    for id in 0..args.clients {
        runtime.connect_client(ClientId::new(id + 1), client_position(args, id), None);
    }
}

fn seed_entities(runtime: &mut ServerRuntime, args: SimArgs) {
    for id in 0..args.entities {
        let position = entity_position(args, id, 0);
        let payload = payload_for(id + 1, position.x, position.y);
        runtime.spawn_entity(EntityId::new(id + 1), position, payload, 32, 1.0);
    }
}

fn move_entities(runtime: &mut ServerRuntime, args: SimArgs, step: u64) {
    for id in 0..args.entities {
        let position = entity_position(args, id, step);
        let entity = EntityId::new(id + 1);
        runtime.move_entity(entity, position);
        runtime.set_entity_payload(entity, payload_for(id + 1, position.x, position.y));
    }
}

fn client_position(args: SimArgs, id: u64) -> Vec3 {
    match args.scenario {
        Scenario::Uniform | Scenario::SparseOpenWorld => {
            grid_position(id, args.clients, args.world_size, 0.0)
        }
        Scenario::DenseHotspot => hotspot_position(id, args.world_size, args.interest_radius * 0.8),
        Scenario::MovingBattlefront => {
            let y = spread_axis(id, args.clients, args.world_size);
            Vec3::new(args.world_size * 0.5, y, 0.0)
        }
    }
}

fn entity_position(args: SimArgs, id: u64, step: u64) -> Vec3 {
    match args.scenario {
        Scenario::Uniform => {
            let side = (args.entities as f32).sqrt().ceil() as u64;
            let spacing = args.world_size / side.max(1) as f32;
            grid_position(
                id,
                args.entities,
                args.world_size,
                step as f32 * spacing.min(1.5),
            )
        }
        Scenario::DenseHotspot => {
            let mut position = hotspot_position(id, args.world_size, args.interest_radius * 0.9);
            position.x = wrap_world(position.x + step as f32 * 0.75, args.world_size);
            position
        }
        Scenario::MovingBattlefront => {
            let base = grid_position(id, args.entities, args.world_size, 0.0);
            Vec3::new(
                wrap_world(base.x + step as f32 * 5.0, args.world_size),
                base.y,
                0.0,
            )
        }
        Scenario::SparseOpenWorld => {
            let x = hash_unit(id.wrapping_mul(17)) * args.world_size;
            let y = hash_unit(id.wrapping_mul(31)) * args.world_size;
            Vec3::new(wrap_world(x + step as f32 * 2.0, args.world_size), y, 0.0)
        }
    }
}

fn grid_position(id: u64, total: u64, world_size: f32, offset_x: f32) -> Vec3 {
    let side = (total as f32).sqrt().ceil() as u64;
    let spacing = world_size / side.max(1) as f32;
    let x = wrap_world((id % side) as f32 * spacing + offset_x, world_size);
    let y = (id / side) as f32 * spacing;
    Vec3::new(x, y, 0.0)
}

fn hotspot_position(id: u64, world_size: f32, radius: f32) -> Vec3 {
    let angle = hash_unit(id.wrapping_mul(97)) * std::f32::consts::TAU;
    let distance = hash_unit(id.wrapping_mul(193)).sqrt() * radius;
    let center = world_size * 0.5;
    Vec3::new(
        wrap_world(center + distance * angle.cos(), world_size),
        wrap_world(center + distance * angle.sin(), world_size),
        0.0,
    )
}

fn spread_axis(id: u64, total: u64, world_size: f32) -> f32 {
    if total <= 1 {
        return world_size * 0.5;
    }
    id as f32 / (total - 1) as f32 * world_size
}

fn wrap_world(value: f32, world_size: f32) -> f32 {
    value.rem_euclid(world_size)
}

fn hash_unit(value: u64) -> f32 {
    let mixed = value
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17)
        .wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (mixed >> 40) as f32 / (1u64 << 24) as f32
}

fn payload_for(id: u64, x: f32, y: f32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16);
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(&x.to_le_bytes());
    payload.extend_from_slice(&y.to_le_bytes());
    payload
}
