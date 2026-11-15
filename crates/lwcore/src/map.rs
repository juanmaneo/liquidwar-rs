//! Battle maps for Liquid War.
//!
//! A map is a plain grid of cells: wall or floor. Fighters live on floor
//! cells, walls block fighters and cursors. Maps can be built from ASCII art
//! (which may also carry team start positions), generated procedurally with
//! a seed, or serialized over the wire with [`Map::to_bytes`] /
//! [`Map::from_bytes`].

/// Maximum number of teams/players the game supports (like Liquid War: 2-6).
pub const MAX_TEAMS: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    pub width: u32,
    pub height: u32,
    /// One entry per cell, row-major: `true` = wall.
    walls: Vec<bool>,
}

impl Map {
    pub fn new(width: u32, height: u32) -> Map {
        Map {
            width,
            height,
            walls: vec![false; (width * height) as usize],
        }
    }

    #[inline]
    pub fn in_bounds(&self, x: i64, y: i64) -> bool {
        x >= 0 && y >= 0 && (x as u64) < self.width as u64 && (y as u64) < self.height as u64
    }

    #[inline]
    pub fn index(&self, x: u32, y: u32) -> usize {
        (y * self.width + x) as usize
    }

    #[inline]
    pub fn is_wall(&self, x: u32, y: u32) -> bool {
        self.walls[self.index(x, y)]
    }

    /// Coordinates are clamped into bounds before the lookup.
    #[inline]
    pub fn is_wall_clamped(&self, x: i64, y: i64) -> bool {
        if !self.in_bounds(x, y) {
            return true;
        }
        self.is_wall(x as u32, y as u32)
    }

    #[inline]
    pub fn set_wall(&mut self, x: u32, y: u32, wall: bool) {
        let i = self.index(x, y);
        self.walls[i] = wall;
    }

    pub fn floor_count(&self) -> usize {
        self.walls.iter().filter(|w| !**w).count()
    }

    /// A map that is entirely walkable, surrounded by a one-cell wall border.
    pub fn empty_bordered(width: u32, height: u32) -> Map {
        let mut m = Map::new(width, height);
        if width == 0 || height == 0 {
            return m;
        }
        for x in 0..width {
            m.set_wall(x, 0, true);
            m.set_wall(x, height - 1, true);
        }
        for y in 0..height {
            m.set_wall(0, y, true);
            m.set_wall(width - 1, y, true);
        }
        m
    }

    /// The classic wide-open arena (192x144).
    pub fn arena() -> Map {
        Map::empty_bordered(192, 144)
    }

    /// Classic rooms-and-corridors map (192x144): a 4x3 grid of big rooms
    /// linked by wide crossing corridors. Deterministic (no RNG).
    /// Returns the map plus the default start cursors for up to 4 teams.
    pub fn corridors() -> (Map, Vec<(u32, u32)>) {
        let (w, h) = (192u32, 144u32);
        let mut map = Map::new(w, h);
        // Start all-wall: rooms and corridors below carve the walkable space.
        for y in 0..h {
            for x in 0..w {
                map.set_wall(x, y, true);
            }
        }
        let mut carve = |x0: u32, y0: u32, x1: u32, y1: u32| {
            for y in y0..y1 {
                for x in x0..x1 {
                    map.set_wall(x, y, false);
                }
            }
        };
        // Rooms: 4 columns x 3 rows of 40x40 cells.
        for ry in 0..3 {
            for rx in 0..4 {
                let (x0, y0) = (4 + rx * 48, 4 + ry * 48);
                carve(x0, y0, x0 + 40, y0 + 40);
            }
        }
        // Horizontal corridors through the room band.
        for y0 in [24u32, 72, 120] {
            carve(4, y0, w - 4, y0 + 8);
        }
        // Vertical corridors through the room band.
        for x0 in [24u32, 72, 120, 168] {
            carve(x0, 4, x0 + 8, h - 4);
        }
        let starts = vec![(10, 28), (182, 28), (90, 10), (90, 134)];
        (map, starts)
    }

    /// Procedural "islands" map (128x96): random wall blobs on an open field.
    /// Deterministic for a given seed. Team starts are symmetric, near the
    /// edges and center of the map.
    pub fn islands(seed: u64) -> (Map, Vec<(u32, u32)>) {
        let width = 128u32;
        let height = 96u32;
        let mut map = Map::empty_bordered(width, height);
        let starts = vec![
            (12, height / 2),
            (width - 13, height / 2),
            (width / 2, 12),
            (width / 2, height - 13),
        ];
        let mut rng = crate::rng::Rng::new(seed);
        for _ in 0..14 {
            let cx = rng.below((width - 20) as usize) as i64 + 10;
            let cy = rng.below((height - 20) as usize) as i64 + 10;
            let r = rng.below(7) as i64 + 3;
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy <= r * r {
                        let (x, y) = (cx + dx, cy + dy);
                        if map.in_bounds(x, y) && x != 0 && y != 0 {
                            map.set_wall(x as u32, y as u32, true);
                        }
                    }
                }
            }
        }
        // Keep the team start areas walkable.
        for &(sx, sy) in &starts {
            for dy in -4i64..=4 {
                for dx in -4i64..=4 {
                    let (x, y) = (sx as i64 + dx, sy as i64 + dy);
                    if map.in_bounds(x, y) {
                        map.set_wall(x as u32, y as u32, false);
                    }
                }
            }
        }
        (map, starts)
    }

    /// Parse ASCII art. `#` = wall, `.` or space = floor, `1`..`6` = floor and
    /// a start position for that team. Returns the map and the start list
    /// (indices 0-5 → positions).
    pub fn from_ascii(rows: &[&str]) -> (Map, Vec<(u32, u32)>) {
        let height = rows.len() as u32;
        let width = rows.iter().map(|r| r.len()).max().unwrap_or(0) as u32;
        let mut map = Map::new(width, height);
        let mut starts: Vec<Option<(u32, u32)>> = vec![None; MAX_TEAMS];
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                let (x, y) = (x as u32, y as u32);
                match ch {
                    '#' => map.set_wall(x, y, true),
                    '.' | ' ' => {}
                    '1'..='6' => {
                        let t = (ch as u8 - b'1') as usize;
                        if t < MAX_TEAMS {
                            starts[t] = Some((x, y));
                        }
                    }
                    _ => {}
                }
            }
        }
        let starts = starts.into_iter().flatten().collect();
        (map, starts)
    }

    /// Compact serialization: width, height, then one byte per cell
    /// (1 = wall, 0 = floor).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.walls.len());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        for w in &self.walls {
            out.push(*w as u8);
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Map> {
        if bytes.len() < 8 {
            return None;
        }
        let width = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        let height = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        // Checked: on 32-bit targets `width * height` can wrap.
        let n = (width as usize).checked_mul(height as usize)?;
        if bytes.len() - 8 < n {
            return None;
        }
        let walls = bytes[8..8 + n].iter().map(|b| *b != 0).collect();
        Some(Map {
            width,
            height,
            walls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_and_walls() {
        let m = Map::empty_bordered(10, 6);
        assert!(m.is_wall(0, 0));
        assert!(m.is_wall(9, 5));
        assert!(!m.is_wall(5, 3));
        assert_eq!(m.width, 10);
        assert_eq!(m.height, 6);
    }

    #[test]
    fn ascii_starts() {
        let (m, starts) = Map::from_ascii(&["#1.2#", "#...#"]);
        assert_eq!(starts, vec![(1, 0), (3, 0)]);
        assert!(m.is_wall(0, 0));
        assert!(!m.is_wall(1, 0));
    }

    #[test]
    fn from_bytes_rejects_huge_dimensions() {
        let mut b = Vec::new();
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        b.push(0);
        assert!(Map::from_bytes(&b).is_none());
    }

    #[test]
    fn degenerate_bordered_maps_do_not_panic() {
        assert_eq!(Map::empty_bordered(0, 5).floor_count(), 0);
        assert_eq!(Map::empty_bordered(5, 0).floor_count(), 0);
    }

    #[test]
    fn bytes_roundtrip() {
        let m = Map::islands(7).0;
        let m2 = Map::from_bytes(&m.to_bytes()).unwrap();
        assert_eq!(m, m2);
    }
}
