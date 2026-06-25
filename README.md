# Gridwake

Gridwake is an open-source Rust workspace for an engine-neutral, server-authoritative multiplayer runtime. It targets the layer between low-level game networking libraries and full game engines: AOI, replication graph primitives, snapshot deltas, bandwidth budgets, tick scheduling, metrics, and simulation tooling for dense shared-world servers.

The project is intentionally not a renderer, editor, physics engine, matchmaking service, transport library, or hosted backend. Transports and engines should plug into Gridwake instead of being baked into it.

## Current Workspace

| Crate | Purpose |
| --- | --- |
| `gridwake-core` | Shared ids, ticks, sequence ids, byte budgets, and math-neutral position types. |
| `gridwake-aoi` | Spatial interest management with a grid-backed AOI index. |
| `gridwake-replication` | Per-client visibility, dirty generations, priority accumulation, per-client network LOD byte estimates, and byte-budgeted selection. |
| `gridwake-snapshot` | Snapshot frames, delta ops, retained baseline history, and ack tracking. |
| `gridwake-protocol` | Transport-neutral client/server message enums, metric frames, and a versioned byte codec. |
| `gridwake-server` | Authoritative runtime shell using fake/memory/UDP codec transports, inbound message pumping, fixed-step scheduling, metrics sinks, AOI, customizable budget-aware hysteresis-stabilized per-client LOD payloads, acked snapshot deltas, budget-deferred update metrics, bounded interpolated lag-history and sphere-hit validation hooks, cell ownership, and dispatchable cross-cell event batches. |
| `gridwake-sim` | Runnable load-test harness with fake clients, fake entities, fixed-step ticks, and named synthetic scenarios. |

## Adjacent Projects

As of the initial project check on 2026-06-25:

- [Lightyear](https://github.com/cBournhonesque/lightyear) is a high-level Bevy networking library with prediction/rollback and world replication.
- [Bevy Replicon](https://github.com/simgine/bevy_replicon) is a server-authoritative replication crate for Bevy.
- [Renet](https://github.com/lucaspoffo/renet) focuses on client/server game networking transport primitives.
- [naia](https://github.com/naia-lib/naia) is a cross-platform Rust networking library for multiplayer games.
- [sdec](https://github.com/kplane-dev/sdec) and [`sdec-repgraph`](https://docs.rs/sdec-repgraph) overlap with snapshot delta encoding and replication graph concepts.
- [librg](https://github.com/zpl-c/librg) is C middleware for replication-oriented game state transfer.

Gridwake's non-duplicative direction is an engine-neutral replication and simulation kernel for large continuous worlds: AOI, replication scheduling, lag-compensation hooks, cell ownership, cross-cell routing and handoff, metrics, and load testing.

## Quick Start

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p gridwake-sim -- --scenario uniform --clients 100 --entities 1000 --ticks 10 --tick-rate 20
cargo run -p gridwake-sim -- --scenario dense-hotspot --clients 100 --entities 1000 --ticks 10
cargo run -p gridwake-sim -- --scenario moving-battlefront --clients 100 --entities 1000 --ticks 10
cargo run -p gridwake-sim -- --scenario sparse-open-world --clients 100 --entities 1000 --ticks 10 --report json
```

## Non-Goals

- No renderer, editor, scene graph, animation system, or physics engine.
- No matchmaking, account system, lobby service, or hosted backend replacement.
- No hard dependency on Bevy, Unity, Unreal, Godot, or any ECS.
- No hard dependency on a specific transport stack; byte transports plug in through the codec-backed adapter.

## License

Gridwake is dual-licensed under MIT OR Apache-2.0.

## Milestones

1. Scaffold the workspace, docs, CI-ready commands, and publishable crate namespace.
2. Implement grid AOI with observer/entity insert, update, remove, and interest queries.
3. Implement replication graph selection with priority accumulation, per-client byte budgets, and adaptive network LOD payload selection.
4. Add snapshot/delta baselines and ack handling.
5. Expand the simulation harness into larger repeatable benchmark scenarios.
6. Grow cell-event outboxes into dispatchable multi-worker handoff batches and cross-cell delivery infrastructure.
7. Add real transport adapters and production transport integrations once the fake transport pipeline remains stable.
