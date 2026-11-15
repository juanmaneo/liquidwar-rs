//! liquidwar-rs — a Rust port of Liquid War, Bevy 0.16 frontend.
//!
//! Usage:
//!   liquidwar-rs                 two players on one machine
//!   liquidwar-rs --vs-ai         player vs simple bot
//!   liquidwar-rs --host 8080     host an online match on port 8080
//!   liquidwar-rs --join host:8080  join an online match
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
use lwcore::proto::{Move, NetMessage, TeamSetup, PROTO_VERSION};


mod bot;
mod net;

use bot::Bot;
use net::{GameSpec, NetSession, INPUT_DELAY_TICKS};

const TICK_HZ: f64 = 64.0;
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

/// State fingerprint per completed tick (for desync checks).
#[derive(Resource)]
struct LocalHashes(Vec<u64>);

#[derive(Resource)]
struct Desync(bool);

#[derive(Resource)]
struct OverRes(Option<Scoreboard>);

#[derive(Resource)]
struct NetOpt(Option<NetSession>);

#[derive(Resource)]
struct BotOpt(Option<Bot>);

/// Fighter sprite entities, in the same order as `game.fighters()`.
#[derive(Resource)]
struct FighterSprites(Vec<Entity>);

#[derive(Component)]
struct FighterTag;

#[derive(Component)]
struct CursorTag;

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
        _ => (
            Map::arena(),
            vec![(10, 72), (182, 72), (96, 10), (96, 134)],
        ),
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

fn setup_world(
    mut commands: Commands,
    game: Res<LwGame>,
    mut images: ResMut<Assets<Image>>,
    mut sprites: ResMut<FighterSprites>,
) {
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
        let e = commands
            .spawn((
                Sprite::from_color(team_color(f.team), Vec2::splat(CELL * 0.9)),
                Transform::from_translation(cell_to_world(map.height, f.x, f.y, 1.0)),
                FighterTag,
            ))
            .id();
        sprites.0.push(e);
    }

    // Team cursors: bright blocks above everything but the text.
    for t in 0..game.0.num_teams() {
        let (x, y) = game.0.cursor(t as u8);
        commands.spawn((
            Sprite::from_color(team_color(t as u8), Vec2::splat(CELL * 1.7)),
            Transform::from_translation(cell_to_world(map.height, x, y, 2.0)),
            CursorTag,
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

fn fixed_simulate(
    mut game: ResMut<LwGame>,
    setup: Res<MatchSetup>,
    mut pending: ResMut<PendingInputs>,
    mut net: ResMut<NetOpt>,
    mut hashes: ResMut<LocalHashes>,
    mut desync: ResMut<Desync>,
    mut over: ResMut<OverRes>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut bot: ResMut<BotOpt>,
    mut exit: EventWriter<AppExit>,
) {
    if desync.0 || over.0.is_some() {
        return;
    }
    let tick = game.0.tick;
    let net_mode = matches!(setup.mode, Mode::Host | Mode::Join);
    let delay = if net_mode { INPUT_DELAY_TICKS } else { 0 };

    // 1) Drain the network: queue inputs, verify hash checks.
    if let Some(session) = net.0.as_mut() {
        let mut err = false;
        loop {
            match session.try_recv() {
                Ok(None) => break,
                Ok(Some(NetMessage::Input { tick: t, team, mv })) => {
                    pending.0.push((t, mv.to_command(team)));
                }
                Ok(Some(NetMessage::HashCheck { tick: t, hash })) => {
                    if hashes.0.get(t as usize).copied() != Some(hash) {
                        desync.0 = true;
                        eprintln!("liquidwar-rs: DESYNC detected at tick {t}");
                    }
                }
                Ok(Some(NetMessage::Quit)) => {
                    println!("liquidwar-rs: opponent left the game");
                    exit.write(AppExit::Success);
                    return;
                }
                Ok(Some(other)) => {
                    eprintln!("liquidwar-rs: unexpected message {other:?}");
                }
                Err(_) => {
                    err = true;
                    break;
                }
            }
        }
        if err || session.closed {
            eprintln!("liquidwar-rs: connection lost");
            exit.write(AppExit::Success);
            return;
        }
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
    pending.0.push((tick + delay, cmd));
    if let Some(session) = net.0.as_mut() {
        let mv = Move::from_command(cmd);
        let _ = session.send_input(tick, mouse_team, mv);
    }
    if setup.mode == Mode::Local {
        if let Some((dx, dy)) = key_direction(&keys) {
            pending.0.push((
                tick,
                Command::MoveCursor {
                    team: 1,
                    dx,
                    dy,
                },
            ));
        }
    }

    // 3) Bot input (vs-ai): applied on the same tick, deterministic.
    if let Some(bot) = bot.0.as_mut() {
        pending.0.push((tick, bot.step(&game.0)));
    }

    // 4) Lockstep barrier: in net mode wait until the peer's input for
    //    this tick has arrived.
    if net_mode {
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
    if let Some(session) = net.0.as_mut() {
        let _ = session.send(&NetMessage::HashCheck { tick, hash: h });
    }

    if let Some(board) = game.0.game_over() {
        over.0 = Some(board);
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
    let x = (world_x / CELL)
        .floor()
        .clamp(0.0, (map.width - 1) as f32) as u32;
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
    mut fighters: Query<(&mut Transform, &mut Sprite), With<FighterTag>>,
    mut cursors: Query<&mut Transform, (With<CursorTag>, Without<FighterTag>)>,
    mut score: Query<&mut Text2d, With<ScoreTextTag>>,
) {
    let map_h = game.0.map.height;

    // Fighters: game order matches sprite spawn order.
    let mut it = fighters.iter_mut();
    for f in game.0.fighters() {
        let Some((mut tr, mut sp)) = it.next() else {
            break;
        };
        tr.translation = cell_to_world(map_h, f.x, f.y, 1.0);
        sp.color = team_color(f.team);
    }

    // Cursors.
    for (i, mut tr) in cursors.iter_mut().enumerate() {
        let (x, y) = game.0.cursor(i as u8);
        tr.translation = cell_to_world(map_h, x, y, 2.0);
    }

    // Score line.
    if let Ok(mut text) = score.single_mut() {
        let board = game.0.scoreboard();
        let counts: Vec<String> = (0..board.counts.len())
            .map(|t| format!("{} {}", team_name(t as u8), board.counts[t]))
            .collect();
        let secs_left = game
            .0
            .params
            .duration_ticks
            .saturating_sub(game.0.tick)
            / 64;
        let mut line = format!("{}   {}", counts.join("  "), secs_left);
        if desync.0 {
            line.push_str("   DESYNC!");
        } else if let Some(board) = over.0.as_ref() {
            match board.winner() {
                Some(w) => line.push_str(&format!("   {} WINS!  [R]estart  [Esc]uit", team_name(w))),
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

fn global_keys(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    setup: Res<MatchSetup>,
    mut game: ResMut<LwGame>,
    mut pending: ResMut<PendingInputs>,
    mut hashes: ResMut<LocalHashes>,
    mut desync: ResMut<Desync>,
    mut over: ResMut<OverRes>,
    mut bot: ResMut<BotOpt>,
    mut net: ResMut<NetOpt>,
    mut sprites: ResMut<FighterSprites>,
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
    // R: restart the match. Note: online restarts need both players to
    // press R (documented v1 limitation).
    for e in fighters.iter() {
        commands.entity(e).despawn();
    }
    sprites.0.clear();
    *game = LwGame(build_game(&setup));
    pending.0.clear();
    hashes.0.clear();
    desync.0 = false;
    over.0 = None;
    if let Some(b) = bot.0.as_mut() {
        *b = Bot::new(1, setup.seed ^ 0xB0B_5EED);
    }
    for f in game.0.fighters() {
        let e = commands
            .spawn((
                Sprite::from_color(team_color(f.team), Vec2::splat(CELL * 0.9)),
                Transform::from_translation(cell_to_world(game.0.map.height, f.x, f.y, 1.0)),
                FighterTag,
            ))
            .id();
        sprites.0.push(e);
    }
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
            let session = net::host_lobby(port, &spec)
                .unwrap_or_else(|e| panic!("liquidwar-rs: hosting failed: {e}"));
            setup.cursors = spec.teams.iter().map(|t| t.cursor).collect();
            net_session = Some(session);
        }
        Mode::Join => {
            if addr.is_empty() {
                eprintln!("liquidwar-rs: --join needs host:port");
                std::process::exit(2);
            }
            let (session, spec) = net::join_lobby(&addr)
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
        .insert_resource(LocalHashes(Vec::new()))
        .insert_resource(Desync(false))
        .insert_resource(OverRes(None))
        .insert_resource(NetOpt(net_session))
        .insert_resource(BotOpt(if mode == Mode::VsAi {
            Some(Bot::new(1, seed ^ 0xB0B_5EED))
        } else {
            None
        }))
        .insert_resource(FighterSprites(Vec::new()))
        .add_systems(Startup, setup_world)
        .add_systems(FixedUpdate, fixed_simulate)
        .add_systems(Update, (global_keys, update_sprites).chain())
        .run();
}

/// A stable seed per map name (FNV-1a), so every match of a given map
/// starts from the same (deterministic) world.
fn fnv_seed(name: &str) -> u64 {
    lwcore::hash::fnv1a64(name.as_bytes()) | 1
}

// Silence unused warnings for protocol items referenced only in net mode.
const _: u16 = PROTO_VERSION;
