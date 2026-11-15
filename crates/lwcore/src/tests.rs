//! Portable scenario testkit.
//!
//! These are not `#[test]` functions: the same code must run under
//! `cargo test` *and* compiled to `wasm32-unknown-unknown` by the `wasmtest`
//! harness (see `tools/wasm-test.cjs`). Every scenario is a plain function
//! returning `Result<(), String>`; [`run_tests`] runs them all and returns
//! the list of failure strings.

use crate::game::{Command, Game, GameParams};
use crate::map::Map;
use crate::proto::{Move, NetMessage, TeamSetup};
use crate::rng::Rng;

/// The lockstep input delay used by the netcode, in ticks. Peers send
/// inputs for `tick + INPUT_DELAY_TICKS` so both sides have them in hand
/// before the tick executes.
pub const INPUT_DELAY_TICKS: u64 = 8;

/// Names of all scenarios, in run order.
pub const SCENARIOS: &[&str] = &[
    "match_invariants",
    "cursor_wall_clamp",
    "cursor_slides_along_walls",
    "army_converges",
    "deterministic_multi_team",
    "lockstep_loopback_through_protocol",
    "front_line_battles",
];

fn check(cond: bool, msg: impl Into<String>) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(msg.into())
    }
}

/// Run every scenario; returns one entry per failure:
/// `"scenario_name: what went wrong"`.
pub fn run_tests() -> Vec<String> {
    let mut failures = Vec::new();
    for name in SCENARIOS {
        let result = match *name {
            "match_invariants" => match_invariants(),
            "cursor_wall_clamp" => cursor_wall_clamp(),
            "cursor_slides_along_walls" => cursor_slides_along_walls(),
            "army_converges" => army_converges(),
            "deterministic_multi_team" => deterministic_multi_team(),
            "lockstep_loopback_through_protocol" => lockstep_loopback_through_protocol(),
            "front_line_battles" => front_line_battles(),
            other => Err(format!("unknown scenario: {other}")),
        };
        if let Err(msg) = result {
            failures.push(format!("{name}: {msg}"));
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// A two-team match with cursor wiggling: fighter-count, wall and
/// double-occupancy invariants must hold on every tick.
fn match_invariants() -> Result<(), String> {
    let (map, starts) = Map::islands(42);
    let mut g = Game::new(map, GameParams::default(), 42);
    let t0 = g.add_team(starts[0]);
    let t1 = g.add_team(starts[1]);
    let n0 = g.spawn_army(t0, 80);
    let n1 = g.spawn_army(t1, 80);
    check(n0 == 80 && n1 == 80, "armies did not fully spawn")?;
    check(g.total_fighters() == 160, "total fighters wrong at start")?;

    let mut rng = Rng::new(1234);
    for tick in 0..300u64 {
        if tick % 3 == 0 {
            g.command(Command::SetCursor {
                team: t0,
                x: rng.below(120) as u32 + 4,
                y: rng.below(88) as u32 + 4,
            });
        }
        if tick % 2 == 0 {
            let dx = rng.below(3) as i32 - 1;
            let dy = rng.below(3) as i32 - 1;
            g.command(Command::MoveCursor {
                team: t1,
                dx: dx as i8,
                dy: dy as i8,
            });
        }
        g.tick();

        if g.total_fighters() != 160 {
            return Err(format!("fighter count changed at tick {tick}"));
        }
        let mut occupied = std::collections::HashSet::new();
        for f in g.fighters() {
            if g.map.is_wall(f.x, f.y) {
                return Err(format!("fighter {} inside wall at tick {tick}", f.id));
            }
            if !occupied.insert((f.x, f.y)) {
                return Err(format!("two fighters on one cell at tick {tick}"));
            }
        }
    }
    Ok(())
}

/// A cursor set into a wall or off the map always lands on a floor cell.
fn cursor_wall_clamp() -> Result<(), String> {
    let mut g = Game::new(Map::arena(), GameParams::default(), 1);
    let t0 = g.add_team((96, 72));

    g.command(Command::SetCursor { team: t0, x: 0, y: 0 });
    let (x, y) = g.cursor(t0);
    check(!g.map.is_wall(x, y), "cursor landed on a wall at (0,0)")?;
    check(
        (x, y) == (1, 1),
        format!("expected nearest floor (1,1), got ({x},{y})"),
    )?;

    g.command(Command::SetCursor { team: t0, x: 95, y: 71 });
    check(g.cursor(t0) == (95, 71), "free-cell SetCursor did not stick")?;

    g.command(Command::SetCursor { team: t0, x: 500, y: 0 });
    let (x, y) = g.cursor(t0);
    check(
        !g.map.is_wall(x, y),
        "cursor landed on a wall off the right edge",
    )?;

    for y in 0..g.map.height {
        g.command(Command::SetCursor { team: t0, x: 0, y });
        let (cx, cy) = g.cursor(t0);
        check(
            !g.map.is_wall(cx, cy),
            format!("wall cursor after SetCursor row {y}"),
        )?;
    }
    Ok(())
}

/// Keyboard cursor movement never crosses a wall and slides along corners.
fn cursor_slides_along_walls() -> Result<(), String> {
    let (map, _) = Map::corridors();
    let mut g = Game::new(map, GameParams::default(), 1);
    let t0 = g.add_team((10, 28));

    // Walk left into the map border: must stop on the last floor cell,
    // never on a wall, never out of bounds. (MoveCursor is rate-limited,
    // so each step needs a tick.)
    for _ in 0..20 {
        g.command(Command::MoveCursor { team: t0, dx: -1, dy: 0 });
        g.tick();
        let (x, y) = g.cursor(t0);
        check(
            g.map.in_bounds(x as i64, y as i64) && !g.map.is_wall(x, y),
            format!("cursor entered wall/out of bounds at ({x},{y})"),
        )?;
    }
    check(g.cursor(t0).0 == 4, "cursor did not stop at the wall")?;

    // Diagonal into a wall corner: the pure-vertical fallback must kick in.
    g.command(Command::MoveCursor {
        team: t0,
        dx: -1,
        dy: -1,
    });
    g.tick();
    check(
        g.cursor(t0) == (4, 27),
        format!("no slide along wall: {:?}", g.cursor(t0)),
    )?;

    // Repeated random steps must never break the invariant either.
    let mut rng = Rng::new(99);
    for _ in 0..200 {
        let dx = rng.below(3) as i32 - 1;
        let dy = rng.below(3) as i32 - 1;
        g.command(Command::MoveCursor {
            team: t0,
            dx: dx as i8,
            dy: dy as i8,
        });
        g.tick();
        let (x, y) = g.cursor(t0);
        check(
            g.map.in_bounds(x as i64, y as i64) && !g.map.is_wall(x, y),
            format!("random walk entered wall at ({x},{y})"),
        )?;
    }
    Ok(())
}

/// The signature Liquid War behaviour: a packed army left behind by its
/// cursor flows like a liquid and catches up with it. Without the lateral
/// equal-gradient flow in the movement rule the swarm deadlocks instead.
fn army_converges() -> Result<(), String> {
    let mut g = Game::new(Map::arena(), GameParams::default(), 3);
    let t0 = g.add_team((96, 72));
    let spawned = g.spawn_army(t0, 300);
    check(spawned == 300, "army did not fully spawn")?;

    g.command(Command::SetCursor { team: t0, x: 10, y: 10 });
    let gmax = g
        .fighters()
        .map(|f| g.gradient(t0, f.x, f.y))
        .max()
        .unwrap_or(0);
    check(
        gmax > 20,
        format!("test setup wrong: army already close (gmax={gmax})"),
    )?;

    for _ in 0..500 {
        g.tick();
    }
    let gmax = g
        .fighters()
        .map(|f| g.gradient(t0, f.x, f.y))
        .max()
        .unwrap_or(0);
    check(
        gmax <= 12,
        format!("army did not converge like a liquid (gmax={gmax}, want <= 12)"),
    )?;
    Ok(())
}

/// Three teams, identical command streams: bit-identical states, tick by
/// tick. A different seed must produce a different match.
fn deterministic_multi_team() -> Result<(), String> {
    fn run(seed: u64, cmd_seed: u64) -> Vec<u64> {
        let (map, starts) = Map::islands(42);
        let mut g = Game::new(map, GameParams::default(), seed);
        let teams: Vec<u8> = (0..3).map(|i| g.add_team(starts[i])).collect();
        for &t in &teams {
            g.spawn_army(t, 60);
        }
        let mut rng = Rng::new(cmd_seed);
        let mut hashes = Vec::new();
        for tick in 0..300u64 {
            let team = (tick % 3) as u8;
            if tick % 4 == 0 {
                g.command(Command::SetCursor {
                    team,
                    x: rng.below(120) as u32 + 4,
                    y: rng.below(88) as u32 + 4,
                });
            } else {
                let dx = rng.below(3) as i32 - 1;
                let dy = rng.below(3) as i32 - 1;
                g.command(Command::MoveCursor {
                    team,
                    dx: dx as i8,
                    dy: dy as i8,
                });
            }
            g.tick();
            if tick == 49 || tick == 149 || tick == 299 {
                hashes.push(g.state_hash());
            }
        }
        hashes
    }

    let a = run(1000, 55);
    let b = run(1000, 55);
    check(a == b, "same seeds gave different matches")?;
    let c = run(2000, 55);
    check(a != c, "different seeds gave identical matches")?;
    let d = run(1000, 56);
    check(a != d, "different command streams gave identical matches")?;
    Ok(())
}

/// Full lockstep round: two independent game instances exchange serialized
/// [`crate::proto`] inputs with an input delay and verify each other's
/// hashes, exactly like two peers over the network. The loopback channel is
/// just a byte pipe; both games must stay bit-identical.
fn lockstep_loopback_through_protocol() -> Result<(), String> {
    let (map, starts) = Map::islands(77);
    let teams_setup = vec![
        TeamSetup {
            team: 0,
            cursor: starts[0],
            army_size: 60,
        },
        TeamSetup {
            team: 1,
            cursor: starts[1],
            army_size: 60,
        },
    ];

    let mut host = Game::new(map.clone(), GameParams::default(), 77);
    let mut join = Game::new(map, GameParams::default(), 77);
    for ts in &teams_setup {
        host.add_team(ts.cursor);
        join.add_team(ts.cursor);
    }
    for ts in &teams_setup {
        host.spawn_army(ts.team, ts.army_size as usize);
        join.spawn_army(ts.team, ts.army_size as usize);
    }

    // Two loopback pipes of raw frames: host->join and join->host.
    let mut wire_hj: Vec<Vec<u8>> = Vec::new();
    let mut wire_jh: Vec<Vec<u8>> = Vec::new();
    let mut host_pending: Vec<(u64, Command)> = Vec::new();
    let mut join_pending: Vec<(u64, Command)> = Vec::new();
    let mut host_hashes: Vec<u64> = Vec::new();
    let mut join_hashes: Vec<u64> = Vec::new();
    let mut rh = Rng::new(1001);
    let mut rj = Rng::new(1002);

    const STEPS: u64 = 200;
    for step in 0..STEPS {
        // 1) Each side produces one input for the future tick: applied
        //    locally AND sent over the wire.
        let target = step + INPUT_DELAY_TICKS;
        let host_cmd = random_command(&mut rh, 0);
        let join_cmd = random_command(&mut rj, 1);
        host_pending.push((target, host_cmd));
        join_pending.push((target, join_cmd));
        wire_hj.push(
            NetMessage::Input {
                tick: target,
                team: 0,
                mv: Move::from_command(host_cmd),
            }
            .to_bytes(),
        );
        wire_jh.push(
            NetMessage::Input {
                tick: target,
                team: 1,
                mv: Move::from_command(join_cmd),
            }
            .to_bytes(),
        );

        // 2) Delivery: each side drains its inbox (frames from last step):
        //    inputs get queued, hash checks get verified against the local
        //    history for that tick.
        deliver(&mut wire_jh, &mut host_pending, &host_hashes)?;
        deliver(&mut wire_hj, &mut join_pending, &join_hashes)?;

        // 3) Both sides simulate the step.
        for side in 0..2 {
            let (game, pending, hashes) = if side == 0 {
                (&mut host, &mut host_pending, &mut host_hashes)
            } else {
                (&mut join, &mut join_pending, &mut join_hashes)
            };
            let mut i = 0;
            while i < pending.len() {
                if pending[i].0 == step {
                    let (_, c) = pending.remove(i);
                    game.command(c);
                } else {
                    i += 1;
                }
            }
            game.tick();
            hashes.push(game.state_hash());
        }

        // 4) Hash-check exchange for this step (delivered next step).
        let hh = *host_hashes.last().unwrap();
        let jh = *join_hashes.last().unwrap();
        wire_hj.push(NetMessage::HashCheck { tick: step, hash: hh }.to_bytes());
        wire_jh.push(NetMessage::HashCheck { tick: step, hash: jh }.to_bytes());

        // 5) The two simulations must agree after every single step.
        check(
            hh == jh,
            format!("desync at tick {step}: host={hh} join={jh}"),
        )?;
    }

    check(
        host.state_hash() == join.state_hash(),
        "final states differ after lockstep",
    )?;
    check(
        !host_hashes.is_empty() && host_hashes[0] != host_hashes[STEPS as usize - 1],
        "match did not evolve",
    )?;
    Ok(())
}

/// Parse every frame in `inbox`: inputs are queued into `pending`, hash
/// checks are compared against `hashes` (indexed by tick).
fn deliver(
    inbox: &mut Vec<Vec<u8>>,
    pending: &mut Vec<(u64, Command)>,
    hashes: &[u64],
) -> Result<(), String> {
    for frame in inbox.drain(..) {
        match NetMessage::from_bytes(&frame) {
            Some(NetMessage::Input { tick, team, mv }) => {
                pending.push((tick, mv.to_command(team)));
            }
            Some(NetMessage::HashCheck { tick, hash }) => {
                if hashes.get(tick as usize).copied() != Some(hash) {
                    return Err(format!("peer hash mismatch at tick {tick}"));
                }
            }
            other => return Err(format!("unexpected message on the wire: {other:?}")),
        }
    }
    Ok(())
}

fn random_command(rng: &mut Rng, team: u8) -> Command {
    if rng.below(4) == 0 {
        Command::SetCursor {
            team,
            x: rng.below(120) as u32 + 4,
            y: rng.below(88) as u32 + 4,
        }
    } else {
        let dx = rng.below(3) as i32 - 1;
        let dy = rng.below(3) as i32 - 1;
        Command::MoveCursor {
            team,
            dx: dx as i8,
            dy: dy as i8,
        }
    }
}

/// Two armies whose cursors meet in the middle: the front line must clash,
/// fighters get eaten and converted, totals stay constant, and one team
/// must come out ahead or the fight is still on — but something must have
/// happened to both counts.
fn front_line_battles() -> Result<(), String> {
    let mut g = Game::new(Map::arena(), GameParams::default(), 5);
    let t0 = g.add_team((60, 72));
    let t1 = g.add_team((132, 72));
    check(g.spawn_army(t0, 150) == 150, "team0 army short")?;
    check(g.spawn_army(t1, 150) == 150, "team1 army short")?;

    // Both cursors charge the middle: the armies flow into each other.
    g.command(Command::SetCursor { team: t0, x: 96, y: 72 });
    g.command(Command::SetCursor { team: t1, x: 96, y: 72 });

    let mut history: Vec<(usize, usize)> = Vec::new();
    for tick in 0..600 {
        g.tick();
        if g.total_fighters() != 300 {
            return Err(format!("total fighters changed at tick {tick}"));
        }
        if tick % 100 == 99 {
            let b = g.scoreboard();
            history.push((b.counts[0], b.counts[1]));
        }
    }

    let b = g.scoreboard();
    check(
        b.counts[0] + b.counts[1] == 300,
        "fighters were created or destroyed",
    )?;
    let (c0, c1) = (b.counts[0], b.counts[1]);
    check(c0 != 150 || c1 != 150, "armies never met: no battle happened")?;
    // A team that is behind must have lost fighters to the other.
    if c0 != c1 {
        let (loser, winner) = if c0 < c1 { (c0, c1) } else { (c1, c0) };
        check(
            loser < 150 && winner > 150,
            "eaten fighters vanished into thin air",
        )?;
    }
    // The front line must have moved long before the end: the counts at
    // tick 100 cannot still be the pristine (150, 150).
    check(
        history.first() != Some(&(150, 150)),
        "armies did not clash within the first 100 ticks",
    )?;
    // And the end state must be consistent with the scoreboard's winner.
    match b.winner() {
        Some(w) => check(
            b.counts[w as usize] > 150,
            "winner is not the bigger team",
        )?,
        None => check(c0 == c1, "draw with unequal counts")?,
    }
    Ok(())
}
