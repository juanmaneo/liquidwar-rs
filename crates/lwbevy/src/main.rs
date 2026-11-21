//! liquidwar-rs — a Rust port of Liquid War, Bevy 0.16 frontend.
//!
//! Usage:
//!   liquidwar-rs                 two players on one machine
//!   liquidwar-rs --vs-ai         player vs simple bot
//!   liquidwar-rs --host 8080     host an online match on port 8080
//!   liquidwar-rs --join host:8080  join an online match
//!   Online play requires LIQUIDWAR_PASSWORD set to the same secret on both peers.
//!   liquidwar-rs --map corridors   pick the map (arena|corridors|islands)
//!
//! Controls: P1 moves the red cursor with the mouse. P2 moves the blue
//! cursor with the arrow keys (or WASD). R restarts, Esc quits.

use bevy::app::AppExit;
use bevy::prelude::*;
use bevy::render::camera::{OrthographicProjection, Projection, ScalingMode};
use bevy::render::render_asset::RenderAssetUsages;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::text::JustifyText;
use bevy::window::PrimaryWindow;
use lwcore::game::{Command, Game, GameParams, Scoreboard};
use lwcore::map::Map;
use lwcore::proto::{Move, NetMessage, TeamSetup, INPUT_DELAY_TICKS};

mod bot;
mod net;

use bot::Bot;
use net::{GameSpec, NetSession};

const TICK_HZ: f64 = 64.0;
/// How far ahead of the local tick a peer may schedule an input. A peer can
/// lead us by about one input delay, and its inputs are tagged a further
/// input delay ahead; the extra slack covers a stalled barrier.
const INPUT_WINDOW_TICKS: u64 = 2 * INPUT_DELAY_TICKS + 4;
/// Upper bound on queued inputs, so a misbehaving peer cannot exhaust memory.
const MAX_PENDING_INPUTS: usize = 4096;
const MAX_PENDING_HASHES: usize = 256;
/// World size of one map cell.
const CELL: f32 = 4.0;
/// Fighters per army.
const ARMY_SIZE: usize = 400;
/// Match length.
const GAME_SECONDS: u64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Two players sharing one machine.
    Local,
    /// Player vs bot.
    VsAi,
    /// Hosting an online match (we are team 0).
    Host,
    /// Joined an online match (we are team 1).
    Join,
}

// ---------------------------------------------------------------------------
// Resources and markers
// ---------------------------------------------------------------------------

/// The deterministic simulation. Owned by the app, mutated in FixedUpdate.
#[derive(Resource)]
struct LwGame(Game);

#[derive(Resource)]
struct MatchSetup {
    mode: Mode,
    map_name: String,
    seed: u64,
    duration_ticks: u64,
    army_size: usize,
    cursors: Vec<(u32, u32)>,
}

/// Commands tagged with the tick they must be applied on.
#[derive(Resource)]
struct PendingInputs(Vec<(u64, Command)>);

/// Simulation tick for which the local input has already been queued/sent.
#[derive(Resource, Default)]
struct LastInputTick(Option<u64>);

/// State fingerprint per completed tick (for desync checks).
#[derive(Resource)]
struct LocalHashes(Vec<u64>);

/// Peer fingerprints that arrived before this side completed the same tick.
#[derive(Resource, Default)]
struct PendingHashes(Vec<(u64, u64)>);

#[derive(Resource)]
struct Desync(bool);

#[derive(Resource)]
struct OverRes(Option<Scoreboard>);

#[derive(Resource)]
struct NetOpt(Option<NetSession>);

#[derive(Resource, Default)]
struct RestartState {
    phase: RestartPhase,
    id: u64,
    request_sent: bool,
    host_commit: bool,
    joiner_commit: bool,
}

#[derive(Default, Debug, PartialEq, Eq)]
enum RestartPhase {
    #[default]
    Idle,
    HostWaitingReady,
    JoinerPaused,
}

#[derive(Resource)]
struct BotOpt(Option<Bot>);

#[derive(Component)]
struct FighterTag {
    id: usize,
}

#[derive(Component)]
struct CursorTag {
    team: u8,
}

#[derive(Component)]
struct WallTag;

#[derive(Component)]
struct ScoreTextTag;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn map_with_starts(name: &str, seed: u64) -> (Map, Vec<(u32, u32)>) {
    match name {
        "corridors" => Map::corridors(),
        "islands" => Map::islands(seed),
        _ => (Map::arena(), vec![(10, 72), (182, 72), (96, 10), (96, 134)]),
    }
}

fn build_game(setup: &MatchSetup) -> Game {
    let (map, _) = map_with_starts(&setup.map_name, setup.seed);
    let mut game = Game::new(
        map,
        GameParams {
            duration_ticks: setup.duration_ticks,
            cursor_step_interval: 1,
        },
        setup.seed,
    );
    for (i, &cursor) in setup.cursors.iter().enumerate() {
        let team = game.add_team(cursor);
        debug_assert_eq!(team, i as u8);
        game.spawn_army(team, setup.army_size);
    }
    game
}

fn team_color(team: u8) -> Color {
    match team {
        0 => Color::srgb(0.92, 0.16, 0.16), // red
        1 => Color::srgb(0.16, 0.34, 0.95), // blue
        2 => Color::srgb(0.16, 0.85, 0.25), // green
        3 => Color::srgb(0.98, 0.85, 0.10), // yellow
        4 => Color::srgb(0.95, 0.20, 0.80), // magenta
        _ => Color::srgb(0.15, 0.90, 0.95), // cyan
    }
}

fn team_name(team: u8) -> &'static str {
    match team {
        0 => "Red",
        1 => "Blue",
        2 => "Green",
        3 => "Yellow",
        4 => "Magenta",
        _ => "Cyan",
    }
}

/// World position of the center of a cell; row 0 is drawn at the top.
fn cell_to_world(map_h: u32, x: u32, y: u32, z: f32) -> Vec3 {
    Vec3::new(
        (x as f32 + 0.5) * CELL,
        (map_h as f32 - 1.0 - y as f32 + 0.5) * CELL,
        z,
    )
}

/// A small runtime-generated wall texture: a two-tone brick pattern.
fn make_wall_texture() -> Image {
    let size = 16u32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let brick_row = (y / 4) % 2;
            let base: u8 = if ((x / 8) + brick_row) % 2 == 0 {
                150
            } else {
                115
            };
            // Slight noise per pixel via a cheap deterministic mix.
            let n = ((x * 7 + y * 13) % 17) as u8;
            data.extend_from_slice(&[
                base.saturating_sub(n / 2),
                base.saturating_sub(n / 2 + 8),
                base.saturating_sub(n / 2 + 16),
                255,
            ]);
        }
    }
    Image::new(
        Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

fn command_team(cmd: &Command) -> u8 {
    match cmd {
        Command::MoveCursor { team, .. } | Command::SetCursor { team, .. } => *team,
    }
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

fn setup_world(mut commands: Commands, game: Res<LwGame>, mut images: ResMut<Assets<Image>>) {
    let map = &game.0.map;
    let view_w = map.width as f32 * CELL;
    let view_h = map.height as f32 * CELL;

    // Camera: one fixed ortho view of the whole battlefield.
    commands.spawn((
        Camera2d,
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::Fixed {
                width: view_w,
                height: view_h,
            },
            ..OrthographicProjection::default_2d()
        }),
        Transform::from_xyz(view_w / 2.0, view_h / 2.0, 1000.0),
    ));

    // Walls: one sprite per wall cell, sharing a runtime texture.
    let texture = images.add(make_wall_texture());
    for y in 0..map.height {
        for x in 0..map.width {
            if map.is_wall(x, y) {
                commands.spawn((
                    Sprite {
                        image: texture.clone(),
                        custom_size: Some(Vec2::splat(CELL)),
                        color: Color::WHITE,
                        ..default()
                    },
                    Transform::from_translation(cell_to_world(map.height, x, y, 0.0)),
                    WallTag,
                ));
            }
        }
    }

    // Fighters: one colored square each, in game order.
    for f in game.0.fighters() {
        commands.spawn((
            Sprite::from_color(team_color(f.team), Vec2::splat(CELL * 0.9)),
            Transform::from_translation(cell_to_world(map.height, f.x, f.y, 1.0)),
            FighterTag { id: f.id },
        ));
    }

    // Team cursors: bright blocks above everything but the text.
    for t in 0..game.0.num_teams() {
        let (x, y) = game.0.cursor(t as u8);
        commands.spawn((
            Sprite::from_color(team_color(t as u8), Vec2::splat(CELL * 1.7)),
            Transform::from_translation(cell_to_world(map.height, x, y, 2.0)),
            CursorTag { team: t as u8 },
        ));
    }

    // Score line, centered at the top.
    commands.spawn((
        Text2d::new("liquidwar-rs"),
        TextLayout {
            justify: JustifyText::Left,
            ..default()
        },
        TextFont {
            font_size: 28.0,
            ..default()
        },
        TextColor(Color::WHITE),
        Transform::from_xyz(view_w / 2.0, view_h - 18.0, 5.0),
        ScoreTextTag,
    ));
}

// ---------------------------------------------------------------------------
// Simulation (FixedUpdate, 64 Hz)
// ---------------------------------------------------------------------------

/// What the caller should do after the network has been drained.
enum NetFlow {
    /// Keep simulating.
    Continue,
    /// Stop simulating and quit the app.
    Exit,
}

fn drain_net(
    session: &mut NetSession,
    pending: &mut PendingInputs,
    hashes: &mut LocalHashes,
    pending_hashes: &mut PendingHashes,
    desync: &mut Desync,
    restart: &mut RestartState,
    tick: u64,
) -> NetFlow {
    let mut err = false;
    loop {
        match session.try_recv() {
            Ok(None) => break,
            Ok(Some(NetMessage::Input { tick: t, team, mv })) => {
                let in_window = t >= tick && t <= tick.saturating_add(INPUT_WINDOW_TICKS);
                let duplicate = pending
                    .0
                    .iter()
                    .any(|(queued_tick, cmd)| *queued_tick == t && command_team(cmd) == team);
                if in_window && !duplicate && pending.0.len() < MAX_PENDING_INPUTS {
                    pending.0.push((t, mv.to_command(team)));
                }
            }
            Ok(Some(NetMessage::HashCheck { tick: t, hash })) => {
                if restart.phase == RestartPhase::Idle {
                    match usize::try_from(t)
                        .ok()
                        .and_then(|i| hashes.0.get(i).copied())
                    {
                        Some(local_hash) if local_hash == hash => {}
                        Some(_) => {
                            desync.0 = true;
                            eprintln!("liquidwar-rs: DESYNC detected at tick {t}");
                        }
                        None if t >= tick
                            && t <= tick.saturating_add(INPUT_WINDOW_TICKS)
                            && pending_hashes.0.len() < MAX_PENDING_HASHES =>
                        {
                            if let Some((_, queued_hash)) = pending_hashes
                                .0
                                .iter()
                                .find(|(queued_tick, _)| *queued_tick == t)
                            {
                                if *queued_hash != hash {
                                    desync.0 = true;
                                    eprintln!("liquidwar-rs: conflicting hash checks at tick {t}");
                                }
                            } else {
                                pending_hashes.0.push((t, hash));
                            }
                        }
                        None => {
                            desync.0 = true;
                            eprintln!("liquidwar-rs: DESYNC detected at tick {t}");
                        }
                    }
                }
            }
            Ok(Some(NetMessage::RestartRequest)) => {
                if session.local_team == 0 && restart.phase == RestartPhase::Idle {
                    restart.id = restart.id.wrapping_add(1);
                    let _ = session.send(&NetMessage::RestartPrepare {
                        restart_id: restart.id,
                    });
                    restart.phase = RestartPhase::HostWaitingReady;
                }
            }
            Ok(Some(NetMessage::RestartPrepare { restart_id })) => {
                if session.local_team == 1 && restart.phase == RestartPhase::Idle {
                    restart.id = restart_id;
                    restart.request_sent = false;
                    restart.phase = RestartPhase::JoinerPaused;
                    let _ = session.send(&NetMessage::RestartReady { restart_id });
                }
            }
            Ok(Some(NetMessage::RestartReady { restart_id })) => {
                if session.local_team == 0
                    && restart.phase == RestartPhase::HostWaitingReady
                    && restart_id == restart.id
                {
                    restart.host_commit = true;
                }
            }
            Ok(Some(NetMessage::RestartCommit { restart_id })) => {
                if session.local_team == 1
                    && restart.phase == RestartPhase::JoinerPaused
                    && restart_id == restart.id
                {
                    restart.joiner_commit = true;
                }
            }
            Ok(Some(NetMessage::Quit)) => {
                println!("liquidwar-rs: opponent left the game");
                return NetFlow::Exit;
            }
            Ok(Some(other)) => eprintln!("liquidwar-rs: unexpected message {other:?}"),
            Err(_) => {
                err = true;
                break;
            }
        }
    }
    if session.corrupt {
        eprintln!("liquidwar-rs: invalid authenticated data from opponent, disconnecting");
        return NetFlow::Exit;
    }
    if err || session.closed {
        eprintln!("liquidwar-rs: connection lost");
        return NetFlow::Exit;
    }
    NetFlow::Continue
}

#[allow(clippy::too_many_arguments)] // Bevy system parameters
fn fixed_simulate(
    mut commands: Commands,
    fighters: Query<Entity, With<FighterTag>>,
    mut game: ResMut<LwGame>,
    setup: Res<MatchSetup>,
    mut pending: ResMut<PendingInputs>,
    mut last_input_tick: ResMut<LastInputTick>,
    mut net: ResMut<NetOpt>,
    mut restart: ResMut<RestartState>,
    mut hashes: ResMut<LocalHashes>,
    mut pending_hashes: ResMut<PendingHashes>,
    mut desync: ResMut<Desync>,
    mut over: ResMut<OverRes>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut bot: ResMut<BotOpt>,
    mut exit: EventWriter<AppExit>,
) {
    let tick = game.0.tick;
    let net_mode = matches!(setup.mode, Mode::Host | Mode::Join);
    let delay = if net_mode { INPUT_DELAY_TICKS } else { 0 };

    // Drain control messages even when simulation is paused, over, or desynced.
    if let Some(session) = net.0.as_mut() {
        match drain_net(
            session,
            &mut pending,
            &mut hashes,
            &mut pending_hashes,
            &mut desync,
            &mut restart,
            tick,
        ) {
            NetFlow::Continue => {}
            NetFlow::Exit => {
                exit.write(AppExit::Success);
                return;
            }
        }
    }

    if restart.host_commit {
        restart.host_commit = false;
        if let Some(session) = net.0.as_mut() {
            let _ = session.send(&NetMessage::RestartCommit {
                restart_id: restart.id,
            });
        }
        restart.phase = RestartPhase::Idle;
        reset_match(
            &mut commands,
            &fighters,
            &setup,
            &mut game,
            &mut pending,
            &mut last_input_tick,
            &mut hashes,
            &mut pending_hashes,
            &mut desync,
            &mut over,
            &mut bot,
        );
    }
    if restart.joiner_commit {
        restart.joiner_commit = false;
        restart.phase = RestartPhase::Idle;
        reset_match(
            &mut commands,
            &fighters,
            &setup,
            &mut game,
            &mut pending,
            &mut last_input_tick,
            &mut hashes,
            &mut pending_hashes,
            &mut desync,
            &mut over,
            &mut bot,
        );
    }

    if desync.0 || over.0.is_some() || restart.phase != RestartPhase::Idle {
        return;
    }

    // 2) Human input. In net mode, exactly one input message per tick so
    //    the peer never stalls (a missing mouse position degrades to a
    //    no-op step). The mouse drives our own team; on a shared machine
    //    it drives team 0 and the keyboard drives team 1.
    let mouse_team = match net.0.as_ref() {
        Some(session) => session.local_team,
        None => 0,
    };
    let mouse_cmd = mouse_cursor_command(&game.0, &windows, mouse_team);
    let cmd = mouse_cmd.unwrap_or(Command::MoveCursor {
        team: mouse_team,
        dx: 0,
        dy: 0,
    });
    if last_input_tick.0 != Some(tick) {
        pending.0.push((tick + delay, cmd));
        if let Some(session) = net.0.as_mut() {
            let mv = Move::from_command(cmd);
            let _ = session.send_input(tick, mouse_team, mv);
        }
        last_input_tick.0 = Some(tick);
    }
    if setup.mode == Mode::Local {
        if let Some((dx, dy)) = key_direction(&keys) {
            pending
                .0
                .push((tick, Command::MoveCursor { team: 1, dx, dy }));
        }
    }

    // 3) Bot input (vs-ai): applied on the same tick, deterministic.
    if let Some(bot) = bot.0.as_mut() {
        pending.0.push((tick, bot.step(&game.0)));
    }

    // 4) Lockstep barrier: in net mode wait until the peer's input for
    //    this tick has arrived.
    if net_mode && tick >= INPUT_DELAY_TICKS {
        if let Some(session) = net.0.as_ref() {
            let have = pending
                .0
                .iter()
                .any(|(t, c)| *t == tick && command_team(c) == session.remote_team);
            if !have {
                return; // stall: retry on the next FixedUpdate step
            }
        }
    }

    // 5) Apply due commands, tick, record the fingerprint.
    let mut i = 0;
    while i < pending.0.len() {
        if pending.0[i].0 == tick {
            let (_, c) = pending.0.remove(i);
            game.0.command(c);
        } else {
            i += 1;
        }
    }
    game.0.tick();
    let h = game.0.state_hash();
    hashes.0.push(h);
    if let Some(position) = pending_hashes
        .0
        .iter()
        .position(|(queued_tick, _)| *queued_tick == tick)
    {
        let (_, peer_hash) = pending_hashes.0.remove(position);
        if peer_hash != h {
            desync.0 = true;
            eprintln!("liquidwar-rs: DESYNC detected at tick {tick}");
        }
    }
    if let Some(session) = net.0.as_mut() {
        let _ = session.send(&NetMessage::HashCheck { tick, hash: h });
    }

    if let Some(board) = game.0.game_over() {
        over.0 = Some(board);
    }
}

#[allow(clippy::too_many_arguments)]
fn reset_match(
    commands: &mut Commands,
    fighters: &Query<Entity, With<FighterTag>>,
    setup: &MatchSetup,
    game: &mut LwGame,
    pending: &mut PendingInputs,
    last_input_tick: &mut LastInputTick,
    hashes: &mut LocalHashes,
    pending_hashes: &mut PendingHashes,
    desync: &mut Desync,
    over: &mut OverRes,
    bot: &mut BotOpt,
) {
    for entity in fighters.iter() {
        commands.entity(entity).despawn();
    }
    *game = LwGame(build_game(setup));
    pending.0.clear();
    last_input_tick.0 = None;
    hashes.0.clear();
    pending_hashes.0.clear();
    desync.0 = false;
    over.0 = None;
    if let Some(bot) = bot.0.as_mut() {
        *bot = Bot::new(1, setup.seed ^ 0xB0B_5EED);
    }
    for fighter in game.0.fighters() {
        commands.spawn((
            Sprite::from_color(team_color(fighter.team), Vec2::splat(CELL * 0.9)),
            Transform::from_translation(cell_to_world(
                game.0.map.height,
                fighter.x,
                fighter.y,
                1.0,
            )),
            FighterTag { id: fighter.id },
        ));
    }
}

/// Mouse to SetCursor command; `None` when the cursor is outside the window.
fn mouse_cursor_command(
    game: &Game,
    windows: &Query<&Window, With<PrimaryWindow>>,
    team: u8,
) -> Option<Command> {
    let window = windows.single().ok()?;
    let pos = window.cursor_position()?;
    let (mx, my) = (pos.x, pos.y);
    let map = &game.map;
    let view_w = map.width as f32 * CELL;
    let view_h = map.height as f32 * CELL;
    let world_x = view_w / 2.0 + (mx / window.width() - 0.5) * view_w;
    let world_y = view_h / 2.0 - (my / window.height() - 0.5) * view_h;
    let x = (world_x / CELL).floor().clamp(0.0, (map.width - 1) as f32) as u32;
    let y = (map.height as f32 - 1.0 - world_y / CELL + 0.5)
        .floor()
        .clamp(0.0, (map.height - 1) as f32) as u32;
    Some(Command::SetCursor { team, x, y })
}

fn key_direction(keys: &ButtonInput<KeyCode>) -> Option<(i8, i8)> {
    let left = keys.pressed(KeyCode::ArrowLeft) || keys.pressed(KeyCode::KeyA);
    let right = keys.pressed(KeyCode::ArrowRight) || keys.pressed(KeyCode::KeyD);
    let up = keys.pressed(KeyCode::ArrowUp) || keys.pressed(KeyCode::KeyW);
    let down = keys.pressed(KeyCode::ArrowDown) || keys.pressed(KeyCode::KeyS);
    let dx = right as i8 - left as i8;
    let dy = down as i8 - up as i8;
    if dx == 0 && dy == 0 {
        None
    } else {
        Some((dx, dy))
    }
}

// ---------------------------------------------------------------------------
// Per-frame presentation (Update)
// ---------------------------------------------------------------------------

fn update_sprites(
    game: Res<LwGame>,
    setup: Res<MatchSetup>,
    over: Res<OverRes>,
    desync: Res<Desync>,
    mut fighters: Query<(&mut Transform, &mut Sprite, &FighterTag)>,
    mut cursors: Query<(&mut Transform, &CursorTag), Without<FighterTag>>,
    mut score: Query<&mut Text2d, With<ScoreTextTag>>,
) {
    let map_h = game.0.map.height;

    // Match entities by stable fighter ID rather than query iteration order.
    let views: Vec<_> = game.0.fighters().collect();
    for (mut tr, mut sp, tag) in &mut fighters {
        if let Some(f) = views.get(tag.id).filter(|f| f.id == tag.id) {
            tr.translation = cell_to_world(map_h, f.x, f.y, 1.0);
            sp.color = team_color(f.team);
        }
    }

    // Cursors.
    for (mut tr, cursor) in cursors.iter_mut() {
        let (x, y) = game.0.cursor(cursor.team);
        tr.translation = cell_to_world(map_h, x, y, 2.0);
    }

    // Score line.
    if let Ok(mut text) = score.single_mut() {
        let board = game.0.scoreboard();
        let counts: Vec<String> = (0..board.counts.len())
            .map(|t| format!("{} {}", team_name(t as u8), board.counts[t]))
            .collect();
        let secs_left = game.0.params.duration_ticks.saturating_sub(game.0.tick) / 64;
        let mut line = format!("{}   {}", counts.join("  "), secs_left);
        if desync.0 {
            line.push_str("   DESYNC!");
        } else if let Some(board) = over.0.as_ref() {
            match board.winner() {
                Some(w) => {
                    line.push_str(&format!("   {} WINS!  [R]estart  [Esc]uit", team_name(w)))
                }
                None => line.push_str("   DRAW!  [R]estart  [Esc]uit"),
            }
        } else if matches!(setup.mode, Mode::Host | Mode::Join) {
            line.push_str("   online");
        }
        text.0 = line;
    }
}

// ---------------------------------------------------------------------------
// Global keys (Update)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)] // Bevy system parameters
fn global_keys(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    setup: Res<MatchSetup>,
    mut game: ResMut<LwGame>,
    mut pending: ResMut<PendingInputs>,
    mut last_input_tick: ResMut<LastInputTick>,
    mut hashes: ResMut<LocalHashes>,
    mut pending_hashes: ResMut<PendingHashes>,
    mut desync: ResMut<Desync>,
    mut over: ResMut<OverRes>,
    mut bot: ResMut<BotOpt>,
    mut net: ResMut<NetOpt>,
    mut restart: ResMut<RestartState>,
    fighters: Query<Entity, With<FighterTag>>,
    mut exit: EventWriter<AppExit>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        if let Some(session) = net.0.as_mut() {
            let _ = session.send(&NetMessage::Quit);
        }
        exit.write(AppExit::Success);
        return;
    }
    if !keys.just_pressed(KeyCode::KeyR) {
        return;
    }
    if matches!(setup.mode, Mode::Host | Mode::Join) {
        if let Some(session) = net.0.as_mut() {
            match setup.mode {
                Mode::Host if restart.phase == RestartPhase::Idle => {
                    restart.id = restart.id.wrapping_add(1);
                    let _ = session.send(&NetMessage::RestartPrepare {
                        restart_id: restart.id,
                    });
                    restart.phase = RestartPhase::HostWaitingReady;
                }
                Mode::Join if restart.phase == RestartPhase::Idle && !restart.request_sent => {
                    let _ = session.send(&NetMessage::RestartRequest);
                    restart.request_sent = true;
                }
                _ => {}
            }
        }
        return;
    }

    reset_match(
        &mut commands,
        &fighters,
        &setup,
        &mut game,
        &mut pending,
        &mut last_input_tick,
        &mut hashes,
        &mut pending_hashes,
        &mut desync,
        &mut over,
        &mut bot,
    );
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode = Mode::Local;
    let mut map_name = "arena".to_string();
    let mut port: u16 = 8080;
    let mut addr = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--vs-ai" => mode = Mode::VsAi,
            "--host" => {
                mode = Mode::Host;
                if let Some(p) = args.get(i + 1) {
                    if let Ok(v) = p.parse() {
                        port = v;
                        i += 1;
                    }
                }
            }
            "--join" => {
                mode = Mode::Join;
                if let Some(a) = args.get(i + 1) {
                    addr = a.clone();
                    i += 1;
                }
            }
            "--map" => {
                if let Some(m) = args.get(i + 1) {
                    map_name = m.clone();
                    i += 1;
                }
            }
            other => {
                eprintln!("liquidwar-rs: unknown argument {other:?}");
                eprintln!("usage: liquidwar-rs [--vs-ai] [--host PORT | --join ADDR] [--map arena|corridors|islands]");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // The simulation seed is fixed per map name, so matches (and online
    // restarts) replay identically.
    let seed = fnv_seed(&map_name);
    let duration_ticks = 64 * GAME_SECONDS;

    // Online handshake happens before the engine starts: it is blocking.
    let mut net_session: Option<NetSession> = None;
    let mut setup = MatchSetup {
        mode,
        map_name: map_name.clone(),
        seed,
        duration_ticks,
        army_size: ARMY_SIZE,
        cursors: Vec::new(),
    };

    match mode {
        Mode::Host => {
            let (_, starts) = map_with_starts(&map_name, seed);
            let teams = vec![
                TeamSetup {
                    team: 0,
                    cursor: starts[0],
                    army_size: ARMY_SIZE as u32,
                },
                TeamSetup {
                    team: 1,
                    cursor: starts[1],
                    army_size: ARMY_SIZE as u32,
                },
            ];
            let spec = GameSpec {
                map_name: map_name.clone(),
                seed,
                duration_ticks,
                teams,
            };
            let secret = online_password();
            let session = net::host_lobby(port, &spec, &secret)
                .unwrap_or_else(|e| panic!("liquidwar-rs: hosting failed: {e}"));
            setup.cursors = spec.teams.iter().map(|t| t.cursor).collect();
            net_session = Some(session);
        }
        Mode::Join => {
            if addr.is_empty() {
                eprintln!("liquidwar-rs: --join needs host:port");
                std::process::exit(2);
            }
            let secret = online_password();
            let (session, spec) = net::join_lobby(&addr, &secret)
                .unwrap_or_else(|e| panic!("liquidwar-rs: joining failed: {e}"));
            setup.map_name = spec.map_name.clone();
            setup.seed = spec.seed;
            setup.duration_ticks = spec.duration_ticks;
            setup.army_size = spec.army_size();
            setup.cursors = spec.teams.iter().map(|t| t.cursor).collect();
            net_session = Some(session);
        }
        _ => {
            let (_, starts) = map_with_starts(&map_name, seed);
            setup.cursors = vec![starts[0], starts[1]];
        }
    }

    println!(
        "liquidwar-rs: map={}, armies={} fighters, mode={:?}",
        setup.map_name, setup.army_size, setup.mode
    );
    println!("liquidwar-rs: mouse = your cursor, [R]estart, [Esc]uit");

    let game = build_game(&setup);

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "liquidwar-rs".to_string(),
                resolution: (1024.0_f32, 768.0_f32).into(),
                ..default()
            }),
            ..default()
        }))
        .insert_resource(ClearColor(Color::srgb(0.04, 0.05, 0.09)))
        .insert_resource(Time::<Fixed>::from_hz(TICK_HZ))
        .insert_resource(setup)
        .insert_resource(LwGame(game))
        .insert_resource(PendingInputs(Vec::new()))
        .insert_resource(LastInputTick::default())
        .insert_resource(LocalHashes(Vec::new()))
        .insert_resource(PendingHashes::default())
        .insert_resource(Desync(false))
        .insert_resource(OverRes(None))
        .insert_resource(NetOpt(net_session))
        .insert_resource(RestartState::default())
        .insert_resource(BotOpt(if mode == Mode::VsAi {
            Some(Bot::new(1, seed ^ 0xB0B_5EED))
        } else {
            None
        }))
        .add_systems(Startup, setup_world)
        .add_systems(FixedUpdate, fixed_simulate)
        .add_systems(Update, (global_keys, update_sprites).chain())
        .run();
}

fn online_password() -> String {
    std::env::var("LIQUIDWAR_PASSWORD").unwrap_or_else(|_| {
        eprintln!(
            "liquidwar-rs: online play requires LIQUIDWAR_PASSWORD, shared with your opponent"
        );
        std::process::exit(2);
    })
}

/// A stable seed per map name (FNV-1a), so every match of a given map
/// starts from the same (deterministic) world.
fn fnv_seed(name: &str) -> u64 {
    lwcore::hash::fnv1a64(name.as_bytes()) | 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::test_sessions;
    use bevy::ecs::system::RunSystemOnce;
    use std::thread::sleep;
    use std::time::Duration;

    /// Loopback sessions plus the resource state `drain_net` mutates.
    struct Drain {
        session: NetSession,
        pending: PendingInputs,
        hashes: LocalHashes,
        pending_hashes: PendingHashes,
        desync: Desync,
        restart: RestartState,
        exits: Vec<AppExit>,
    }

    impl Drain {
        fn new(session: NetSession) -> Self {
            Self {
                session,
                pending: PendingInputs(vec![]),
                hashes: LocalHashes(vec![]),
                pending_hashes: PendingHashes::default(),
                desync: Desync(false),
                restart: RestartState::default(),
                exits: vec![],
            }
        }

        /// Returns `false` when the session asked the app to exit.
        fn drain(&mut self, tick: u64) -> bool {
            match drain_net(
                &mut self.session,
                &mut self.pending,
                &mut self.hashes,
                &mut self.pending_hashes,
                &mut self.desync,
                &mut self.restart,
                tick,
            ) {
                NetFlow::Continue => true,
                NetFlow::Exit => {
                    self.exits.push(AppExit::Success);
                    false
                }
            }
        }

        /// Drain repeatedly until `done` holds; loopback delivery is fast but
        /// not instantaneous, so retry instead of assuming one pass suffices.
        fn until(&mut self, tick: u64, done: impl Fn(&Self) -> bool) {
            for _ in 0..250 {
                self.drain(tick);
                if done(self) {
                    return;
                }
                sleep(Duration::from_millis(2));
            }
            panic!("expected condition was not reached after draining");
        }
    }

    #[test]
    fn joiner_requested_restart_completes_on_both_sides() {
        let (host_session, joiner_session) = test_sessions();
        let mut host = Drain::new(host_session);
        let mut joiner = Drain::new(joiner_session);

        // Joiner presses R: global_keys sends the request.
        joiner.session.send(&NetMessage::RestartRequest).unwrap();

        // Host pauses and asks the joiner to acknowledge.
        host.until(0, |d| d.restart.phase == RestartPhase::HostWaitingReady);
        assert_eq!(host.restart.phase, RestartPhase::HostWaitingReady);
        assert_eq!(host.restart.id, 1);

        // Joiner pauses and answers Ready.
        joiner.until(0, |d| d.restart.phase == RestartPhase::JoinerPaused);
        assert_eq!(joiner.restart.id, 1);
        assert!(!joiner.restart.request_sent);

        // Host sees Ready and commits.
        host.until(0, |d| d.restart.host_commit);
        assert_eq!(host.restart.phase, RestartPhase::HostWaitingReady);

        // fixed_simulate: host sends Commit and resets.
        host.session
            .send(&NetMessage::RestartCommit {
                restart_id: host.restart.id,
            })
            .unwrap();
        host.restart.phase = RestartPhase::Idle;
        host.restart.host_commit = false;

        // Joiner sees Commit and resets.
        joiner.until(0, |d| d.restart.joiner_commit);
        joiner.restart.phase = RestartPhase::Idle;
        joiner.restart.joiner_commit = false;

        assert_eq!(host.restart.phase, RestartPhase::Idle);
        assert_eq!(joiner.restart.phase, RestartPhase::Idle);
        assert!(!host.desync.0);
        assert!(!joiner.desync.0);
        assert!(host.exits.is_empty());
        assert!(joiner.exits.is_empty());
    }

    #[test]
    fn host_initiated_restart_completes_on_both_sides() {
        let (host_session, joiner_session) = test_sessions();
        let mut host = Drain::new(host_session);
        let mut joiner = Drain::new(joiner_session);

        // Host presses R: global_keys sends Prepare directly.
        host.restart.id = 1;
        host.restart.phase = RestartPhase::HostWaitingReady;
        host.session
            .send(&NetMessage::RestartPrepare { restart_id: 1 })
            .unwrap();

        joiner.until(0, |d| d.restart.phase == RestartPhase::JoinerPaused);
        assert_eq!(joiner.restart.id, 1);

        host.until(0, |d| d.restart.host_commit);
        host.session
            .send(&NetMessage::RestartCommit {
                restart_id: host.restart.id,
            })
            .unwrap();
        host.restart.phase = RestartPhase::Idle;
        host.restart.host_commit = false;

        joiner.until(0, |d| d.restart.joiner_commit);
        joiner.restart.phase = RestartPhase::Idle;
        joiner.restart.joiner_commit = false;

        assert_eq!(host.restart.phase, RestartPhase::Idle);
        assert_eq!(joiner.restart.phase, RestartPhase::Idle);
    }

    #[test]
    fn hash_checks_are_ignored_during_a_restart() {
        let (host_session, joiner_session) = test_sessions();
        let mut host = Drain::new(host_session);
        let mut joiner = Drain::new(joiner_session);

        joiner.hashes.0.resize(7, 0);
        // A mismatching hash arriving while the joiner is paused is not a desync.
        joiner.restart.phase = RestartPhase::JoinerPaused;
        host.send_and_drain(&mut joiner, NetMessage::HashCheck { tick: 5, hash: 999 }, 0);
        assert!(!joiner.desync.0);

        // The same mismatch while idle is flagged.
        joiner.restart.phase = RestartPhase::Idle;
        host.send_and_drain(&mut joiner, NetMessage::HashCheck { tick: 6, hash: 999 }, 0);
        assert!(joiner.desync.0);
    }

    #[test]
    fn restart_messages_with_a_stale_id_are_ignored() {
        let (host_session, joiner_session) = test_sessions();
        let mut host = Drain::new(host_session);
        let mut joiner = Drain::new(joiner_session);

        host.restart.id = 2;
        host.restart.phase = RestartPhase::HostWaitingReady;

        // Ready for an older restart must not commit. Drain for a grace
        // period so the message has time to arrive and be processed.
        joiner
            .session
            .send(&NetMessage::RestartReady { restart_id: 1 })
            .unwrap();
        for _ in 0..50 {
            host.drain(0);
            sleep(Duration::from_millis(2));
        }
        assert!(!host.restart.host_commit);
        assert_eq!(host.restart.phase, RestartPhase::HostWaitingReady);

        // Ready for the current restart commits.
        joiner
            .session
            .send(&NetMessage::RestartReady { restart_id: 2 })
            .unwrap();
        host.until(0, |d| d.restart.host_commit);
        assert!(host.restart.host_commit);
    }

    #[test]
    fn inputs_are_windowed_and_deduped_per_team() {
        let (host_session, joiner_session) = test_sessions();
        let mut host = Drain::new(host_session);
        let mut joiner = Drain::new(joiner_session);
        let tick = 100;
        let due = tick + INPUT_DELAY_TICKS;

        joiner
            .session
            .send(&NetMessage::Input {
                tick: due,
                team: 1,
                mv: Move::Step { dx: 1, dy: 0 },
            })
            .unwrap();
        // Duplicate for the same tick and team.
        joiner
            .session
            .send(&NetMessage::Input {
                tick: due,
                team: 1,
                mv: Move::Step { dx: -1, dy: 0 },
            })
            .unwrap();
        // Too far in the future.
        joiner
            .session
            .send(&NetMessage::Input {
                tick: tick + INPUT_WINDOW_TICKS + 1,
                team: 1,
                mv: Move::Step { dx: 1, dy: 0 },
            })
            .unwrap();
        // In the past.
        joiner
            .session
            .send(&NetMessage::Input {
                tick: 99,
                team: 1,
                mv: Move::Step { dx: 1, dy: 0 },
            })
            .unwrap();
        // Same tick, other team: not a duplicate.
        joiner
            .session
            .send(&NetMessage::Input {
                tick: due,
                team: 0,
                mv: Move::Set { x: 5, y: 5 },
            })
            .unwrap();

        host.until(tick, |d| d.pending.0.len() == 2);
        assert_eq!(host.pending.0.len(), 2);
        assert_eq!(host.pending.0[0].0, due);
        assert_eq!(
            host.pending.0[0].1,
            Command::MoveCursor {
                team: 1,
                dx: 1,
                dy: 0
            }
        );
        assert_eq!(host.pending.0[1].0, due);
        assert_eq!(
            host.pending.0[1].1,
            Command::SetCursor {
                team: 0,
                x: 5,
                y: 5
            }
        );
    }

    #[test]
    fn quit_message_ends_the_session() {
        let (host_session, joiner_session) = test_sessions();
        let mut host = Drain::new(host_session);
        let mut joiner = Drain::new(joiner_session);

        joiner.session.send(&NetMessage::Quit).unwrap();
        let mut ended = false;
        for _ in 0..250 {
            if !host.drain(0) {
                ended = true;
                break;
            }
            sleep(Duration::from_millis(2));
        }
        assert!(ended);
        assert!(!host.exits.is_empty());
    }

    impl Drain {
        /// Send from this side, then keep draining the other side for a
        /// bounded grace period so the message is reliably consumed.
        fn send_and_drain(&mut self, other: &mut Drain, msg: NetMessage, tick: u64) {
            self.session.send(&msg).unwrap();
            for _ in 0..50 {
                other.drain(tick);
                sleep(Duration::from_millis(2));
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // Bevy system parameters
    fn reset_system(
        fighters: Query<Entity, With<FighterTag>>,
        setup: Res<MatchSetup>,
        mut game: ResMut<LwGame>,
        mut pending: ResMut<PendingInputs>,
        mut last_input_tick: ResMut<LastInputTick>,
        mut hashes: ResMut<LocalHashes>,
        mut pending_hashes: ResMut<PendingHashes>,
        mut desync: ResMut<Desync>,
        mut over: ResMut<OverRes>,
        mut bot: ResMut<BotOpt>,
        mut commands: Commands,
    ) {
        reset_match(
            &mut commands,
            &fighters,
            &setup,
            &mut game,
            &mut pending,
            &mut last_input_tick,
            &mut hashes,
            &mut pending_hashes,
            &mut desync,
            &mut over,
            &mut bot,
        );
    }

    fn restart_app(session: NetSession, mode: Mode) -> App {
        let setup = MatchSetup {
            mode,
            map_name: "arena".to_string(),
            seed: 42,
            duration_ticks: 10_000,
            army_size: 4,
            cursors: vec![(10, 72), (182, 72)],
        };
        let mut game = build_game(&setup);
        game.tick = 1000;

        let mut app = App::new();
        app.add_event::<AppExit>()
            .insert_resource(setup)
            .insert_resource(LwGame(game))
            .insert_resource(PendingInputs(Vec::new()))
            .insert_resource(LastInputTick::default())
            .insert_resource(LocalHashes(Vec::new()))
            .insert_resource(PendingHashes::default())
            .insert_resource(Desync(false))
            .insert_resource(OverRes(None))
            .insert_resource(NetOpt(Some(session)))
            .insert_resource(RestartState::default())
            .insert_resource(ButtonInput::<KeyCode>::default())
            .insert_resource(BotOpt(None))
            .add_systems(FixedUpdate, fixed_simulate);
        app
    }

    #[test]
    fn online_restart_resets_and_resumes_both_fixed_update_apps() {
        let (host_session, joiner_session) = test_sessions();
        let mut host = restart_app(host_session, Mode::Host);
        let mut joiner = restart_app(joiner_session, Mode::Join);

        joiner
            .world_mut()
            .resource_mut::<NetOpt>()
            .0
            .as_mut()
            .unwrap()
            .send(&NetMessage::RestartRequest)
            .unwrap();

        let mut completed = false;
        for _ in 0..1000 {
            host.world_mut().run_schedule(FixedUpdate);
            joiner.world_mut().run_schedule(FixedUpdate);
            let host_game = &host.world().resource::<LwGame>().0;
            let joiner_game = &joiner.world().resource::<LwGame>().0;
            if host_game.tick >= 16
                && host_game.tick == joiner_game.tick
                && host_game.state_hash() == joiner_game.state_hash()
                && host.world().resource::<RestartState>().phase == RestartPhase::Idle
                && joiner.world().resource::<RestartState>().phase == RestartPhase::Idle
            {
                completed = true;
                break;
            }
            sleep(Duration::from_millis(1));
        }

        assert!(
            completed,
            "both peers should restart and resume in lockstep"
        );
        assert!(host.world().resource::<LwGame>().0.tick < 1000);
        assert!(joiner.world().resource::<LwGame>().0.tick < 1000);
        assert!(!host.world().resource::<Desync>().0);
        assert!(!joiner.world().resource::<Desync>().0);
    }

    #[test]
    fn reset_match_clears_state_and_respawns_fighters() {
        let mut world = World::new();
        let setup = MatchSetup {
            mode: Mode::Host,
            map_name: "arena".to_string(),
            seed: 42,
            duration_ticks: 1000,
            army_size: 8,
            cursors: vec![(10, 72), (182, 72)],
        };
        world.insert_resource(LwGame(build_game(&setup)));
        world.insert_resource(setup);
        world.insert_resource(PendingInputs(vec![(
            7,
            Command::MoveCursor {
                team: 1,
                dx: 1,
                dy: 0,
            },
        )]));
        world.insert_resource(LastInputTick(Some(7)));
        world.insert_resource(LocalHashes(vec![1, 2, 3]));
        world.insert_resource(PendingHashes(vec![(7, 9)]));
        world.insert_resource(Desync(true));
        world.insert_resource(OverRes(Some(Scoreboard { counts: vec![1, 1] })));
        world.insert_resource(BotOpt(None));
        world.spawn(FighterTag { id: usize::MAX });

        world.run_system_once(reset_system).unwrap();

        assert!(world.resource::<PendingInputs>().0.is_empty());
        assert_eq!(world.resource::<LastInputTick>().0, None);
        assert!(world.resource::<LocalHashes>().0.is_empty());
        assert!(world.resource::<PendingHashes>().0.is_empty());
        assert!(!world.resource::<Desync>().0);
        assert!(world.resource::<OverRes>().0.is_none());
        // Two teams of eight fighters on a fresh game.
        assert_eq!(world.resource::<LwGame>().0.total_fighters(), 16);
        let mut fighters = world.query_filtered::<Entity, With<FighterTag>>();
        assert_eq!(fighters.iter(&world).count(), 16);
    }
}
