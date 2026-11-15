//! Authenticated TCP lockstep netcode for liquidwar-rs.
//!
//! Peers authenticate each other with a pre-shared secret and fresh nonces.
//! Every subsequent frame is authenticated with HMAC-SHA256 and a monotonic
//! sequence number. The stream is authenticated but not encrypted.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use lwcore::proto::{Move, NetMessage, TeamSetup, INPUT_DELAY_TICKS, PROTO_VERSION};

type HmacSha256 = Hmac<Sha256>;

/// Largest accepted authenticated payload, in bytes.
const MAX_FRAME_LEN: usize = 1 << 20;
/// Sequence number, message tag, and truncated MAC.
const FRAME_HEADER_LEN: usize = 8 + 4 + 16;

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
        self.teams
            .first()
            .map(|t| t.army_size as usize)
            .unwrap_or(0)
    }
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts keys of any size");
    for part in parts {
        mac.update(part);
    }
    let output = mac.finalize().into_bytes();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&output);
    bytes
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Direction-separated keys prevent reflecting a valid frame back to its sender.
fn derive_keys(secret: &str) -> ([u8; 32], [u8; 32]) {
    let master = hmac_sha256(secret.as_bytes(), &[b"liquidwar-rs v2 session key"]);
    (
        hmac_sha256(&master, &[b"host to joiner"]),
        hmac_sha256(&master, &[b"joiner to host"]),
    )
}

fn disconnected() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "peer disconnected during handshake",
    )
}

/// One authenticated peer connection.
pub struct NetSession {
    stream: TcpStream,
    pub local_team: u8,
    pub remote_team: u8,
    recv_buf: Vec<u8>,
    pub closed: bool,
    /// The stream carried a malformed, replayed, or unauthenticated frame.
    pub corrupt: bool,
    send_key: [u8; 32],
    recv_key: [u8; 32],
    send_seq: u64,
    recv_seq: u64,
}

impl NetSession {
    fn new(stream: TcpStream, local_team: u8, remote_team: u8, secret: &str) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        let (host_to_joiner, joiner_to_host) = derive_keys(secret);
        let (send_key, recv_key) = if local_team == 0 {
            (host_to_joiner, joiner_to_host)
        } else {
            (joiner_to_host, host_to_joiner)
        };
        Ok(Self {
            stream,
            local_team,
            remote_team,
            recv_buf: Vec::new(),
            closed: false,
            corrupt: false,
            send_key,
            recv_key,
            send_seq: 0,
            recv_seq: 0,
        })
    }

    #[cfg(test)]
    fn with_keys(
        stream: TcpStream,
        local_team: u8,
        remote_team: u8,
        send_key: [u8; 32],
        recv_key: [u8; 32],
    ) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            local_team,
            remote_team,
            recv_buf: Vec::new(),
            closed: false,
            corrupt: false,
            send_key,
            recv_key,
            send_seq: 0,
            recv_seq: 0,
        })
    }

    /// Authenticate, frame, and send one message.
    pub fn send(&mut self, msg: &NetMessage) -> io::Result<()> {
        let payload = msg.to_bytes();
        let Some(&message_tag) = payload.first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty protocol message",
            ));
        };
        let len = u32::try_from(FRAME_HEADER_LEN + payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
        let len_bytes = len.to_le_bytes();
        let seq_bytes = self.send_seq.to_le_bytes();
        let tag_bytes = u32::from(message_tag).to_le_bytes();
        let tag = hmac_sha256(
            &self.send_key,
            &[&len_bytes, &seq_bytes, &tag_bytes, &payload],
        );

        let mut frame = Vec::with_capacity(4 + len as usize);
        frame.extend_from_slice(&len_bytes);
        frame.extend_from_slice(&seq_bytes);
        frame.extend_from_slice(&tag_bytes);
        frame.extend_from_slice(&tag[..16]);
        frame.extend_from_slice(&payload);
        self.stream.write_all(&frame)?;
        self.send_seq = self.send_seq.wrapping_add(1);
        Ok(())
    }

    /// Send one cursor command as a delayed input.
    pub fn send_input(&mut self, tick: u64, team: u8, mv: Move) -> io::Result<()> {
        self.send(&NetMessage::Input {
            tick: tick.saturating_add(INPUT_DELAY_TICKS),
            team,
            mv,
        })
    }

    /// Non-blocking receive of one authenticated message, if a full frame is available.
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
            if let Some(msg) = self.try_parse() {
                return Ok(Some(msg));
            }
            if self.corrupt {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid authenticated frame",
                ));
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
        let len_bytes: [u8; 4] = self.recv_buf[0..4].try_into().unwrap();
        let len = u32::from_le_bytes(len_bytes) as usize;
        if !(FRAME_HEADER_LEN..=MAX_FRAME_LEN).contains(&len) {
            self.corrupt = true;
            return None;
        }
        if self.recv_buf.len() < 4 + len {
            return None;
        }

        let seq_bytes: [u8; 8] = self.recv_buf[4..12].try_into().unwrap();
        let tag_bytes: [u8; 4] = self.recv_buf[12..16].try_into().unwrap();
        let got_tag = &self.recv_buf[16..32];
        let payload = &self.recv_buf[32..4 + len];
        let seq = u64::from_le_bytes(seq_bytes);
        let message_tag = u32::from_le_bytes(tag_bytes);
        let expected_tag = hmac_sha256(
            &self.recv_key,
            &[&len_bytes, &seq_bytes, &tag_bytes, payload],
        );

        if seq != self.recv_seq
            || !constant_time_eq(&expected_tag[..16], got_tag)
            || payload.first().copied() != Some(message_tag as u8)
        {
            self.corrupt = true;
            return None;
        }

        let msg = NetMessage::from_bytes(payload);
        self.recv_buf.drain(..4 + len);
        self.recv_seq = self.recv_seq.wrapping_add(1);
        if msg.is_none() {
            self.corrupt = true;
        }
        msg
    }
}

/// Host a match: listen on `port`, authenticate the opponent, and send the spec.
pub fn host_lobby(port: u16, spec: &GameSpec, secret: &str) -> io::Result<NetSession> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    host_on(listener, spec, secret)
}

fn host_on(listener: TcpListener, spec: &GameSpec, secret: &str) -> io::Result<NetSession> {
    let addr = listener.local_addr()?;
    println!("liquidwar-rs: hosting on {addr}, waiting for opponent...");
    let (stream, peer) = listener.accept()?;
    println!("liquidwar-rs: opponent connected: {peer}");
    let mut session = NetSession::new(stream, 0, 1, secret)?;

    let mut host_nonce = [0u8; 32];
    getrandom::fill(&mut host_nonce).map_err(io::Error::other)?;
    session.send(&NetMessage::Challenge {
        proto_version: PROTO_VERSION,
        host_nonce,
    })?;

    let response = session.recv_blocking()?.ok_or_else(disconnected)?;
    let client_nonce = match response {
        NetMessage::AuthResponse {
            client_nonce,
            proof,
        } => {
            let expected = hmac_sha256(
                secret.as_bytes(),
                &[b"lw2 client proof", &host_nonce, &client_nonce],
            );
            if !constant_time_eq(&expected, &proof) {
                let _ = session.send(&NetMessage::Quit);
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "authentication failed",
                ));
            }
            client_nonce
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected AuthResponse, got {other:?}"),
            ));
        }
    };

    let proof = hmac_sha256(
        secret.as_bytes(),
        &[b"lw2 host proof", &client_nonce, &host_nonce],
    );
    session.send(&NetMessage::AuthProof { proof })?;
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

/// Join a match and mutually authenticate using the shared secret.
pub fn join_lobby(addr: &str, secret: &str) -> io::Result<(NetSession, GameSpec)> {
    let stream = TcpStream::connect(addr)?;
    let mut session = NetSession::new(stream, 1, 0, secret)?;

    let challenge = session.recv_blocking()?.ok_or_else(disconnected)?;
    let host_nonce = match challenge {
        NetMessage::Challenge {
            proto_version,
            host_nonce,
        } => {
            if proto_version != PROTO_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("protocol version mismatch: host={proto_version}, us={PROTO_VERSION}"),
                ));
            }
            host_nonce
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected Challenge, got {other:?}"),
            ));
        }
    };

    let mut client_nonce = [0u8; 32];
    getrandom::fill(&mut client_nonce).map_err(io::Error::other)?;
    let proof = hmac_sha256(
        secret.as_bytes(),
        &[b"lw2 client proof", &host_nonce, &client_nonce],
    );
    session.send(&NetMessage::AuthResponse {
        client_nonce,
        proof,
    })?;

    match session.recv_blocking()?.ok_or_else(disconnected)? {
        NetMessage::AuthProof { proof } => {
            let expected = hmac_sha256(
                secret.as_bytes(),
                &[b"lw2 host proof", &client_nonce, &host_nonce],
            );
            if !constant_time_eq(&expected, &proof) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "host authentication failed",
                ));
            }
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected AuthProof, got {other:?}"),
            ));
        }
    }

    match session.recv_blocking()?.ok_or_else(disconnected)? {
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
                    format!("protocol version mismatch: host={proto_version}, us={PROTO_VERSION}"),
                ));
            }
            let spec = GameSpec {
                map_name,
                seed,
                duration_ticks,
                teams: validate_teams(teams)?,
            };
            Ok((session, spec))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected StartGame, got {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec() -> GameSpec {
        GameSpec {
            map_name: "arena".to_string(),
            seed: 17,
            duration_ticks: 120,
            teams: vec![
                TeamSetup {
                    team: 0,
                    cursor: (1, 1),
                    army_size: 10,
                },
                TeamSetup {
                    team: 1,
                    cursor: (2, 2),
                    army_size: 10,
                },
            ],
        }
    }

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    #[test]
    fn lobby_handshake_authenticates_both_peers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let spec = test_spec();
        let host = std::thread::spawn(move || {
            host_on(listener, &spec, "correct horse battery staple").unwrap()
        });
        let (_, joined_spec) =
            join_lobby(&addr.to_string(), "correct horse battery staple").unwrap();
        let host_session = host.join().unwrap();
        assert_eq!(host_session.local_team, 0);
        assert_eq!(joined_spec.seed, 17);
        assert_eq!(joined_spec.teams.len(), 2);
    }

    #[test]
    fn wrong_password_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let host = std::thread::spawn(move || host_on(listener, &test_spec(), "right password"));
        assert!(join_lobby(&addr.to_string(), "wrong password").is_err());
        assert!(host.join().unwrap().is_err());
    }

    #[test]
    fn frames_reject_tampering_and_replays() {
        let (server, client) = connected_pair();
        let (host_to_joiner, joiner_to_host) = derive_keys("secret");
        let mut host = NetSession::with_keys(server, 0, 1, host_to_joiner, joiner_to_host).unwrap();
        let mut joiner =
            NetSession::with_keys(client, 1, 0, joiner_to_host, host_to_joiner).unwrap();

        host.send(&NetMessage::Quit).unwrap();
        assert_eq!(joiner.try_recv().unwrap(), Some(NetMessage::Quit));

        host.send(&NetMessage::Quit).unwrap();
        let mut raw = [0u8; 256];
        let n = joiner.stream.read(&mut raw).unwrap();
        joiner.recv_buf.extend_from_slice(&raw[..n]);
        joiner.recv_buf[9] ^= 0x80;
        assert!(joiner.try_recv().unwrap().is_none());
        assert!(joiner.corrupt);
    }
}
