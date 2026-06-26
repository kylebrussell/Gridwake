# Gridwake Godot PSX Demo

This is a grey-box++ Godot 4.7 two-team deathmatch client for exercising Gridwake as an engine-neutral server runtime. The Godot side is intentionally thin: it sends player input over UDP, decodes Gridwake snapshot deltas, and renders server-selected AOI state as low-fidelity PS1-style primitives. The Rust server owns team assignment, health, kills, score, respawns, bot fire, player blasts, destructible cover, and impact churn. Bots, effects, destructible cover, score pylons, and blast impacts are batched into `MultiMeshInstance3D` buckets by mesh/material; player entities remain regular scene nodes. The local arena uses batched box props so normal-window runs exercise something closer to an FPS map without depending on external art.

## Run

Start the Rust demo server:

```sh
cargo run -p gridwake-server --example godot_psx_demo_server
```

Open `examples/godot_psx_demo/project.godot` in Godot 4.7 and run the project.
For render performance, run it in a normal Godot window rather than headless:

```sh
godot --path examples/godot_psx_demo
```

The server default is a playable local match profile: 800 bots, 140 world effects, 520 destructible cover pieces, a 60-kill score limit, `--budget 2400`, and `--max-datagram 4096`.
The demo transport splits oversized snapshot deltas into fragment datagrams, and
the Godot client only acks a snapshot after all fragments have been reassembled.
Fragments are applied progressively as they arrive, so extreme stress profiles
still produce visible work while incomplete snapshots remain unacked.
Requested datagram sizes above 8192 bytes are clamped to that portable payload
ceiling to avoid platform-specific UDP `Message too long` failures.

Keyboard controls:

- `W` / `S` or Up / Down: move forward/back
- `A` / `D`: strafe
- Mouse: aim
- Left / Right or `Q` / `E`: keyboard turn fallback
- Space or Left Mouse: fire a server-applied blast that damages enemy players/bots, damages destructible cover, and spawns a short-lived replicated impact
- Escape: release/capture the mouse

For a lighter local smoke:

```sh
cargo run -p gridwake-server --example godot_psx_demo_server -- --bots 200 --effects 40 --cover 100
godot --path examples/godot_psx_demo
```

For a repeatable server-side benchmark:

```sh
cargo run --release -p gridwake-server --example godot_psx_demo_server -- --bots 2000 --effects 350 --cover 900 --budget 1400 --max-datagram 4096 --run-ticks 200
```

The HUD shows red/blue score, local team, health/respawn state, FPS, frame/process/physics time, draw calls, render objects/primitives, memory, visible entity count, instanced bucket counts, cover/impact/score-pylon counts, snapshot sequence, packet backlog, pending fragments, and decoded op count. The Godot client also prints one `perf ...` line per second by default; headless runs are useful for script/protocol validation, but normal-window runs are the meaningful render benchmark. The server logs AOI candidates, selected updates, LOD mix, deferred updates, bytes, message counts, score, kills, combat hits, cover damage, impact churn, timing buckets, datagrams, bytes sent, and fragments sent.
