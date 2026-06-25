# Gridwake Design Notes

Gridwake is a server runtime kernel, not a full game engine. Game code owns simulation semantics. Gridwake owns the server-side machinery that decides what each client should know, when it should be sent, and how to measure whether the server is keeping up.

## Architecture

The workspace uses crate-level boundaries so each subsystem can be tested independently:

- `gridwake-core` defines stable ids, ticks, sequence ids, byte budgets, and small math-neutral value types.
- `gridwake-aoi` indexes observer and entity positions. The first index is a uniform grid suitable for predictable tests and synthetic worlds.
- `gridwake-replication` tracks client visibility, entity dirty generations, priority accumulation, network LOD byte estimates, and byte-budgeted update selection.
- `gridwake-snapshot` represents snapshot frames and delta operations without choosing a serializer or transport.
- `gridwake-protocol` contains transport-neutral messages and a small versioned byte codec.
- `gridwake-server` composes the crates into an authoritative fixed-step tick shell, adapts byte transports through the protocol codec, pumps inbound client messages, records metrics through sinks, applies selected network LODs to snapshot payloads, retains bounded entity-position history for lag-compensation hooks, and tracks cell ownership for local versus cross-region event routing into region outboxes.
- `gridwake-sim` drives fake clients and entities through deterministic synthetic scenarios using the same fixed-step scheduler and emits text or JSON summaries for repeatable load-test comparisons.

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

Network LOD is an explicit part of replication selection. Entities can provide full, reduced, and minimal payload variants; the replication graph budgets the selected variant's byte estimate, and the server inserts that selected payload into the snapshot frame.

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

The runtime can also be driven by elapsed wall-clock time:

```text
elapsed time
  -> fixed-step scheduler
  -> pump inbound client messages
  -> run due ticks
  -> record tick metrics
```

Simulation reports include per-tick runtime and step timing, AOI candidates, selected updates, exits, bytes scheduled, messages sent, average AOI set size per client, and bytes per client. Summary reports include average and max runtime duration plus client-normalized AOI and bandwidth metrics.

Lag-compensation hooks are intentionally minimal at this stage:

```text
server entity positions
  -> bounded per-entity tick history
  -> exact rewind lookup by tick
  -> future hit validation or rewind/interpolation policy
```

Cross-cell events are classified separately from state replication:

```text
source position/cell + target position/cell
  -> cell owner lookup
  -> local, cross-region, or unowned route
  -> target region outbox or unowned event queue
```

## Runtime Principles

- Server authoritative by default.
- Engine-neutral ids and payloads.
- Transport-neutral messages.
- Versioned message codec for future transport adapters.
- Codec-backed byte transport adapter for transport implementations.
- AOI filtering before replication scheduling.
- Byte budgets enforced per client.
- Network LOD byte estimates affect scheduling and emitted payloads.
- Priority accumulation to reduce starvation.
- Fixed-step scheduling with catch-up caps.
- Inbound client messages are transport-neutral and pumped before due ticks.
- Lag-compensation history stores authoritative server positions by tick.
- Deterministic tests where possible.
- Metrics emitted from the first runnable path, with JSON summaries for scripted load-test comparisons.

## Near-Term Gaps

- Cell/region ownership has in-process region outboxes; multi-worker dispatch is not implemented yet.
- Snapshot baselines are retained per client and used for runtime deltas; payload-level compression is not implemented yet.
- Network LOD is explicit per entity; adaptive per-client LOD policy is not implemented yet.
- Lag-compensation support is exact-position history only; interpolation, hit shapes, and rewind physics are not implemented yet.
- Real socket transport adapters are not implemented yet; the codec-backed byte adapter is the integration point.
- The simulation harness has deterministic named scenarios, fixed-step ticking, and text/JSON summaries, but still needs sustained benchmark profiles and external visualization.
