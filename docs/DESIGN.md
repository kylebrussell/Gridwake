# Gridwake Design Notes

Gridwake is a server runtime kernel, not a full game engine. Game code owns simulation semantics. Gridwake owns the server-side machinery that decides what each client should know, when it should be sent, and how to measure whether the server is keeping up.

## Architecture

The workspace uses crate-level boundaries so each subsystem can be tested independently:

- `gridwake-core` defines stable ids, ticks, sequence ids, byte budgets, and small math-neutral value types.
- `gridwake-aoi` indexes observer and entity positions. The first index is a uniform grid suitable for predictable tests and synthetic worlds.
- `gridwake-replication` tracks client visibility, entity dirty generations, priority accumulation, per-client network LOD byte estimates, and byte-budgeted update selection.
- `gridwake-snapshot` represents snapshot frames and delta operations without choosing a serializer or transport.
- `gridwake-protocol` contains transport-neutral messages and a small versioned byte codec.
- `gridwake-server` composes the crates into an authoritative fixed-step tick shell, adapts memory or UDP byte transports through the protocol codec, pumps inbound client messages, records metrics through sinks, applies customizable budget-aware hysteresis-stabilized per-client network LODs to snapshot payloads, reports per-LOD and budget-deferred update pressure, retains bounded entity-position history with exact and interpolated lag-compensation lookup plus rewound sphere-hit validation, and tracks cell ownership for local versus cross-region event routing into dispatchable region batches.
- `gridwake-sim` drives fake clients and entities through deterministic synthetic scenarios and named benchmark profiles using the same fixed-step scheduler, then emits text or JSON summaries for repeatable load-test comparisons.
- `examples/godot_psx_demo` is a thin Godot client integration: GDScript sends input through the transport-neutral client message codec, decodes snapshot deltas, and renders server-selected AOI state with simple PS1-style primitives.

## Data Flow

```text
game/sim state
  -> AOI query per observer
  -> replication visibility + priority + network LOD selection
  -> snapshot delta ops
  -> transport-neutral server message
  -> real or fake transport
```

The initial runtime sends snapshot delta operations through an in-process fake transport. Runtime history carries forward each client's known state, then diffs it against the latest retained acked baseline so dropped snapshots can be repaired by later deltas. Real transports should implement the same boundary later.

Network LOD is an explicit part of replication selection. Entities can provide full, reduced, and minimal payload variants; the server derives a per-client LOD from client-to-entity distance inside the interest radius and applies hysteresis using the last LOD sent to that client. Callers can override that per-tick classification through a selector hook before the entity quality cap and byte-budget fallback are applied. If the desired tier does not fit the remaining client byte budget, selection tries lower-detail variants before deferring the update. The server inserts the selected payload into the snapshot frame. The entity's configured LOD acts as an upper quality cap, so game code can force an entity down to reduced or minimal detail.

The protocol codec is intentionally narrow and dependency-free:

```text
typed client/server message
  -> Gridwake wire header
  -> little-endian ids, ticks, counts, payload lengths
  -> bounded decode config for payload and delta-op limits
```

The server transport boundary has two layers:

```text
real transport adapter
  -> send/drain client-addressed byte frames
  -> CodecTransport
  -> typed Gridwake Transport trait
  -> ServerRuntime
```

The first real socket adapter is a dependency-free UDP byte transport. It registers client socket addresses, records unknown-client or unknown-peer routing errors, and relies on the codec layer for typed Gridwake messages. Reliability, packet ordering, auth, NAT traversal, and production session lifecycle remain outside this adapter.

The Godot demo uses the same protocol boundary from a non-Rust engine. It keeps engine code limited to input encoding, snapshot decoding, acknowledgement, and presentation, while the Rust server remains authoritative over AOI, replication priority, LOD choice, and snapshot deltas.

Current replication byte budgets apply to selected payload bytes before protocol
envelope and per-op overhead. Until snapshot fragmentation or wire-size-aware
budgeting exists, UDP demo budgets should stay conservative enough for encoded
datagrams; the demo transport drops oversized datagrams instead of relying on
platform `send_to` errors.

The runtime can also be driven by elapsed wall-clock time:

```text
elapsed time
  -> fixed-step scheduler
  -> pump inbound client messages
  -> run due ticks
  -> record tick metrics
```

Simulation reports include per-tick runtime and step timing, AOI candidates, selected updates, selected full/reduced/minimal LOD counts, budget-deferred updates, exits, bytes scheduled, deferred bytes, messages sent, average AOI set size per client, and bytes per client. Named quick, baseline, hotspot, and scale profiles provide repeatable load-test entry points. Summary reports include average and max runtime duration plus client-normalized AOI, LOD mix, bandwidth, and budget-pressure metrics.

Lag-compensation hooks keep authoritative entity positions by server tick, reconstruct sub-tick positions between adjacent retained samples, and validate simple historical sphere hits:

```text
server entity positions
  -> bounded per-entity tick history
  -> exact rewind lookup by tick
  -> interpolated sub-tick lookup between adjacent samples
  -> ray versus rewound sphere validation
  -> future rewind-physics policy
```

Cross-cell events are classified separately from state replication:

```text
source position/cell + target position/cell
  -> cell owner lookup
  -> local, cross-region, or unowned route
  -> target region outbox or unowned event queue
  -> sorted region batches
  -> worker handoff sink
```

## Runtime Principles

- Server authoritative by default.
- Engine-neutral ids and payloads.
- Transport-neutral messages.
- Versioned message codec for future transport adapters.
- Codec-backed memory and UDP byte transport adapters for transport implementations.
- AOI filtering before replication scheduling.
- Byte budgets enforced per client.
- Budget-deferred updates remain dirty and are surfaced in metrics.
- Selected full, reduced, and minimal LOD update counts are surfaced in tick metrics.
- Hysteresis-stabilized per-client network LOD byte estimates affect scheduling and emitted payloads.
- Per-tick network LOD selector hooks can override distance classifications before entity quality caps and budget fallback are applied.
- LOD selection can degrade within configured payload tiers before deferring an update.
- Priority accumulation to reduce starvation.
- Fixed-step scheduling with catch-up caps.
- Inbound client messages are transport-neutral and pumped before due ticks.
- Cross-cell events drain through explicit region handoff batches.
- Lag-compensation history stores authoritative server positions by tick and supports interpolation plus sphere-hit validation.
- Deterministic tests where possible.
- Metrics emitted from the first runnable path, with JSON summaries for scripted load-test comparisons.

## Near-Term Gaps

- Cell/region ownership has dispatchable region batches; durable or networked multi-process delivery is not implemented yet.
- Snapshot baselines are retained per client and used for runtime deltas; payload-level compression is not implemented yet.
- Per-client network LOD is distance-band based with hysteresis, selector hooks, and budget-aware fallback; longer-term load feedback is not implemented yet.
- Lag-compensation support has exact/interpolated position lookup and sphere-hit validation; full rewind physics and engine collision integration are not implemented yet.
- A UDP byte adapter exists; production transport integrations, reliability, auth, and session lifecycle are not implemented yet.
- The simulation harness has deterministic named scenarios, benchmark profiles, fixed-step ticking, and text/JSON summaries, but still needs external visualization.
