//! The player panel.
//!
//! Everything is built once at startup and only updated afterwards, so no UI
//! entity is created or destroyed during play.
//!
//! Markers are enums queried as a group rather than one marker component per
//! widget. Two `Query<&mut Text>` in one system conflict unless Bevy can prove
//! their filters disjoint, which it cannot do for two unrelated marker types --
//! so a single query over `(&HudText, &mut Text)` and a `match` is both simpler
//! and the thing that actually works.

use bevy::prelude::*;
use orewar_shared::math::Vec2 as SimVec2;
use orewar_shared::protocol::{GameStatus, VehicleSlot};
use orewar_shared::sim::{self, VehicleKind};
use orewar_shared::world::{MAX_PLAYERS, PowerUp};

use crate::coords;
use crate::input::LocalInput;
use crate::net::{Link, NetClient};
use crate::state::GameState;

const PANEL_BG: Color = Color::srgba(0.03, 0.05, 0.07, 0.72);
/// Shared with [`crate::menu`], so the menu and the HUD read as one interface.
pub const TEXT: Color = Color::srgb(0.90, 0.94, 0.97);
pub const TEXT_DIM: Color = Color::srgb(0.62, 0.69, 0.75);
const SHIELD_COLOR: Color = Color::srgb(0.35, 0.72, 1.0);
const HULL_COLOR: Color = Color::srgb(0.95, 0.44, 0.30);
const CARGO_COLOR: Color = Color::srgb(0.95, 0.74, 0.22);
const CAPTURE_COLOR: Color = Color::srgb(1.0, 0.85, 0.25);

const RADAR_SIZE: f32 = 156.0;
/// World units from edge to edge of the radar display.
const RADAR_RANGE: f32 = 150.0;

/// Text widgets, identified rather than separately marked.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum HudText {
    Status,
    Log,
    Banner,
    BuildMenu,
    TankStats,
    MinerStats,
    Roster(u8),
}

/// Bar fills, sized by setting their width each frame.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum HudBar {
    TankShield,
    TankHull,
    MinerShield,
    MinerHull,
    MinerCargo,
    Capture,
}

/// Panels shown and hidden as a whole.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum HudPanel {
    BuildMenu,
    Radar,
    Banner,
}

#[derive(Component)]
pub struct RadarBlip(pub usize);

fn font(size: f32) -> TextFont {
    TextFont { font_size: FontSize::Px(size), ..default() }
}

fn panel_node() -> Node {
    Node {
        position_type: PositionType::Absolute,
        flex_direction: FlexDirection::Column,
        padding: UiRect::all(px(10)),
        row_gap: px(4),
        border_radius: BorderRadius::all(px(6)),
        ..default()
    }
}

/// Spawns a labelled bar and tags its fill so it can be resized later.
fn spawn_bar(parent: &mut ChildSpawnerCommands, bar: HudBar, color: Color, width: f32) {
    parent
        .spawn((
            Node {
                width: px(width),
                height: px(9),
                margin: UiRect { top: px(2), ..default() },
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.12)),
        ))
        .with_children(|track| {
            track.spawn((
                bar,
                Node {
                    width: percent(100),
                    height: percent(100),
                    border_radius: BorderRadius::all(px(3)),
                    ..default()
                },
                BackgroundColor(color),
            ));
        });
}

pub fn setup(mut commands: Commands) {
    // ---- Status, top left -------------------------------------------------
    commands
        .spawn((
            Node { top: px(12), left: px(12), min_width: px(210), ..panel_node() },
            BackgroundColor(PANEL_BG),
        ))
        .with_children(|p| {
            p.spawn((HudText::Status, Text::new("Connecting..."), font(14.0), TextColor(TEXT)));
        });

    // ---- Roster, top right ------------------------------------------------
    commands
        .spawn((
            Node { top: px(12), right: px(12), min_width: px(230), ..panel_node() },
            BackgroundColor(PANEL_BG),
        ))
        .with_children(|p| {
            p.spawn((Text::new("PLAYERS"), font(11.0), TextColor(TEXT_DIM)));
            for id in 0..MAX_PLAYERS as u8 {
                p.spawn((
                    Node {
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        column_gap: px(7),
                        ..default()
                    },
                    // Each row wears its player's colour permanently, so the
                    // vehicles on the field and the names here always agree.
                    children![
                        (
                            Node {
                                width: px(11),
                                height: px(11),
                                border_radius: BorderRadius::MAX,
                                ..default()
                            },
                            BackgroundColor(coords::player_color(id)),
                        ),
                        (HudText::Roster(id), Text::new("--"), font(13.0), TextColor(TEXT)),
                    ],
                ));
            }
        });

    // ---- Tank panel, bottom left -----------------------------------------
    commands
        .spawn((
            Node { bottom: px(12), left: px(12), width: px(230), ..panel_node() },
            BackgroundColor(PANEL_BG),
        ))
        .with_children(|p| {
            p.spawn((Text::new("TANK"), font(12.0), TextColor(TEXT_DIM)));
            p.spawn((HudText::TankStats, Text::new(""), font(13.0), TextColor(TEXT)));
            spawn_bar(p, HudBar::TankShield, SHIELD_COLOR, 210.0);
            spawn_bar(p, HudBar::TankHull, HULL_COLOR, 210.0);
        });

    // ---- Miner panel, bottom right ------------------------------------
    commands
        .spawn((
            Node { bottom: px(12), right: px(12), width: px(230), ..panel_node() },
            BackgroundColor(PANEL_BG),
        ))
        .with_children(|p| {
            p.spawn((Text::new("MINER"), font(12.0), TextColor(TEXT_DIM)));
            p.spawn((HudText::MinerStats, Text::new(""), font(13.0), TextColor(TEXT)));
            spawn_bar(p, HudBar::MinerShield, SHIELD_COLOR, 210.0);
            spawn_bar(p, HudBar::MinerHull, HULL_COLOR, 210.0);
            spawn_bar(p, HudBar::MinerCargo, CARGO_COLOR, 210.0);
            spawn_bar(p, HudBar::Capture, CAPTURE_COLOR, 210.0);
        });

    // ---- Event log, bottom centre ----------------------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: px(104),
                width: percent(100),
                justify_content: JustifyContent::Center,
                ..default()
            },
            children![(
                HudText::Log,
                Text::new(""),
                font(13.0),
                TextLayout::justify(Justify::Center),
                TextColor(Color::srgb(0.85, 0.90, 0.95)),
            )],
        ));

    // ---- Crosshair --------------------------------------------------------
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: percent(50),
            top: percent(50),
            width: px(6),
            height: px(6),
            margin: UiRect { left: px(-3), top: px(-3), ..default() },
            border_radius: BorderRadius::MAX,
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.65)),
    ));

    // ---- Radar, top centre (hidden until the power-up is owned) -----------
    commands
        .spawn((
            HudPanel::Radar,
            Node {
                position_type: PositionType::Absolute,
                top: px(12),
                left: percent(50),
                width: px(RADAR_SIZE),
                height: px(RADAR_SIZE),
                margin: UiRect { left: px(-RADAR_SIZE / 2.0), ..default() },
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.12, 0.06, 0.62)),
            BorderColor::all(Color::srgba(0.35, 0.85, 0.45, 0.5)),
            Visibility::Hidden,
        ))
        .with_children(|p| {
            // One blip per possible vehicle, plus one for the viewer.
            for i in 0..MAX_PLAYERS * 2 + 1 {
                p.spawn((
                    RadarBlip(i),
                    Node {
                        position_type: PositionType::Absolute,
                        width: px(7),
                        height: px(7),
                        border_radius: BorderRadius::MAX,
                        ..default()
                    },
                    BackgroundColor(Color::WHITE),
                    Visibility::Hidden,
                ));
            }
        });

    // ---- Build menu -------------------------------------------------------
    commands
        .spawn((
            HudPanel::BuildMenu,
            Node {
                position_type: PositionType::Absolute,
                left: percent(50),
                top: percent(50),
                // Half its own size, to centre it on the screen: the panel
                // grew a row taller when the seventh upgrade was added.
                width: px(430),
                margin: UiRect { left: px(-215), top: px(-180), ..default() },
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(16)),
                row_gap: px(6),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.04, 0.06, 0.09, 0.94)),
            BorderColor::all(Color::srgba(0.5, 0.6, 0.7, 0.55)),
            Visibility::Hidden,
            children![
                (Text::new("BUILD  --  press a number to buy, B to close"), font(13.0), TextColor(TEXT_DIM)),
                (HudText::BuildMenu, Text::new(""), font(14.0), TextColor(TEXT)),
            ],
        ));

    // ---- Banner -----------------------------------------------------------
    commands.spawn((
        HudPanel::Banner,
        Node {
            position_type: PositionType::Absolute,
            top: percent(38),
            width: percent(100),
            justify_content: JustifyContent::Center,
            ..default()
        },
        Visibility::Hidden,
        children![(
            HudText::Banner,
            Text::new(""),
            font(34.0),
            TextLayout::justify(Justify::Center),
            TextColor(Color::srgb(1.0, 0.95, 0.7)),
        )],
    ));
}

pub fn update_texts(
    state: Res<GameState>,
    net: Res<NetClient>,
    input: Res<LocalInput>,
    mut texts: Query<(&HudText, &mut Text)>,
) {
    let me = state.local();
    let credits = me.map_or(0, |p| p.credits);

    for (id, mut text) in &mut texts {
        match id {
            HudText::Status => {
                let link = match &net.link {
                    Link::Connecting => format!("connecting to {}", net.server_addr_text),
                    Link::Connected => {
                        format!("{:.0} ms  |  {:.0}% loss", net.rtt_ms(), net.packet_loss() * 100.0)
                    }
                    Link::Denied(reason) => format!("refused: {}", reason.describe()),
                };
                let name = state.local_player.map(|id| state.name_of(id)).unwrap_or_default();
                // The sortie line is only worth the room once the aircraft is
                // bought: for most of most matches there is nothing to say.
                let cheats = state.cheats();
                let sortie = match me {
                    Some(p) if PowerUp::Bomber.held(p.powerups) => {
                        if let Some(plane) = p.plane {
                            // Airspeed alongside the fuel: the throttle is a
                            // slow, small change and without a number on it the
                            // player cannot tell it is doing anything.
                            let fuel = if cheats {
                                "fuel unlimited".to_string()
                            } else {
                                format!("{:.0}s of fuel", plane.cargo * sim::PLANE_FUEL)
                            };
                            format!("\nsortie: {fuel}   {:.0} kts", plane.speed)
                        } else if p.plane_ready_in > 0 {
                            format!("\nsortie: ready in {}s", p.plane_ready_in)
                        } else {
                            "\nsortie: ready  [G]".to_string()
                        }
                    }
                    _ => String::new(),
                };
                let cheat_line = if cheats { "\nCHEATS ON  [Alt-C]" } else { "" };
                let weapons = if input.controlling == VehicleSlot::Plane {
                    "WASD fly | LMB drop bombs"
                } else {
                    "WASD drive | mouse aim | LMB gun | RMB missile"
                };
                **text = format!(
                    "OREWAR  {name}\n{link}\nore {credits}   mined {}\ndriving: {}{sortie}{cheat_line}\n\n\
                     {weapons}\n\
                     TAB swap vehicle | B build | O overview | ESC menu",
                    me.map_or(0, |p| p.ore_mined),
                    match input.controlling {
                        VehicleSlot::Tank => "TANK",
                        VehicleSlot::Miner => "MINER",
                        VehicleSlot::Plane => "BOMBER",
                    }
                );
            }

            HudText::TankStats => {
                **text = match me.and_then(|p| p.tank) {
                    Some(v) => {
                        let powerups = me.map_or(0, |p| p.powerups);
                        format!(
                            "shield {:.0}/{:.0}   hull {:.0}/{:.0}\nmissiles {}",
                            v.shield,
                            sim::max_shield(VehicleKind::Tank, powerups),
                            v.hull,
                            sim::max_hull(VehicleKind::Tank, powerups),
                            me.map_or(0, |p| p.missiles)
                        )
                    }
                    None => "destroyed -- respawning".to_string(),
                };
            }

            HudText::MinerStats => {
                **text = match me.and_then(|p| p.miner) {
                    Some(v) => {
                        let powerups = me.map_or(0, |p| p.powerups);
                        let mut s = format!(
                            "shield {:.0}/{:.0}   hull {:.0}/{:.0}\ncargo {:.0}/{:.0}",
                            v.shield,
                            sim::max_shield(VehicleKind::Miner, powerups),
                            v.hull,
                            sim::max_hull(VehicleKind::Miner, powerups),
                            v.cargo,
                            sim::cargo_capacity(powerups),
                        );
                        if v.disabled {
                            s.push_str(&format!(
                                "\nDISABLED -- capture {:.0}%",
                                v.capture_progress * 100.0
                            ));
                        }
                        s
                    }
                    None => "captured".to_string(),
                };
            }

            HudText::Roster(id) => {
                **text = match state.render.players.get(*id as usize).and_then(|p| p.as_ref()) {
                    Some(player) => {
                        let mut tags = String::new();
                        for powerup in PowerUp::ALL {
                            if powerup.held(player.powerups) {
                                tags.push(powerup.name().chars().next().unwrap_or('?'));
                            }
                        }
                        format!(
                            "{}{}  ore {}  caps {}{}",
                            state.name_of(*id),
                            if player.connected { "" } else { " (offline)" },
                            player.ore_mined,
                            player.captures,
                            if tags.is_empty() {
                                String::new()
                            } else {
                                format!("  [{tags}]")
                            }
                        ) + if player.eliminated { "  OUT" } else { "" }
                    }
                    None => "--".to_string(),
                };
            }

            HudText::Log => {
                **text = state.log.iter().cloned().collect::<Vec<_>>().join("\n");
            }

            HudText::BuildMenu => {
                let powerups = me.map_or(0, |p| p.powerups);
                let mut lines = Vec::new();
                for (i, powerup) in PowerUp::ALL.iter().enumerate() {
                    let owned = powerup.held(powerups);
                    let status = if owned {
                        "OWNED".to_string()
                    } else if state.cheats() {
                        "free".to_string()
                    } else if credits >= powerup.cost() {
                        format!("{} ore", powerup.cost())
                    } else {
                        format!("{} ore (short {})", powerup.cost(), powerup.cost() - credits)
                    };
                    lines.push(format!(
                        "{}. {:<17} {:<22} {}",
                        i + 1,
                        powerup.name(),
                        status,
                        powerup.description()
                    ));
                }
                **text = lines.join("\n");
            }

            HudText::Banner => {
                let coming_back = me.map_or(0, |p| p.respawn_in);
                **text = match (state.status, state.winner) {
                    (GameStatus::Waiting, _) => "WAITING FOR ANOTHER PLAYER".to_string(),
                    // Losing a miner is the one thing that takes you off the
                    // field entirely, so while that clock runs it is the only
                    // thing worth saying.
                    (GameStatus::Running, _) if coming_back > 0 => {
                        format!("MINER LOST\nBACK IN {coming_back}")
                    }
                    (GameStatus::Finished, Some(winner)) => {
                        if Some(winner) == state.local_player {
                            "YOU WIN".to_string()
                        } else {
                            format!("{} WINS", state.name_of(winner).to_uppercase())
                        }
                    }
                    _ => String::new(),
                };
            }
        }
    }
}

pub fn update_bars(state: Res<GameState>, mut bars: Query<(&HudBar, &mut Node)>) {
    let me = state.local();
    let powerups = me.map_or(0, |p| p.powerups);

    for (bar, mut node) in &mut bars {
        let fraction = match bar {
            HudBar::TankShield => me.and_then(|p| p.tank).map(|v| {
                v.shield / sim::max_shield(VehicleKind::Tank, powerups).max(1.0)
            }),
            HudBar::TankHull => me
                .and_then(|p| p.tank)
                .map(|v| v.hull / sim::max_hull(VehicleKind::Tank, powerups).max(1.0)),
            HudBar::MinerShield => me.and_then(|p| p.miner).map(|v| {
                v.shield / sim::max_shield(VehicleKind::Miner, powerups).max(1.0)
            }),
            HudBar::MinerHull => me.and_then(|p| p.miner).map(|v| {
                v.hull / sim::max_hull(VehicleKind::Miner, powerups).max(1.0)
            }),
            HudBar::MinerCargo => me
                .and_then(|p| p.miner)
                .map(|v| v.cargo / sim::cargo_capacity(powerups).max(1.0)),
            HudBar::Capture => me
                .and_then(|p| p.miner)
                .map(|v| if v.disabled { v.capture_progress } else { 0.0 }),
        };
        node.width = percent(fraction.unwrap_or(0.0).clamp(0.0, 1.0) * 100.0);
    }
}

pub fn update_panels(
    state: Res<GameState>,
    input: Res<LocalInput>,
    mut panels: Query<(&HudPanel, &mut Visibility), Without<RadarBlip>>,
) {
    let radar_unlocked = state.local().is_some_and(|p| PowerUp::Radar.held(p.powerups));
    let banner = state.status != GameStatus::Running;

    for (panel, mut visibility) in &mut panels {
        let show = match panel {
            HudPanel::BuildMenu => input.build_menu,
            HudPanel::Radar => radar_unlocked,
            HudPanel::Banner => banner,
        };
        *visibility = if show { Visibility::Visible } else { Visibility::Hidden };
    }
}

/// Places contacts on the radar, oriented so the viewer always faces up.
pub fn update_radar(
    state: Res<GameState>,
    input: Res<LocalInput>,
    mut blips: Query<(&RadarBlip, &mut Node, &mut Visibility, &mut BackgroundColor)>,
) {
    let me = state.local();
    let unlocked = me.is_some_and(|p| PowerUp::Radar.held(p.powerups));

    // The reference frame: where the viewer is and which way they face.
    let viewer = me
        .and_then(|p| {
            let slot = if p.vehicle(input.controlling).is_some() {
                input.controlling
            } else {
                input.controlling.next_available(|s| p.vehicle(s).is_some())
            };
            p.vehicle(slot)
        })
        .map(|v| (v.pos, v.yaw));

    // Flatten every vehicle on the field into a list of contacts.
    let mut contacts: Vec<(SimVec2, Color, bool)> = Vec::new();
    for player in state.render.players.iter().flatten() {
        let color = coords::player_color(player.id);
        let is_me = Some(player.id) == state.local_player;
        if let Some(v) = player.tank {
            contacts.push((v.pos, color, is_me));
        }
        if let Some(v) = player.miner {
            contacts.push((v.pos, color, is_me));
        }
    }

    let half = RADAR_SIZE / 2.0;
    let scale = (RADAR_SIZE / 2.0 - 8.0) / (RADAR_RANGE / 2.0);

    for (blip, mut node, mut visibility, mut background) in &mut blips {
        let Some((origin, yaw)) = viewer.filter(|_| unlocked) else {
            *visibility = Visibility::Hidden;
            continue;
        };
        let Some(&(pos, color, is_me)) = contacts.get(blip.0) else {
            *visibility = Visibility::Hidden;
            continue;
        };

        let offset = pos - origin;
        let heading = SimVec2::from_angle(yaw);
        // Rotate into the viewer's frame: forward is up the screen, and screen
        // Y grows downward, so the forward component is negated.
        let forward = offset.dot(heading);
        // In simulation space `perp` is the vehicle's right; see
        // `coords::sim_perp_is_screen_right`.
        let right = offset.dot(heading.perp());
        let x = right * scale;
        let y = -forward * scale;

        if x.hypot(y) > half - 6.0 {
            *visibility = Visibility::Hidden;
            continue;
        }

        let size = if is_me { 9.0 } else { 7.0 };
        node.width = px(size);
        node.height = px(size);
        node.left = px(half + x - size / 2.0);
        node.top = px(half + y - size / 2.0);
        *background = BackgroundColor(if is_me { Color::WHITE } else { color });
        *visibility = Visibility::Visible;
    }
}
