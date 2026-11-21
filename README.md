# liquidwar-rs

A from-scratch **Rust port of [Liquid War](https://ufoot.org/liquidwar/)** —
the classic "liquid" real-time strategy game where you do not control your
troops directly, only a **cursor**, and your army flows like a fluid toward
it while eating everything enemy-colored it touches on the way.

The port takes **Liquid War 6** (GNU) as its architectural reference and
runs on [Bevy](https://bevyengine.org/). The core rules and the famous
gradient pathfinding algorithm are reimplemented from the public
descriptions of the original:

- Christian Mauduit's algorithm notes for Liquid War 5:
  <https://ufoot.org/liquidwar/v5/techinfo/algorithm/en/>
- The GNU Liquid War 6 manual, core algorithm chapter:
  <https://www.gnu.org/software/liquidwar6/manual/html_node/Core-algorithm.html>

Licensed **GPL-3.0-or-later**, like the originals. All credit for the game
design and the core algorithm goes to **Christian Mauduit (ufo)**
and **Thomas Colcombet**, the authors of Liquid War.
Upstream projects: [liquidwar5](https://ufoot.org/liquidwar/v5/),
[liquidwar6](https://www.gnu.org/software/liquidwar6/).

## Rules in 30 seconds

- 2 to 6 teams, each with an army of fighters on a 2-D battlefield.
- You only move your **cursor** (mouse or keys).
- Every tick, each team's **gradient** — the shortest-path distance from
  every floor cell to that team's cursor — is recomputed, and every
  fighter either **moves downhill** toward its cursor or **eats** an
  adjacent enemy fighter (the eaten fighter *joins* the eater's color).
- The total number of fighters never changes; armies are absorbed.
- The game ends at time-out or extermination: the team with the most
  fighters wins.

## Usage

```sh
# two players on one machine
cargo run --release -p lwbevy

# versus the (simple) bot
cargo run --release -p lwbevy -- --vs-ai

# Online peers must share a strong secret (not sent over the network).
# The connection is authenticated but not encrypted.
LIQUIDWAR_PASSWORD='use-a-long-random-secret' cargo run --release -p lwbevy -- --host 8080
LIQUIDWAR_PASSWORD='use-a-long-random-secret' cargo run --release -p lwbevy -- --join somehost.example:8080

# pick a battlefield
cargo run --release -p lwbevy -- --vs-ai --map corridors
```

Maps: `arena` (default, wide open), `corridors` (rooms + wide corridors),
`islands` (seeded wall blobs — the seed is fixed per map name).

### Controls

| Input | Effect |
| --- | --- |
| Mouse | P1 cursor (red), always follows the mouse |
| Arrows / WASD | P2 cursor (blue), in local two-player mode |
| R | Restart the match |
| Esc | Quit |

## Architecture

```
crates/
  lwcore/     dependency-free, 100% deterministic simulation kernel
  lwbevy/     Bevy 0.16 frontend: rendering, input, bot, TCP netcode
  wasmtest/   scenario suite compiled to wasm32, run under Node
tools/
  wasm-test.cjs   drives wasmtest.wasm and reports results
```

- **`lwcore`** is the "ker" of LW6 in spirit: maps, per-team gradient
  (BFS shortest-path) fields, fighters that eat and flow, a seeded
  xorshift64* RNG and FNV-1a hashing. It has **zero dependencies** and
  builds anywhere, including `wasm32-unknown-unknown`. Given the same map,
  seed and command stream it is **bit-for-bit deterministic** on every
  platform.
- **`lwbevy`** renders one sprite per fighter, converts mouse/key input to
  `lwcore::Command`s, drives a deliberately simple greedy bot
  (`--vs-ai`), and implements **lockstep netplay** over TCP: peers exchange
  only cursor commands (never state), with an 8-tick input delay, mutual
  authentication using `LIQUIDWAR_PASSWORD`, ChaCha20-Poly1305 authenticated
  encryption for every frame, and a per-tick hash check that flags simulation
  desyncs.

### Netcode

Both peers run the identical deterministic simulation. Each tick every
peer sends one input message tagged `tick + 8`; a tick only executes once
the opponent's input for it has arrived (the game simply pauses to wait
— no rubber-banding, no state sync). A `HashCheck` fingerprint travels
after every tick so any divergence is caught within a second.

## Testing

```sh
# Bevy/netplay tests, including authenticated encrypted framing and online restart
cargo test -p lwbevy

# core unit tests (gradient, invariants, determinism, protocol...)
cargo test -p lwcore

# the same scenario suite on a second platform (wasm via Node)
node tools/wasm-test.cjs
```

The scenario suite covers match invariants (no wall clipping, no double
occupancy, conserved fighter counts), cursor clamping/sliding along
walls, liquid convergence of a full army, multi-team determinism, a full
lockstep loopback through the wire protocol, and front-line battles.

Online mode authenticates both peers and encrypts/authenticates every frame
with ChaCha20-Poly1305. Use a long, randomly generated shared secret: captured
handshakes still allow offline guesses against weak secrets. Encryption does
not hide connection metadata such as peer addresses or traffic timing.

## Known limitations (v3)

- No graphical/sound polish: flat colored squares, one wall texture.
- Online restarts are coordinated: pressing R sends a restart request, the
  host pauses both peers, and both reset together once the host commits.
- The bot is deliberately simple, like the original's basic AI.
- No GUI lobby: host/join is command-line only.
- Max ~2 teams in the UI, although the core supports 6.

## License

GPL-3.0-or-later. Liquid War was created by Christian Mauduit with the
core algorithm by Thomas Colcombet; this port honors their request that
derivatives stay free software.
