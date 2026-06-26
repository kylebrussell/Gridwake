# Gridwake Godot PSX Demo

This is a grey-box++ Godot 4.7 client for exercising Gridwake as an engine-neutral server runtime. The Godot side is intentionally thin: it sends player input over UDP, decodes Gridwake snapshot deltas, and renders server-selected AOI state as low-fidelity PS1-style primitives. Bots, effects, and cover are batched into `MultiMeshInstance3D` buckets by mesh/material; player entities remain regular scene nodes.

## Run

Start the Rust demo server:

```sh
cargo run -p gridwake-server --example godot_psx_demo_server -- --bots 2000 --effects 350 --cover 900
```

Open `examples/godot_psx_demo/project.godot` in Godot 4.7 and run the project.
For render performance, run it in a normal Godot window rather than headless:

```sh
godot --path examples/godot_psx_demo
```

The server defaults to a conservative `--budget 700` and `--max-datagram 1200`.
The demo transport splits oversized snapshot deltas into fragment datagrams, and
the Godot client only acks a snapshot after all fragments have been reassembled.

Keyboard controls:

- `W` / `S` or Up / Down: move forward/back
- `A` / `D` or Left / Right: turn
- `Q` / `E`: strafe
- Space or Left Mouse: fire a server-applied blast into destructible cover

For a lighter local smoke:

```sh
cargo run -p gridwake-server --example godot_psx_demo_server -- --bots 200 --effects 40 --cover 100
godot --path examples/godot_psx_demo
```

For a repeatable server-side benchmark:

```sh
cargo run --release -p gridwake-server --example godot_psx_demo_server -- --bots 2000 --effects 350 --cover 900 --budget 1400 --max-datagram 4096 --run-ticks 200
```

The HUD shows FPS, frame/process/physics time, draw calls, render objects/primitives, memory, visible entity count, instanced bucket counts, snapshot sequence, packet backlog, pending fragments, and decoded op count. The Godot client also prints one `perf ...` line per second by default; headless runs are useful for script/protocol validation, but normal-window runs are the meaningful render benchmark. The server logs AOI candidates, selected updates, LOD mix, deferred updates, bytes, message counts, timing buckets, datagrams, bytes sent, and fragments sent.
