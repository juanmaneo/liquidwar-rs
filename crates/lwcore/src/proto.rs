//! Wire protocol for Liquid War RS lockstep netplay.
//!
//! Peers never exchange simulation state: they only trade cursor [`Move`]s
//! tagged with the tick they belong to, plus [`NetMessage::HashCheck`]
//! fingerprints used to detect desyncs early. This keeps traffic tiny and
//! cheating-by-state-tampering impossible: both peers run the same
//! deterministic [`crate::game::Game`] and only commands travel.

use crate::game::Command;

pub const PROTO_VERSION: u16 = 1;

/// The lockstep input delay, in ticks. Peers send inputs for
/// `tick + INPUT_DELAY_TICKS` so both sides have them in hand before the
/// tick executes.
pub const INPUT_DELAY_TICKS: u64 = 8;

/// A cursor action on the wire — the serializable twin of [`Command``].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// One keyboard-style step.
    Step { dx: i8, dy: i8 },
    /// One mouse-style absolute placement.
    Set { x: u32, y: u32 },
}

impl Move {
    pub fn from_command(cmd: Command) -> Move {
        match cmd {
            Command::MoveCursor { dx, dy, .. } => Move::Step { dx, dy },
            Command::SetCursor { x, y, .. } => Move::Set { x, y },
        }
    }

    pub fn to_command(self, team: u8) -> Command {
        match self {
            Move::Step { dx, dy } => Command::MoveCursor { team, dx, dy },
            Move::Set { x, y } => Command::SetCursor { team, x, y },
        }
    }
}

/// How one player is set up at match start: team index, initial cursor and
/// army size. The host decides these and sends them in [`NetMessage::StartGame`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamSetup {
    pub team: u8,
    pub cursor: (u32, u32),
    pub army_size: u32,
}

/// One protocol datagram. Framing on the wire (see `lwbevy::net`) is a
/// little-endian `u32` length prefix followed by `NetMessage::to_bytes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetMessage {
    /// Greeting / version probe.
    Hello { proto_version: u16 },
    /// Host -> joiner: everything needed to build the identical game.
    StartGame {
        proto_version: u16,
        map_name: String,
        seed: u64,
        duration_ticks: u64,
        teams: Vec<TeamSetup>,
    },
    /// A cursor input intended for simulation tick `tick`.
    Input { tick: u64, team: u8, mv: Move },
    /// State fingerprint after tick `tick`; peers compare to detect desync.
    HashCheck { tick: u64, hash: u64 },
    /// Graceful disconnect.
    Quit,
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// Minimal cursor reader over a byte slice.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.pos + n > self.bytes.len() {
            return None;
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_le_bytes(s.try_into().unwrap()))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
    }
    fn i8(&mut self) -> Option<i8> {
        self.take(1).map(|s| s[0] as i8)
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).ok()
    }
}

impl Move {
    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Move::Step { dx, dy } => {
                out.push(1);
                out.push(*dx as u8);
                out.push(*dy as u8);
            }
            Move::Set { x, y } => {
                out.push(2);
                put_u32(out, *x);
                put_u32(out, *y);
            }
        }
    }

    fn read(r: &mut Reader) -> Option<Move> {
        match r.u8()? {
            1 => {
                let dx = r.i8()?;
                let dy = r.i8()?;
                Some(Move::Step { dx, dy })
            }
            2 => {
                let x = r.u32()?;
                let y = r.u32()?;
                Some(Move::Set { x, y })
            }
            _ => None,
        }
    }
}

impl NetMessage {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            NetMessage::Hello { proto_version } => {
                out.push(1);
                put_u16(&mut out, *proto_version);
            }
            NetMessage::StartGame {
                proto_version,
                map_name,
                seed,
                duration_ticks,
                teams,
            } => {
                out.push(2);
                put_u16(&mut out, *proto_version);
                put_str(&mut out, map_name);
                put_u64(&mut out, *seed);
                put_u64(&mut out, *duration_ticks);
                put_u32(&mut out, teams.len() as u32);
                for ts in teams {
                    out.push(ts.team);
                    put_u32(&mut out, ts.cursor.0);
                    put_u32(&mut out, ts.cursor.1);
                    put_u32(&mut out, ts.army_size);
                }
            }
            NetMessage::Input { tick, team, mv } => {
                out.push(3);
                put_u64(&mut out, *tick);
                out.push(*team);
                mv.write(&mut out);
            }
            NetMessage::HashCheck { tick, hash } => {
                out.push(4);
                put_u64(&mut out, *tick);
                put_u64(&mut out, *hash);
            }
            NetMessage::Quit => out.push(5),
        }
        out
    }

    /// Inverse of [`to_bytes`]; returns `None` on any malformed input.
    pub fn from_bytes(bytes: &[u8]) -> Option<NetMessage> {
        let mut r = Reader::new(bytes);
        let msg = match r.u8()? {
            1 => NetMessage::Hello {
                proto_version: r.u16()?,
            },
            2 => {
                let proto_version = r.u16()?;
                let map_name = r.str()?;
                let seed = r.u64()?;
                let duration_ticks = r.u64()?;
                let n = r.u32()? as usize;
                // Sanity limit: no team list can be huge.
                if n > crate::map::MAX_TEAMS {
                    return None;
                }
                let mut teams = Vec::with_capacity(n);
                for _ in 0..n {
                    let team = r.u8()?;
                    let x = r.u32()?;
                    let y = r.u32()?;
                    let army_size = r.u32()?;
                    teams.push(TeamSetup {
                        team,
                        cursor: (x, y),
                        army_size,
                    });
                }
                NetMessage::StartGame {
                    proto_version,
                    map_name,
                    seed,
                    duration_ticks,
                    teams,
                }
            }
            3 => {
                let tick = r.u64()?;
                let team = r.u8()?;
                let mv = Move::read(&mut r)?;
                NetMessage::Input { tick, team, mv }
            }
            4 => {
                let tick = r.u64()?;
                let hash = r.u64()?;
                NetMessage::HashCheck { tick, hash }
            }
            5 => NetMessage::Quit,
            _ => return None,
        };
        // Trailing garbage = malformed.
        if r.pos != bytes.len() {
            return None;
        }
        Some(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Map;

    #[test]
    fn roundtrip_all_kinds() {
        let msgs = vec![
            NetMessage::Hello {
                proto_version: PROTO_VERSION,
            },
            NetMessage::StartGame {
                proto_version: PROTO_VERSION,
                map_name: "islands".to_string(),
                seed: 42,
                duration_ticks: 7680,
                teams: vec![
                    TeamSetup {
                        team: 0,
                        cursor: (12, 48),
                        army_size: 400,
                    },
                    TeamSetup {
                        team: 1,
                        cursor: (115, 48),
                        army_size: 400,
                    },
                ],
            },
            NetMessage::Input {
                tick: 8,
                team: 1,
                mv: Move::Step { dx: -1, dy: 0 },
            },
            NetMessage::Input {
                tick: 9,
                team: 0,
                mv: Move::Set { x: 10, y: 10 },
            },
            NetMessage::HashCheck {
                tick: 7,
                hash: 0xDEAD_BEEF_1234_5678,
            },
            NetMessage::Quit,
        ];
        for m in &msgs {
            let bytes = m.to_bytes();
            let back = NetMessage::from_bytes(&bytes).expect("parse failed");
            assert_eq!(&back, m);
        }
    }

    #[test]
    fn start_game_from_map() {
        let (map, starts) = Map::islands(3);
        let teams: Vec<TeamSetup> = (0u8..2)
            .map(|t| TeamSetup {
                team: t,
                cursor: starts[t as usize],
                army_size: 300,
            })
            .collect();
        let msg = NetMessage::StartGame {
            proto_version: PROTO_VERSION,
            map_name: "islands".to_string(),
            seed: 3,
            duration_ticks: 64 * 120,
            teams,
        };
        let back = NetMessage::from_bytes(&msg.to_bytes()).unwrap();
        match back {
            NetMessage::StartGame { teams, .. } => {
                assert_eq!(teams.len(), 2);
                assert_eq!(teams[0].cursor, starts[0]);
                assert!(!map.is_wall(teams[0].cursor.0, teams[0].cursor.1));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn malformed_rejected() {
        assert!(NetMessage::from_bytes(&[]).is_none());
        assert!(NetMessage::from_bytes(&[0]).is_none()); // unknown tag
        assert!(NetMessage::from_bytes(&[1, 1]).is_none()); // truncated u16
        // trailing garbage
        let mut b = NetMessage::Quit.to_bytes();
        b.push(0);
        assert!(NetMessage::from_bytes(&b).is_none());
    }

    #[test]
    fn moves_convert() {
        for (team, cmd) in [
            (
                0u8,
                Command::MoveCursor {
                    team: 0,
                    dx: 3,
                    dy: -3,
                },
            ),
            (
                1u8,
                Command::SetCursor {
                    team: 1,
                    x: 191,
                    y: 143,
                },
            ),
        ] {
            let mv = Move::from_command(cmd);
            let back = mv.to_command(team);
            // Team is carried by the conversion, the rest must match.
            assert_eq!(back, cmd);
            assert_eq!(Move::from_command(back), mv);
        }
    }
}
