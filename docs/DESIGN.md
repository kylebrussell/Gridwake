# Gridwake Design Notes

Gridwake is a server runtime kernel, not a full game engine. Game code owns simulation semantics. Gridwake owns the server-side machinery that decides what each client should know, when it should be sent, and how to measure whether the server is keeping up.

## Architecture

The workspace uses crate-level boundaries so each subsystem can be tested independently:

- `gridwake-core` defines stable ids, ticks, sequence ids, byte budgets, and small math-neutral value types.
- `gridwake-aoi` indexes observer and entity positions. The first index is a uniform grid suitable for predictable tests and synthetic worlds.
- `gridwake-replication` tracks client visibility, entity dirty generations, priority accumulation, and byte-budgeted update selection.
- `gridwake-snapshot` represents snapshot frames and delta operations without choosing a serializer or transport.
- `gridwake-protocol` contains transport-neutral messages and a small versioned byte codec.
- `gridwake-server` composes the crates into an authoritative fixed-step tick shell, pumps inbound client messages, records metrics through sinks, retains bounded entity-position history for lag-compensation hooks, and tracks cell ownership for local versus cross-region event routing into region outboxes.
- `gridwake-sim` drives fake clients and entities through deterministic synthetic scenarios using the same fixed-step scheduler.

## Data Flow

```text
game/sim state
  -> AOI query per observer
  -> replication visibility + priority selection
  -> snapshot delta ops
  -> transport-neutral server message
  -> real or fake transport
```

The initial runtime sends snapshot delta operations through an in-process fake transport. Runtime history carries forward each client's known state, then diffs it against the latest retained acked baseline so dropped snapshots can be repaired by later deltas. Real transports should implement the same boundary later.

The protocol codec is intentionally narrow and dependency-free:

```text
typed client/server message
  -> Gridwake wire header
  -> little-endian ids, ticks, counts, payload lengths
  -> bounded decode config for payload and delta-op limits
```

The runtime can also be driven by elapsed wall-clock time:

```text
elapsed time
  -> fixed-step scheduler
  -> pump inbound client messages
  -> run due ticks
  -> record tick metrics
```

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
- AOI filtering before replication scheduling.
- Byte budgets enforced per client.
- Priority accumulation to reduce starvation.
- Fixed-step scheduling with catch-up caps.
- Inbound client messages are transport-neutral and pumped before due ticks.
- Lag-compensation history stores authoritative server positions by tick.
- Deterministic tests where possible.
- Metrics emitted from the first runnable path.

## Near-Term Gaps

- Cell/region ownership has in-process region outboxes; multi-worker dispatch is not implemented yet.
- Snapshot baselines are retained per client and used for runtime deltas; payload-level compression is not implemented yet.
- Lag-compensation support is exact-position history only; interpolation, hit shapes, and rewind physics are not implemented yet.
- Real socket transport adapters are not implemented yet.
- The simulation harness has deterministic named scenarios and fixed-step ticking, but still needs sustained benchmark reporting and larger default profiles.
