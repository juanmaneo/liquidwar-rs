//! TCP lockstep netcode for liquidwar-rs.
//!
//! The host listens, then sends a single [`NetMessage::StartGame`] with
//! everything the joiner needs to build the *identical* deterministic
//! game (same map, seed, team setups). After that both peers:
//!
//! * send one [`NetMessage::Input`] per tick, tagged `tick +
//!   INPUT_DELAY_TICKS` so the command is already in the peer's queue
//!   before the tick runs (classic deterministic lockstep);
//! * send a [`NetMessage::HashCheck`] per tick and verify the peer's, so
//!   any divergence is caught within a tick instead of silently ruining
//!   the match.
//!
//! Only cursor commands travel: never game state.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};

use lwcore::proto::{Move, NetMessage, TeamSetup, INPUT_DELAY_TICKS, PROTO_VERSION};

/// Largest accepted frame payload, in bytes.
const MAX_FRAME_LEN: usize = 1 << 20;

/// Everything needed to build one match. Produced by the host and shipped
/// to the joiner inside [`NetMessage::StartGame`].
#[derive(Clone, Debug)]
pub struct GameSpec {
    pub map_name: String,
    pub seed: u64,
    pub duration_ticks: u64,
    pub teams: Vec<TeamSetup>,
}

impl GameSpec {
    pub fn army_size(&self) -> usize {
        self.teams.first().map(|t| t.army_size as usize).unwrap_or(0)
    }
}

/// One established peer connection.
pub struct NetSession {
    stream: TcpStream,
    pub local_team: u8,
    pub remote_team: u8,
    /// Frames received but not yet parsed.
    recv_buf: Vec<u8>,
    /// Set when the peer closed the connection.
    pub closed: bool,
    /// Set when the stream carried a malformed or oversized frame. The
    /// stream can no longer be trusted to stay in sync, so the session
    /// must be torn down.
    pub corrupt: bool,
}

impl NetSession {
    fn new(stream: TcpStream, local_team: u8, remote_team: u8) -> io::Result<NetSession> {
        stream.set_nodelay(true)?;
        Ok(NetSession {
            stream,
            local_team,
            remote_team,
            recv_buf: Vec::new(),
            closed: false,
            corrupt: false,
        })
    }

    /// Frame and send one message.
    pub fn send(&mut self, msg: &NetMessage) -> io::Result<()> {
        let payload = msg.to_bytes();
        let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&payload);
        self.stream.write_all(&frame)
    }

    /// Send one cursor command as a delayed input.
    pub fn send_input(&mut self, tick: u64, team: u8, mv: Move) -> io::Result<()> {
        self.send(&NetMessage::Input {
            tick: tick + INPUT_DELAY_TICKS,
            team,
            mv,
        })
    }

    /// Non-blocking receive of one message, if a full frame is in the
    /// socket. Also updates `closed`.
    pub fn try_recv(&mut self) -> io::Result<Option<NetMessage>> {
        self.stream.set_nonblocking(true)?;
        let mut chunk = [0u8; 8192];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.closed = true;
                    break;
                }
                Ok(n) => self.recv_buf.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(self.try_parse())
    }

    /// Blocking receive; `Ok(None)` means the peer hung up.
    pub fn recv_blocking(&mut self) -> io::Result<Option<NetMessage>> {
        self.stream.set_nonblocking(false)?;
        loop {
            if self.corrupt {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed frame from peer",
                ));
            }
            if let Some(msg) = self.try_parse() {
                return Ok(Some(msg));
            }
            let mut chunk = [0u8; 8192];
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                self.closed = true;
                return Ok(None);
            }
            self.recv_buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn try_parse(&mut self) -> Option<NetMessage> {
        if self.corrupt || self.recv_buf.len() < 4 {
            return None;
        }
        let len = u32::from_le_bytes(self.recv_buf[0..4].try_into().unwrap()) as usize;
        if len > MAX_FRAME_LEN {
            self.corrupt = true;
            return None;
        }
        if self.recv_buf.len() < 4 + len {
            return None;
        }
        let msg = NetMessage::from_bytes(&self.recv_buf[4..4 + len]);
        self.recv_buf.drain(0..4 + len);
        if msg.is_none() {
            self.corrupt = true;
        }
        msg
    }
}

/// Host a match: listen on `port`, accept one opponent, hand them the spec.
pub fn host_lobby(port: u16, spec: &GameSpec) -> io::Result<NetSession> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    println!("liquidwar-rs: hosting on 0.0.0.0:{port}, waiting for opponent...");
    let (stream, peer) = listener.accept()?;
    println!("liquidwar-rs: opponent connected: {peer}");
    let mut session = NetSession::new(stream, 0, 1)?;
    session.send(&NetMessage::StartGame {
        proto_version: PROTO_VERSION,
        map_name: spec.map_name.clone(),
        seed: spec.seed,
        duration_ticks: spec.duration_ticks,
        teams: spec.teams.clone(),
    })?;
    Ok(session)
}

/// Teams must be exactly `0..n` with no duplicates or gaps. Returns them
/// sorted by team index, so positions and `team` fields always agree.
fn validate_teams(mut teams: Vec<TeamSetup>) -> io::Result<Vec<TeamSetup>> {
    teams.sort_by_key(|t| t.team);
    for (i, t) in teams.iter().enumerate() {
        if t.team as usize != i {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid team layout: expected team {i}, got {}", t.team),
            ));
        }
    }
    Ok(teams)
}

/// Join a match: connect, wait for the host's StartGame, build the spec.
pub fn join_lobby(addr: &str) -> io::Result<(NetSession, GameSpec)> {
    let stream = TcpStream::connect(addr)?;
    let mut session = NetSession::new(stream, 1, 0)?;
    let msg = loop {
        match session.recv_blocking()? {
            Some(m) => break m,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "host disconnected during handshake",
                ))
            }
        }
    };
    match msg {
        NetMessage::StartGame {
            proto_version,
            map_name,
            seed,
            duration_ticks,
            teams,
        } => {
            if proto_version != PROTO_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "protocol version mismatch: host={proto_version}, us={PROTO_VERSION}"
                    ),
                ));
            }
            let teams = validate_teams(teams)?;
            let spec = GameSpec {
                map_name,
                seed,
                duration_ticks,
                teams,
            };
            Ok((session, spec))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected StartGame, got {other:?}"),
        )),
    }
}
