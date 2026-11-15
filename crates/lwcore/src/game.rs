//! The deterministic Liquid War simulation kernel (the "ker" of LW6).
//!
//! Rules, faithfully inherited from Liquid War (algorithm by Thomas
//! Colcombet, game by Christian Mauduit):
//!
//! * every player controls a *cursor* on a 2-D battlefield;
//! * each player owns an army of fighters, one per cell;
//! * for every team a *gradient* is computed over the map: the distance in
//!   steps from each floor cell to that team's cursor (shortest path,
//!   BFS through walkable cells, 8 directions);
//! * on each tick, every fighter either **eats** an adjacent enemy (the
//!   eaten fighter joins the eater's team — armies are absorbed, the total
//!   number of fighters never changes) or **moves** like a liquid downhill
//!   toward its cursor;
//! * the game ends when time runs out or only one team still has fighters:
//!   the team with the most fighters wins.
//!
//! Determinism: given the same map, params, seed and stream of [`Command`]s,
//! the simulation is bit-for-bit identical on every platform. This is what
//! makes lockstep netplay possible: peers only exchange cursor commands,
//! never game state.

use crate::hash::fnv1a64;
use crate::map::Map;
use crate::rng::Rng;

/// 8 directions, N, NE, E, SE, S, SW, W, NW.
pub const DIRS: [(i32, i32); 8] = [
    (0, -1),
    (1, -1),
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
];

pub const INF: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameParams {
    /// Total game length in ticks.
    pub duration_ticks: u64,
    /// A cursor may step at most once every this many ticks.
    pub cursor_step_interval: u64,
}

impl Default for GameParams {
    fn default() -> Self {
        // ~2 minutes at a 64 Hz tick rate, cursor free to move every tick.
        GameParams {
            duration_ticks: 64 * 120,
            cursor_step_interval: 1,
        }
    }
}

/// A player action. All netplay traffic is a stream of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Classic Liquid War keyboard control: one step in a direction.
    /// The core rate-limits cursor steps to `cursor_step_interval`.
    /// If the step is blocked by a wall, the cursor slides along the wall
    /// (tries the pure-horizontal then pure-vertical step).
    MoveCursor { team: u8, dx: i8, dy: i8 },
    /// Mouse-style control: the cursor jumps to the nearest floor cell
    /// around the requested point (walls repel it deterministically).
    SetCursor { team: u8, x: u32, y: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scoreboard {
    /// Fighter count per team, by team index.
    pub counts: Vec<usize>,
}

impl Scoreboard {
    /// The winning team index, if a single best team exists.
    pub fn winner(&self) -> Option<u8> {
        let mut best: Option<(usize, usize)> = None;
        for (i, &c) in self.counts.iter().enumerate() {
            match best {
                Some((_, bc)) if c <= bc => {}
                _ => best = Some((i, c)),
            }
        }
        let (best, count) = best?;
        if count == 0 || self.counts.iter().filter(|&&c| c == count).count() > 1 {
            return None; // draw
        }
        Some(best as u8)
    }
}

/// A read-only view of a fighter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FighterView {
    pub id: usize,
    pub x: u32,
    pub y: u32,
    pub team: u8,
}

pub struct Game {
    pub map: Map,
    pub params: GameParams,
    pub seed: u64,
    /// Number of ticks simulated so far.
    pub tick: u64,

    /// Cursor position per team.
    cursors: Vec<(u32, u32)>,
    /// Tick of the last cursor step per team (rate limiting), -1 = never.
    cursor_last_step: Vec<i64>,
    /// Per-team gradient: distance from each cell to that team's cursor.
    gradients: Vec<Vec<u32>>,
    /// Scratch BFS frontier, reused every tick to avoid allocations.
    bfs_queue: Vec<u32>,

    /// Fighter positions by fighter id (fighters never die, they convert).
    positions: Vec<(u32, u32)>,
    /// Fighter team by fighter id.
    teams: Vec<u8>,
    /// Fighter id occupying a cell, or -1 if the cell is free.
    cell_fighter: Vec<i32>,

    rng: Rng,
}

impl Game {
    pub fn new(map: Map, params: GameParams, seed: u64) -> Game {
        let n = (map.width * map.height) as usize;
        Game {
            map,
            params,
            seed,
            tick: 0,
            cursors: Vec::new(),
            cursor_last_step: Vec::new(),
            gradients: Vec::new(),
            bfs_queue: Vec::with_capacity(n),
            positions: Vec::new(),
            teams: Vec::new(),
            cell_fighter: vec![-1; n],
            rng: Rng::new(seed),
        }
    }

    pub fn num_teams(&self) -> usize {
        self.cursors.len()
    }

    /// Register a team with a cursor; the cursor is clamped to the nearest
    /// floor cell. Returns the new team index.
    pub fn add_team(&mut self, cursor: (u32, u32)) -> u8 {
        let team = self.cursors.len() as u8;
        let clamped = self.nearest_floor(cursor.0 as i64, cursor.1 as i64);
        self.cursors.push(clamped);
        self.cursor_last_step.push(-1);
        self.gradients
            .push(vec![INF; (self.map.width * self.map.height) as usize]);
        team
    }

    /// Spawn `count` fighters for `team`, flooding outward from the team
    /// cursor in BFS order (deterministic). Returns how many were actually
    /// placed (may be fewer on a tiny map).
    pub fn spawn_army(&mut self, team: u8, count: usize) -> usize {
        let team_idx = team as usize;
        assert!(team_idx < self.cursors.len(), "team not registered");
        let (cx, cy) = self.cursors[team_idx];
        let width = self.map.width as usize;
        let n = (self.map.width * self.map.height) as usize;

        let mut queue: Vec<u32> = Vec::with_capacity(n);
        let mut visited = vec![false; n];
        let start = self.map.index(cx, cy);
        queue.push(start as u32);
        visited[start] = true;
        let mut spawned = 0usize;

        let mut head = 0usize;
        while head < queue.len() && spawned < count {
            let cur = queue[head] as usize;
            head += 1;
            if self.cell_fighter[cur] < 0 {
                let (x, y) = ((cur % width) as u32, (cur / width) as u32);
                self.add_fighter(x, y, team);
                spawned += 1;
            }
            for (dx, dy) in DIRS {
                if let Some((nx, ny)) = self.walkable_neighbor(
                    (cur % width) as u32,
                    (cur / width) as u32,
                    dx,
                    dy,
                ) {
                    let ni = self.map.index(nx, ny);
                    if !visited[ni] {
                        visited[ni] = true;
                        queue.push(ni as u32);
                    }
                }
            }
        }
        spawned
    }

    fn add_fighter(&mut self, x: u32, y: u32, team: u8) -> usize {
        let id = self.positions.len();
        let idx = self.map.index(x, y);
        self.positions.push((x, y));
        self.teams.push(team);
        self.cell_fighter[idx] = id as i32;
        id
    }

    /// Apply a player command for the current tick.
    pub fn command(&mut self, cmd: Command) {
        let team = match cmd {
            Command::MoveCursor { team, .. } => team,
            Command::SetCursor { team, .. } => team,
        } as usize;
        if team >= self.cursors.len() {
            return; // unknown team: ignore
        }
        match cmd {
            Command::SetCursor { x, y, .. } => {
                self.cursors[team] = self.nearest_floor(x as i64, y as i64);
            }
            Command::MoveCursor { dx, dy, .. } => {
                let interval = self.params.cursor_step_interval as i64;
                if self.tick as i64 - self.cursor_last_step[team] < interval {
                    return; // too soon
                }
                let (cx, cy) = self.cursors[team];
                // Try the full step; if blocked by a wall, slide along it.
                for (tx, ty) in [
                    (cx as i64 + dx as i64, cy as i64 + dy as i64),
                    (cx as i64 + dx as i64, cy as i64),
                    (cx as i64, cy as i64 + dy as i64),
                ] {
                    if self.map.in_bounds(tx, ty) && !self.map.is_wall(tx as u32, ty as u32) {
                        self.cursors[team] = (tx as u32, ty as u32);
                        self.cursor_last_step[team] = self.tick as i64;
                        return;
                    }
                }
            }
        }
    }

    /// Simulate one tick. All commands intended for this tick must already
    /// have been applied via [`Game::command`].
    pub fn tick(&mut self) {
        self.tick += 1;
        for t in 0..self.cursors.len() {
            self.recompute_gradient(t);
        }
        let n_fighters = self.positions.len();
        for id in 0..n_fighters {
            self.step_fighter(id);
        }
    }

    /// Recompute team `t`'s gradient: exact BFS distance from every floor
    /// cell to the team cursor. This is the Liquid War "shortest path"
    /// potential field; fighters simply walk downhill on it.
    fn recompute_gradient(&mut self, t: usize) {
        let (gradients, map, cursors, queue) = (
            &mut self.gradients,
            &self.map,
            &self.cursors,
            &mut self.bfs_queue,
        );
        let g = &mut gradients[t];
        for v in g.iter_mut() {
            *v = INF;
        }
        queue.clear();
        let (cx, cy) = cursors[t];
        let start = map.index(cx, cy);
        g[start] = 0;
        queue.push(start as u32);

        let width = map.width as usize;
        let mut head = 0usize;
        while head < queue.len() {
            let cur = queue[head] as usize;
            head += 1;
            let d = g[cur];
            let (x, y) = ((cur % width) as i64, (cur / width) as i64);
            for (dx0, dy0) in DIRS {
                let (dx, dy) = (dx0 as i64, dy0 as i64);
                let (nx, ny) = (x + dx, y + dy);
                if !map.in_bounds(nx, ny) || map.is_wall(nx as u32, ny as u32) {
                    continue;
                }
                // No cutting diagonally between two walls.
                if dx != 0 && dy != 0 && map.is_wall_clamped(x + dx, y) && map.is_wall_clamped(x, y + dy)
                {
                    continue;
                }
                let ni = map.index(nx as u32, ny as u32);
                if g[ni] == INF {
                    g[ni] = d + 1;
                    queue.push(ni as u32);
                }
            }
        }
    }

    /// One fighter acts: eat an adjacent enemy, else move like a liquid.
    fn step_fighter(&mut self, id: usize) {
        let team = self.teams[id] as usize;
        let (x, y) = self.positions[id];

        // 1) Eat: if an enemy is adjacent, absorb one of them.
        let mut enemies: [usize; 8] = [0; 8];
        let mut enemy_count = 0usize;
        for (dx, dy) in DIRS {
            if let Some(f) = self.fighter_toward(x, y, dx, dy) {
                if self.teams[f] as usize != team {
                    enemies[enemy_count] = f;
                    enemy_count += 1;
                }
            }
        }
        if enemy_count > 0 {
            let victim = enemies[self.rng.below(enemy_count)];
            self.teams[victim] = team as u8;
            return;
        }

        // 2) Move downhill on this team's gradient — like a liquid: if the
        //    fighter cannot descend (blocked by walls or packed fighters),
        //    it flows sideways into an equal-gradient cell. This shuffle is
        //    what un-jams the swarm; without it the army deadlocks. It is
        //    the Rust analogue of the original game's imperfect gradient
        //    ("Liquid War does this with a 10% error tolerance").
        let cur_d = self.gradients[team][self.map.index(x, y)];
        let mut best = INF;
        let mut mins: [(u32, u32); 8] = [(0, 0); 8];
        let mut min_count = 0usize;
        let mut equal: [(u32, u32); 8] = [(0, 0); 8];
        let mut equal_count = 0usize;
        for (dx, dy) in DIRS {
            if let Some((nx, ny)) = self.free_neighbor(x, y, dx, dy) {
                let d = self.gradients[team][self.map.index(nx, ny)];
                if d < best {
                    best = d;
                    min_count = 1;
                    mins[0] = (nx, ny);
                } else if d == best && min_count < 8 {
                    mins[min_count] = (nx, ny);
                    min_count += 1;
                }
                if d == cur_d && equal_count < 8 {
                    equal[equal_count] = (nx, ny);
                    equal_count += 1;
                }
            }
        }
        if min_count > 0 && best < cur_d {
            let (nx, ny) = mins[self.rng.below(min_count)];
            self.move_fighter(id, nx, ny);
        } else if equal_count > 0 && self.rng.below(4) > 0 {
            // Flow sideways most of the time: liquid-like relaxation.
            let (nx, ny) = equal[self.rng.below(equal_count)];
            self.move_fighter(id, nx, ny);
        }
    }

    fn move_fighter(&mut self, id: usize, x: u32, y: u32) {
        let (ox, oy) = self.positions[id];
        let old = self.map.index(ox, oy);
        let new = self.map.index(x, y);
        self.cell_fighter[old] = -1;
        self.cell_fighter[new] = id as i32;
        self.positions[id] = (x, y);
    }

    /// The fighter occupying a walkable neighbor cell, if any.
    fn fighter_toward(&self, x: u32, y: u32, dx: i32, dy: i32) -> Option<usize> {
        let (nx, ny) = (x as i64 + dx as i64, y as i64 + dy as i64);
        if !self.map.in_bounds(nx, ny) || self.map.is_wall(nx as u32, ny as u32) {
            return None;
        }
        // Fighters also cannot attack through a wall corner.
        if dx != 0
            && dy != 0
            && self.map.is_wall_clamped(x as i64 + dx as i64, y as i64)
            && self.map.is_wall_clamped(x as i64, y as i64 + dy as i64)
        {
            return None;
        }
        let f = self.cell_fighter[self.map.index(nx as u32, ny as u32)];
        if f < 0 {
            None
        } else {
            Some(f as usize)
        }
    }

    /// A neighboring floor cell that is free of fighters and reachable
    /// without cutting through a wall corner.
    fn free_neighbor(&self, x: u32, y: u32, dx: i32, dy: i32) -> Option<(u32, u32)> {
        let (nx, ny) = (x as i64 + dx as i64, y as i64 + dy as i64);
        if !self.map.in_bounds(nx, ny) || self.map.is_wall(nx as u32, ny as u32) {
            return None;
        }
        if dx != 0
            && dy != 0
            && self.map.is_wall_clamped(x as i64 + dx as i64, y as i64)
            && self.map.is_wall_clamped(x as i64, y as i64 + dy as i64)
        {
            return None;
        }
        if self.cell_fighter[self.map.index(nx as u32, ny as u32)] >= 0 {
            return None;
        }
        Some((nx as u32, ny as u32))
    }

    /// A neighboring floor cell (regardless of fighters), for BFS/spawning.
    fn walkable_neighbor(&self, x: u32, y: u32, dx: i32, dy: i32) -> Option<(u32, u32)> {
        let (nx, ny) = (x as i64 + dx as i64, y as i64 + dy as i64);
        if !self.map.in_bounds(nx, ny) || self.map.is_wall(nx as u32, ny as u32) {
            return None;
        }
        if dx != 0
            && dy != 0
            && self.map.is_wall_clamped(x as i64 + dx as i64, y as i64)
            && self.map.is_wall_clamped(x as i64, y as i64 + dy as i64)
        {
            return None;
        }
        Some((nx as u32, ny as u32))
    }

    /// Nearest floor cell to a point: itself if walkable, else the closest
    /// floor cell by Chebyshev distance (row-major tiebreak, deterministic).
    pub fn nearest_floor(&self, x: i64, y: i64) -> (u32, u32) {
        let x = x.clamp(0, self.map.width as i64 - 1);
        let y = y.clamp(0, self.map.height as i64 - 1);
        if !self.map.is_wall(x as u32, y as u32) {
            return (x as u32, y as u32);
        }
        let mut best = (u32::MAX, (0u32, 0u32));
        for cy in 0..self.map.height {
            for cx in 0..self.map.width {
                if !self.map.is_wall(cx, cy) {
                    let d = (cx as i64 - x).abs().max((cy as i64 - y).abs()) as u32;
                    if d < best.0 {
                        best = (d, (cx, cy));
                    }
                }
            }
        }
        best.1
    }

    pub fn cursor(&self, team: u8) -> (u32, u32) {
        self.cursors[team as usize]
    }

    /// Fighter count per team.
    pub fn fighter_count(&self, team: u8) -> usize {
        self.teams.iter().filter(|&&t| t == team).count()
    }

    pub fn total_fighters(&self) -> usize {
        self.positions.len()
    }

    /// Iterate all fighters (stable id order).
    pub fn fighters(&self) -> impl Iterator<Item = FighterView> + '_ {
        self.positions
            .iter()
            .enumerate()
            .map(|(id, &(x, y))| FighterView {
                id,
                x,
                y,
                team: self.teams[id],
            })
    }

    pub fn scoreboard(&self) -> Scoreboard {
        let mut counts = vec![0usize; self.cursors.len()];
        for &t in &self.teams {
            counts[t as usize] += 1;
        }
        Scoreboard { counts }
    }

    /// `Some(scoreboard)` when the game is over: time out, or at most one
    /// team still has fighters (extermination).
    pub fn game_over(&self) -> Option<Scoreboard> {
        let board = self.scoreboard();
        let alive = board.counts.iter().filter(|&&c| c > 0).count();
        if self.tick >= self.params.duration_ticks || alive <= 1 {
            Some(board)
        } else {
            None
        }
    }

    /// FNV-1a fingerprint of the full state: used by tests and by netplay
    /// to detect desyncs between peers.
    pub fn state_hash(&self) -> u64 {
        let mut buf: Vec<u8> = Vec::with_capacity(64);
        buf.extend_from_slice(&self.tick.to_le_bytes());
        for &(x, y) in &self.cursors {
            buf.extend_from_slice(&x.to_le_bytes());
            buf.extend_from_slice(&y.to_le_bytes());
        }
        for id in 0..self.positions.len() {
            let (x, y) = self.positions[id];
            buf.push(self.teams[id]);
            buf.extend_from_slice(&x.to_le_bytes());
            buf.extend_from_slice(&y.to_le_bytes());
        }
        fnv1a64(&buf)
    }

    /// The gradient value of a cell for a team (distance to the team's
    /// cursor; `INF` if unreachable). Handy for debug views.
    pub fn gradient(&self, team: u8, x: u32, y: u32) -> u32 {
        self.gradients[team as usize][self.map.index(x, y)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maze() -> Map {
        let (m, _) = Map::from_ascii(&[
            "#######",
            "#.....#",
            "###.###",
            "#.....#",
            "#######",
        ]);
        m
    }

    #[test]
    fn gradient_follows_detour_not_chebyshev() {
        let mut g = Game::new(maze(), GameParams::default(), 1);
        let t0 = g.add_team((5, 1));
        g.tick();
        // (1,3) is 4 steps from the cursor (5,1) through the gap at (3,2),
        // while the Chebyshev distance is only 2.
        assert_eq!(g.gradient(t0, 1, 3), 4);
        assert_eq!(g.gradient(t0, 3, 2), 2);
    }

    #[test]
    fn gradient_chebyshev_on_open_map() {
        let mut g = Game::new(Map::arena(), GameParams::default(), 1);
        let t0 = g.add_team((96, 72));
        g.tick();
        assert_eq!(g.gradient(t0, 96, 72), 0);
        assert_eq!(g.gradient(t0, 2, 2), 94);
        assert_eq!(g.gradient(t0, 190, 142), 94);
    }

    #[test]
    fn gradient_unreachable_is_inf() {
        let (mut m, _) = Map::from_ascii(&[
            "#######",
            "#.....#",
            "#######",
            "#.....#",
            "#######",
        ]);
        // two rooms, no connection
        for y in 0..5 {
            m.set_wall(3, y, true);
        }
        let mut g = Game::new(m, GameParams::default(), 1);
        let t0 = g.add_team((1, 1));
        g.tick();
        assert_eq!(g.gradient(t0, 5, 3), INF);
        assert_eq!(g.gradient(t0, 1, 1), 0);
    }

    #[test]
    fn fighters_move_toward_cursor() {
        let mut g = Game::new(Map::arena(), GameParams::default(), 7);
        let t0 = g.add_team((10, 10));
        g.spawn_army(t0, 1);
        g.command(Command::SetCursor { team: t0, x: 150, y: 10 });
        g.tick();
        let view = g.fighters().next().unwrap();
        // Was at (10,10); after one tick it must have taken a step toward
        // (150,10), i.e. its gradient dropped from 140 to at most 139.
        assert_ne!((view.x, view.y), (10u32, 10u32));
        let d = g.gradient(t0, view.x, view.y);
        assert!(d < 140, "gradient did not decrease: {d}");
    }

    #[test]
    fn army_flows_to_cursor() {
        let mut g = Game::new(Map::arena(), GameParams::default(), 3);
        let t0 = g.add_team((10, 10));
        g.spawn_army(t0, 50);
        g.command(Command::SetCursor { team: t0, x: 150, y: 100 });
        for _ in 0..250 {
            g.tick();
        }
        // After 250 ticks the army must have caught up with the cursor and
        // packed around it: 50 fighters fit within Chebyshev radius ~5.
        for f in g.fighters() {
            assert!(
                g.gradient(t0, f.x, f.y) <= 5,
                "fighter {} lagging (gradient {})",
                f.id,
                g.gradient(t0, f.x, f.y)
            );
        }
    }

    #[test]
    fn eating_converts_enemy() {
        let mut g = Game::new(Map::arena(), GameParams::default(), 9);
        let t0 = g.add_team((50, 50));
        let t1 = g.add_team((51, 51));
        g.spawn_army(t0, 1);
        g.spawn_army(t1, 1);
        assert_eq!(g.fighter_count(t0), 1);
        assert_eq!(g.fighter_count(t1), 1);
        g.tick();
        // The two fighters are adjacent: one eats the other.
        let board = g.scoreboard();
        assert_eq!(board.counts[t0 as usize] + board.counts[t1 as usize], 2);
        assert!(board.counts[t0 as usize] == 2 || board.counts[t1 as usize] == 2);
    }

    #[test]
    fn extermination_ends_game() {
        let mut g = Game::new(Map::arena(), GameParams::default(), 11);
        let t0 = g.add_team((50, 50));
        let t1 = g.add_team((51, 50));
        g.spawn_army(t0, 1);
        g.spawn_army(t1, 1);
        g.tick();
        assert!(g.game_over().is_some());
        assert_eq!(g.fighter_count(t0) + g.fighter_count(t1), 2);
    }

    #[test]
    fn deterministic_replay() {
        fn run(seed: u64) -> u64 {
            let (map, starts) = Map::islands(42);
            let mut g = Game::new(map.clone(), GameParams::default(), seed);
            let t0 = g.add_team(starts[0]);
            let t1 = g.add_team(starts[1]);
            g.spawn_army(t0, 80);
            g.spawn_army(t1, 80);
            let mut rng = Rng::new(seed.wrapping_add(1));
            let mut last = g.state_hash();
            for i in 0..300 {
                let dx = rng.below(3) as i32 - 1;
                let dy = rng.below(3) as i32 - 1;
                g.command(Command::MoveCursor { team: 0, dx: dx as i8, dy: dy as i8 });
                if i % 5 == 0 {
                    g.command(Command::SetCursor {
                        team: 1,
                        x: rng.below(120) as u32 + 4,
                        y: rng.below(88) as u32 + 4,
                    });
                }
                g.tick();
                last = g.state_hash();
            }
            last
        }
        assert_eq!(run(1234), run(1234));
        assert_ne!(run(1234), run(1235));
    }

    #[test]
    fn spawn_army_is_bounded_and_non_overlapping() {
        let (map, starts) = Map::islands(5);
        let mut g = Game::new(map, GameParams::default(), 1);
        let t0 = g.add_team(starts[0]);
        let spawned = g.spawn_army(t0, 100_000); // way more than the map holds
        assert!(spawned < 100_000);
        assert_eq!(g.total_fighters(), spawned);
        let mut seen = std::collections::HashSet::new();
        for f in g.fighters() {
            assert!(!g.map.is_wall(f.x, f.y));
            assert!(seen.insert((f.x, f.y)), "two fighters on one cell");
        }
    }
}
