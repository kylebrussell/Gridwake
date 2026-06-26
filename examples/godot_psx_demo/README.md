# Gridwake Godot PSX Demo

This is a grey-box++ Godot 4.7 client for exercising Gridwake as an engine-neutral server runtime. The Godot side is intentionally thin: it sends player input over UDP, decodes Gridwake snapshot deltas, and renders server-selected AOI state as low-fidelity PS1-style primitives.

## Run

Start the Rust demo server:

```sh
cargo run -p gridwake-server --example godot_psx_demo_server -- --bots 2000 --effects 350
```

Open `examples/godot_psx_demo/project.godot` in Godot 4.7 and run the project.

Keyboard controls:

- `W` / `S` or Up / Down: move forward/back
- `A` / `D` or Left / Right: turn
- `Q` / `E`: strafe

For a lighter local smoke:

```sh
cargo run -p gridwake-server --example godot_psx_demo_server -- --bots 200 --effects 40 --budget 4096
godot --path examples/godot_psx_demo
```

The HUD shows visible entity count, snapshot sequence, packet count, and decoded op count. The server logs AOI candidates, selected updates, LOD mix, deferred updates, bytes, and message counts.
