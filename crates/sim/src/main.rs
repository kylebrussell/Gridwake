use std::env;
use std::process::ExitCode;
use std::time::Instant;

use gridwake_core::{ClientId, EntityId, Vec3};
use gridwake_server::{FakeTransport, ServerConfig, ServerRuntime, TickScheduler, VecMetrics};

#[derive(Clone, Copy, Debug)]
struct SimArgs {
    profile: Option<BenchmarkProfile>,
    scenario: Scenario,
    report: ReportFormat,
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
            profile: None,
            scenario: Scenario::Uniform,
            report: ReportFormat::Text,
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

impl SimArgs {
    fn profile_name(self) -> &'static str {
        self.profile
            .map(BenchmarkProfile::as_str)
            .unwrap_or("custom")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReportFormat {
    Text,
    Json,
}

impl ReportFormat {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BenchmarkProfile {
    Quick,
    Baseline,
    Hotspot,
    Scale,
}

impl BenchmarkProfile {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "quick" => Some(Self::Quick),
            "baseline" => Some(Self::Baseline),
            "hotspot" => Some(Self::Hotspot),
            "scale" => Some(Self::Scale),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Baseline => "baseline",
            Self::Hotspot => "hotspot",
            Self::Scale => "scale",
        }
    }

    fn args(self) -> SimArgs {
        let mut args = SimArgs {
            profile: Some(self),
            ..SimArgs::default()
        };
        match self {
            Self::Quick => {}
            Self::Baseline => {
                args.clients = 250;
                args.entities = 2_500;
                args.ticks = 30;
            }
            Self::Hotspot => {
                args.scenario = Scenario::DenseHotspot;
                args.clients = 250;
                args.entities = 2_500;
                args.ticks = 30;
            }
            Self::Scale => {
                args.clients = 1_000;
                args.entities = 10_000;
                args.ticks = 10;
            }
        }
        args
    }
}

#[derive(Default)]
struct SimArgOverrides {
    scenario: Option<Scenario>,
    report: Option<ReportFormat>,
    clients: Option<u64>,
    entities: Option<u64>,
    ticks: Option<u64>,
    tick_rate_hz: Option<u16>,
    world_size: Option<f32>,
    interest_radius: Option<f32>,
    byte_budget: Option<usize>,
}

impl SimArgOverrides {
    fn apply_to(self, args: &mut SimArgs) {
        if let Some(scenario) = self.scenario {
            args.scenario = scenario;
        }
        if let Some(report) = self.report {
            args.report = report;
        }
        if let Some(clients) = self.clients {
            args.clients = clients;
        }
        if let Some(entities) = self.entities {
            args.entities = entities;
        }
        if let Some(ticks) = self.ticks {
            args.ticks = ticks;
        }
        if let Some(tick_rate_hz) = self.tick_rate_hz {
            args.tick_rate_hz = tick_rate_hz;
        }
        if let Some(world_size) = self.world_size {
            args.world_size = world_size;
        }
        if let Some(interest_radius) = self.interest_radius {
            args.interest_radius = interest_radius;
        }
        if let Some(byte_budget) = self.byte_budget {
            args.byte_budget = byte_budget;
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TickSample {
    runtime_micros: u128,
    step_micros: u128,
    aoi_candidates: usize,
    selected_updates: usize,
    selected_full_lod_updates: usize,
    selected_reduced_lod_updates: usize,
    selected_minimal_lod_updates: usize,
    deferred_updates: usize,
    exit_updates: usize,
    bytes_scheduled: usize,
    deferred_bytes: usize,
    messages_sent: usize,
}

#[derive(Debug)]
struct SimReport {
    args: SimArgs,
    samples: Vec<TickSample>,
    elapsed_micros: u128,
}

impl SimReport {
    fn total_candidates(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.aoi_candidates)
            .sum()
    }

    fn total_selected(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.selected_updates)
            .sum()
    }

    fn total_full_lod_updates(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.selected_full_lod_updates)
            .sum()
    }

    fn total_reduced_lod_updates(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.selected_reduced_lod_updates)
            .sum()
    }

    fn total_minimal_lod_updates(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.selected_minimal_lod_updates)
            .sum()
    }

    fn total_deferred(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.deferred_updates)
            .sum()
    }

    fn total_exits(&self) -> usize {
        self.samples.iter().map(|sample| sample.exit_updates).sum()
    }

    fn total_bytes(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.bytes_scheduled)
            .sum()
    }

    fn total_deferred_bytes(&self) -> usize {
        self.samples
            .iter()
            .map(|sample| sample.deferred_bytes)
            .sum()
    }

    fn total_messages(&self) -> usize {
        self.samples.iter().map(|sample| sample.messages_sent).sum()
    }

    fn avg_candidates_per_tick(&self) -> f64 {
        self.total_candidates() as f64 / self.samples.len() as f64
    }

    fn avg_candidates_per_client_tick(&self) -> f64 {
        self.avg_candidates_per_tick() / self.args.clients as f64
    }

    fn avg_selected_per_tick(&self) -> f64 {
        self.total_selected() as f64 / self.samples.len() as f64
    }

    fn avg_full_lod_updates_per_tick(&self) -> f64 {
        self.total_full_lod_updates() as f64 / self.samples.len() as f64
    }

    fn avg_reduced_lod_updates_per_tick(&self) -> f64 {
        self.total_reduced_lod_updates() as f64 / self.samples.len() as f64
    }

    fn avg_minimal_lod_updates_per_tick(&self) -> f64 {
        self.total_minimal_lod_updates() as f64 / self.samples.len() as f64
    }

    fn avg_deferred_per_tick(&self) -> f64 {
        self.total_deferred() as f64 / self.samples.len() as f64
    }

    fn avg_bytes_per_tick(&self) -> f64 {
        self.total_bytes() as f64 / self.samples.len() as f64
    }

    fn avg_bytes_per_client_tick(&self) -> f64 {
        self.avg_bytes_per_tick() / self.args.clients as f64
    }

    fn avg_deferred_bytes_per_tick(&self) -> f64 {
        self.total_deferred_bytes() as f64 / self.samples.len() as f64
    }

    fn avg_messages_per_tick(&self) -> f64 {
        self.total_messages() as f64 / self.samples.len() as f64
    }

    fn avg_runtime_ms(&self) -> f64 {
        avg_micros(self.samples.iter().map(|sample| sample.runtime_micros))
    }

    fn max_runtime_ms(&self) -> f64 {
        micros_to_ms(
            self.samples
                .iter()
                .map(|sample| sample.runtime_micros)
                .max()
                .unwrap_or(0),
        )
    }

    fn avg_step_ms(&self) -> f64 {
        avg_micros(self.samples.iter().map(|sample| sample.step_micros))
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
    let mut samples = Vec::with_capacity(args.ticks as usize);

    println!(
        "gridwake-sim profile={} scenario={} clients={} entities={} ticks={} tick_rate_hz={} radius={} budget={} report={}",
        args.profile_name(),
        args.scenario.as_str(),
        args.clients,
        args.entities,
        args.ticks,
        args.tick_rate_hz,
        args.interest_radius,
        args.byte_budget,
        args.report.as_str()
    );

    for step in 0..args.ticks {
        let step_started = Instant::now();
        move_entities(&mut runtime, args, step);
        transport.clear();
        let runtime_started = Instant::now();
        let mut due_metrics = runtime.advance_elapsed(
            &mut scheduler,
            tick_interval,
            &mut transport,
            &mut metrics_sink,
        );
        let runtime_micros = runtime_started.elapsed().as_micros();
        let metrics = due_metrics
            .pop()
            .expect("one fixed scheduler tick should be due per simulation step");
        assert!(due_metrics.is_empty());
        let step_micros = step_started.elapsed().as_micros();
        samples.push(TickSample {
            runtime_micros,
            step_micros,
            aoi_candidates: metrics.aoi_candidates,
            selected_updates: metrics.selected_updates,
            selected_full_lod_updates: metrics.selected_full_lod_updates,
            selected_reduced_lod_updates: metrics.selected_reduced_lod_updates,
            selected_minimal_lod_updates: metrics.selected_minimal_lod_updates,
            deferred_updates: metrics.deferred_updates,
            exit_updates: metrics.exit_updates,
            bytes_scheduled: metrics.bytes_scheduled,
            deferred_bytes: metrics.deferred_bytes,
            messages_sent: metrics.messages_sent,
        });

        println!(
            "tick={} runtime_ms={:.3} step_ms={:.3} candidates={} selected={} lod_full={} lod_reduced={} lod_minimal={} deferred={} exits={} bytes={} deferred_bytes={} messages={} avg_aoi_per_client={:.2} bytes_per_client={:.2}",
            metrics.tick.raw(),
            micros_to_ms(runtime_micros),
            micros_to_ms(step_micros),
            metrics.aoi_candidates,
            metrics.selected_updates,
            metrics.selected_full_lod_updates,
            metrics.selected_reduced_lod_updates,
            metrics.selected_minimal_lod_updates,
            metrics.deferred_updates,
            metrics.exit_updates,
            metrics.bytes_scheduled,
            metrics.deferred_bytes,
            metrics.messages_sent,
            metrics.aoi_candidates as f64 / args.clients as f64,
            metrics.bytes_scheduled as f64 / args.clients as f64
        );
    }

    let elapsed = started.elapsed();
    let report = SimReport {
        args,
        samples,
        elapsed_micros: elapsed.as_micros(),
    };
    print_report(&report);

    ExitCode::SUCCESS
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<SimArgs, String> {
    let mut profile = None;
    let mut overrides = SimArgOverrides::default();
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
            "--profile" => {
                profile = Some(
                    BenchmarkProfile::parse(&value)
                        .ok_or_else(|| format!("unknown benchmark profile {value}"))?,
                );
            }
            "--clients" => overrides.clients = Some(parse_positive(&arg, &value)?),
            "--entities" => overrides.entities = Some(parse_positive(&arg, &value)?),
            "--ticks" => overrides.ticks = Some(parse_positive(&arg, &value)?),
            "--tick-rate" => overrides.tick_rate_hz = Some(parse_positive(&arg, &value)?),
            "--report" => {
                overrides.report = Some(
                    ReportFormat::parse(&value)
                        .ok_or_else(|| format!("unknown report format {value}"))?,
                );
            }
            "--scenario" => {
                overrides.scenario = Some(
                    Scenario::parse(&value).ok_or_else(|| format!("unknown scenario {value}"))?,
                );
            }
            "--world-size" => overrides.world_size = Some(parse_f32(&arg, &value)?),
            "--radius" => overrides.interest_radius = Some(parse_f32(&arg, &value)?),
            "--budget" => overrides.byte_budget = Some(parse_positive(&arg, &value)?),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }

    let mut parsed = profile.map(BenchmarkProfile::args).unwrap_or_default();
    overrides.apply_to(&mut parsed);

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
        "usage: cargo run -p gridwake-sim -- [--profile quick|baseline|hotspot|scale] [--scenario uniform|dense-hotspot|moving-battlefront|sparse-open-world] [--clients N] [--entities N] [--ticks N] [--tick-rate HZ] [--world-size N] [--radius N] [--budget N] [--report text|json]"
    );
}

fn print_report(report: &SimReport) {
    match report.args.report {
        ReportFormat::Text => print_text_report(report),
        ReportFormat::Json => print_json_report(report),
    }
}

fn print_text_report(report: &SimReport) {
    println!(
        "summary elapsed_ms={:.3} avg_runtime_ms={:.3} max_runtime_ms={:.3} avg_step_ms={:.3} avg_candidates_per_tick={:.2} avg_aoi_per_client_tick={:.2} avg_selected_per_tick={:.2} avg_lod_full_per_tick={:.2} avg_lod_reduced_per_tick={:.2} avg_lod_minimal_per_tick={:.2} avg_deferred_per_tick={:.2} avg_bytes_per_tick={:.2} avg_bytes_per_client_tick={:.2} avg_deferred_bytes_per_tick={:.2} avg_messages_per_tick={:.2} total_deferred={} total_exits={}",
        micros_to_ms(report.elapsed_micros),
        report.avg_runtime_ms(),
        report.max_runtime_ms(),
        report.avg_step_ms(),
        report.avg_candidates_per_tick(),
        report.avg_candidates_per_client_tick(),
        report.avg_selected_per_tick(),
        report.avg_full_lod_updates_per_tick(),
        report.avg_reduced_lod_updates_per_tick(),
        report.avg_minimal_lod_updates_per_tick(),
        report.avg_deferred_per_tick(),
        report.avg_bytes_per_tick(),
        report.avg_bytes_per_client_tick(),
        report.avg_deferred_bytes_per_tick(),
        report.avg_messages_per_tick(),
        report.total_deferred(),
        report.total_exits()
    );
}

fn print_json_report(report: &SimReport) {
    println!(
        "summary_json={{\"profile\":\"{}\",\"scenario\":\"{}\",\"clients\":{},\"entities\":{},\"ticks\":{},\"tick_rate_hz\":{},\"world_size\":{},\"interest_radius\":{},\"byte_budget\":{},\"elapsed_ms\":{:.3},\"avg_runtime_ms\":{:.3},\"max_runtime_ms\":{:.3},\"avg_step_ms\":{:.3},\"avg_candidates_per_tick\":{:.3},\"avg_aoi_per_client_tick\":{:.3},\"avg_selected_per_tick\":{:.3},\"avg_lod_full_per_tick\":{:.3},\"avg_lod_reduced_per_tick\":{:.3},\"avg_lod_minimal_per_tick\":{:.3},\"avg_deferred_per_tick\":{:.3},\"avg_bytes_per_tick\":{:.3},\"avg_bytes_per_client_tick\":{:.3},\"avg_deferred_bytes_per_tick\":{:.3},\"avg_messages_per_tick\":{:.3},\"total_deferred\":{},\"total_exits\":{}}}",
        report.args.profile_name(),
        report.args.scenario.as_str(),
        report.args.clients,
        report.args.entities,
        report.args.ticks,
        report.args.tick_rate_hz,
        report.args.world_size,
        report.args.interest_radius,
        report.args.byte_budget,
        micros_to_ms(report.elapsed_micros),
        report.avg_runtime_ms(),
        report.max_runtime_ms(),
        report.avg_step_ms(),
        report.avg_candidates_per_tick(),
        report.avg_candidates_per_client_tick(),
        report.avg_selected_per_tick(),
        report.avg_full_lod_updates_per_tick(),
        report.avg_reduced_lod_updates_per_tick(),
        report.avg_minimal_lod_updates_per_tick(),
        report.avg_deferred_per_tick(),
        report.avg_bytes_per_tick(),
        report.avg_bytes_per_client_tick(),
        report.avg_deferred_bytes_per_tick(),
        report.avg_messages_per_tick(),
        report.total_deferred(),
        report.total_exits()
    );
}

fn micros_to_ms(micros: u128) -> f64 {
    micros as f64 / 1_000.0
}

fn avg_micros(values: impl Iterator<Item = u128>) -> f64 {
    let mut total = 0u128;
    let mut count = 0u128;
    for value in values {
        total += value;
        count += 1;
    }

    if count == 0 {
        0.0
    } else {
        micros_to_ms(total / count)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_test_args(args: &[&str]) -> Result<SimArgs, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn scale_profile_applies_repeatable_defaults() {
        let args = parse_test_args(&["--profile", "scale"]).unwrap();

        assert_eq!(args.profile, Some(BenchmarkProfile::Scale));
        assert_eq!(args.profile_name(), "scale");
        assert_eq!(args.scenario, Scenario::Uniform);
        assert_eq!(args.clients, 1_000);
        assert_eq!(args.entities, 10_000);
        assert_eq!(args.ticks, 10);
        assert_eq!(args.tick_rate_hz, 20);
        assert_eq!(args.interest_radius, 96.0);
        assert_eq!(args.byte_budget, 1_200);
    }

    #[test]
    fn explicit_args_override_profile_defaults_independent_of_order() {
        let args = parse_test_args(&[
            "--clients",
            "25",
            "--profile",
            "hotspot",
            "--ticks",
            "2",
            "--scenario",
            "sparse-open-world",
            "--report",
            "json",
        ])
        .unwrap();

        assert_eq!(args.profile, Some(BenchmarkProfile::Hotspot));
        assert_eq!(args.scenario, Scenario::SparseOpenWorld);
        assert_eq!(args.report, ReportFormat::Json);
        assert_eq!(args.clients, 25);
        assert_eq!(args.entities, 2_500);
        assert_eq!(args.ticks, 2);
    }

    #[test]
    fn unknown_profile_is_rejected() {
        let error = parse_test_args(&["--profile", "unknown"]).unwrap_err();

        assert_eq!(error, "unknown benchmark profile unknown");
    }
}
