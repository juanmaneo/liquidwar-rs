//! The Liquid War AI: a deliberately simple greedy bot, like the basic
//! brain of the original game. It walks its cursor toward the nearest enemy
//! fighter, with a dash of randomness so it does not hang on wall corners.
//!
//! It uses lwcore's deterministic RNG only, so a bot-driven match replays
//! identically.

use lwcore::game::{Command, Game};
use lwcore::rng::Rng;

pub struct Bot {
    pub team: u8,
    rng: Rng,
}

impl Bot {
    pub fn new(team: u8, seed: u64) -> Bot {
        Bot {
            team,
            rng: Rng::new(seed),
        }
    }

    /// Decide this tick's cursor move by charging the nearest enemy.
    pub fn step(&mut self, game: &Game) -> Command {
        let (cx, cy) = game.cursor(self.team);
        let mut best: Option<(i64, u32, u32)> = None;
        for f in game.fighters() {
            if f.team == self.team {
                continue;
            }
            let d = (f.x as i64 - cx as i64).abs() + (f.y as i64 - cy as i64).abs();
            match best {
                Some((bd, _, _)) if d >= bd => {}
                _ => best = Some((d, f.x, f.y)),
            }
        }
        let Some((_, ex, ey)) = best else {
            // No enemies left: wander.
            let dx = (self.rng.below(3) as i8) - 1;
            let dy = (self.rng.below(3) as i8) - 1;
            return Command::MoveCursor {
                team: self.team,
                dx,
                dy,
            };
        };

        let mut sx = (ex as i64 - cx as i64).signum() as i8;
        let mut sy = (ey as i64 - cy as i64).signum() as i8;
        // Occasional jitter keeps the cursor from grinding on wall corners.
        if self.rng.below(6) == 0 {
            sx = (self.rng.below(3) as i8) - 1;
            sy = (self.rng.below(3) as i8) - 1;
        }
        Command::MoveCursor {
            team: self.team,
            dx: sx,
            dy: sy,
        }
    }
}
