//! Three buttons floating over your own miner, for telling it what to do
//! while you are off driving the tank.
//!
//! Bevy's UI is screen-space, so the panel is an ordinary absolutely-positioned
//! node whose offset is recomputed each frame by projecting the miner's
//! interpolated position through the camera. That keeps it pinned to the
//! vehicle without the buttons scaling or rotating with the world, which is
//! what you want of a control: it stays the same size and stays clickable.

use bevy::prelude::*;
use orewar_shared::protocol::MinerMode;

use crate::coords;
use crate::net::NetClient;
use crate::state::GameState;

/// The container. Moved every frame; hidden when there is nothing to attach to.
#[derive(Component)]
pub struct PanelRoot;

/// One of the three choices.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub struct ModeButton(pub MinerMode);

/// Order left to right, and the order they are spawned.
const MODES: [MinerMode; 3] =
    [MinerMode::Auto, MinerMode::Home, MinerMode::Stop];

const DOT: f32 = 20.0;

/// How high above the hull the panel floats, in world units.
const LIFT: f32 = 4.6;

fn color(mode: MinerMode) -> Color {
    match mode {
        MinerMode::Auto => Color::srgb(0.30, 0.82, 0.38),
        MinerMode::Home => Color::srgb(0.96, 0.76, 0.22),
        MinerMode::Stop => Color::srgb(0.88, 0.28, 0.26),
    }
}

pub fn setup(mut commands: Commands) {
    commands
        .spawn((
            PanelRoot,
            Node {
                position_type: PositionType::Absolute,
                display: Display::Flex,
                column_gap: px(6),
                padding: UiRect::all(px(5)),
                border_radius: BorderRadius::all(px(13)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.03, 0.05, 0.08, 0.62)),
            Visibility::Hidden,
        ))
        .with_children(|parent| {
            for mode in MODES {
                parent.spawn((
                    ModeButton(mode),
                    Button,
                    Node {
                        width: px(DOT),
                        height: px(DOT),
                        border: UiRect::all(px(2)),
                        border_radius: BorderRadius::all(px(DOT * 0.5)),
                        ..default()
                    },
                    BackgroundColor(color(mode)),
                    BorderColor::all(Color::NONE),
                ));
            }
        });
}

pub fn update(
    state: Res<GameState>,
    mut net: ResMut<NetClient>,
    cameras: Query<(&Camera, &GlobalTransform)>,
    mut root: Query<(&mut Node, &mut Visibility), With<PanelRoot>>,
    mut buttons: Query<
        (&ModeButton, &Interaction, &mut BackgroundColor, &mut BorderColor),
        Without<PanelRoot>,
    >,
) {
    let Ok((mut node, mut visibility)) = root.single_mut() else { return };

    // Only your own, and only while it is a miner rather than a wreck --
    // there is nothing to instruct a disabled one to do.
    let showing = state.local().and_then(|p| p.miner.map(|m| (m, p.miner_mode)));
    let Some((miner, mode)) = showing.filter(|(m, _)| !m.disabled) else {
        *visibility = Visibility::Hidden;
        return;
    };

    let Some(screen) = cameras.iter().next().and_then(|(camera, transform)| {
        camera.world_to_viewport(transform, coords::sim_to_world_at(miner.pos, LIFT)).ok()
    }) else {
        // Behind the camera or off the edge of the projection.
        *visibility = Visibility::Hidden;
        return;
    };

    *visibility = Visibility::Inherited;
    // Centred on the point, so the row sits over the hull rather than beside it.
    let width = MODES.len() as f32 * DOT + (MODES.len() as f32 - 1.0) * 6.0 + 10.0;
    node.left = px(screen.x - width * 0.5);
    node.top = px(screen.y - DOT - 10.0);

    for (button, interaction, mut background, mut border) in &mut buttons {
        let selected = button.0 == mode;
        // The chosen one is lit; the rest sit back so the current state reads at
        // a glance rather than having to be worked out.
        let base = color(button.0);
        let shade = if selected || *interaction != Interaction::None { 1.0 } else { 0.42 };
        let c = base.to_srgba();
        *background = BackgroundColor(Color::srgb(c.red * shade, c.green * shade, c.blue * shade));
        *border = BorderColor::all(if selected {
            Color::srgb(0.95, 0.98, 1.0)
        } else {
            Color::NONE
        });

        if *interaction == Interaction::Pressed && !selected {
            net.set_miner_mode(button.0);
        }
    }
}
