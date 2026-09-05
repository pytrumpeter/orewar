//! Chase camera, and the overview it lifts to.
//!
//! The chase camera sits low and close behind the vehicle you are driving,
//! looking along its heading at a shallow angle -- near enough to first person
//! that the field reads as a place you are in, rather than a board seen from
//! above.
//!
//! The overview is the other half of that: a high, free camera you can pan and
//! zoom over the whole field. It comes up on its own for the minute you are off
//! the field after losing a harvester, since there is nothing to chase, and can
//! be raised at any other time with the overview key.

use bevy::input::mouse::{MouseMotion, MouseWheel};
use bevy::prelude::*;
use bevy::render::view::NoIndirectDrawing;
use orewar_shared::math::Vec2 as SimVec2;
use orewar_shared::world::WORLD_SIZE;

use crate::coords;
use crate::field::SKY_COLOR;
use crate::input::LocalInput;
use crate::menu::MenuState;
use crate::state::GameState;

/// Distance the camera trails behind the vehicle.
const TRAIL: f32 = 25.0;
/// Camera height. With `TRAIL` this is a ~21 degree look-down angle.
const HEIGHT: f32 = 9.5;
/// How far ahead of the vehicle the camera aims.
const LOOK_AHEAD: f32 = 16.0;
const LOOK_HEIGHT: f32 = 2.2;

/// Time for the camera to close half the distance to where it wants to be.
/// Long enough to smooth out prediction corrections, short enough to feel
/// attached to the vehicle.
const POSITION_HALF_LIFE: f32 = 0.09;
const AIM_HALF_LIFE: f32 = 0.07;

/// Camera height at the closest and furthest the overview will go.
///
/// The far end is set so the whole field fits: with the default vertical field
/// of view a camera at `h` sees about `0.83 * h` of ground, and the field is
/// [`WORLD_SIZE`] across.
const MIN_HEIGHT: f32 = 70.0;
const MAX_HEIGHT: f32 = WORLD_SIZE / 0.75;
/// How much of the height one wheel notch adds or removes.
const ZOOM_STEP: f32 = 0.12;
/// How far behind the point it looks at the overview camera sits, as a fraction
/// of its height. Straight down reads as a map; a little tilt keeps the hills
/// and vehicles standing up.
const OVERVIEW_TILT: f32 = 0.32;

#[derive(Component)]
pub struct ChaseCamera;

/// The free camera: where it is looking and how high it is.
#[derive(Resource)]
pub struct Overview {
    /// Raised deliberately with the overview key, as opposed to automatically
    /// because there is nothing to follow.
    pub toggled: bool,
    /// Whether it is up at all this frame, for anything that needs to know --
    /// the controls are gated while it is.
    pub active: bool,
    centre: SimVec2,
    height: f32,
}

impl Default for Overview {
    fn default() -> Self {
        Overview {
            toggled: false,
            active: false,
            centre: SimVec2::splat(WORLD_SIZE * 0.5),
            height: WORLD_SIZE * 0.75,
        }
    }
}

pub fn setup(mut commands: Commands) {
    let half = WORLD_SIZE * 0.5;
    commands.spawn((
        Camera3d::default(),
        ChaseCamera,
        // Draw with direct draw calls instead of GPU-culled indirect ones.
        // The Adreno X1 driver this was developed against advertises full
        // support for indirect drawing and then issues no draws at all: the UI
        // rendered while the entire 3D scene stayed empty. Bevy must be told at
        // camera-spawn time; adding it later is documented as unsupported.
        NoIndirectDrawing,
        Transform::from_xyz(half, 60.0, half + 90.0)
            .looking_at(Vec3::new(half, 0.0, half), Vec3::Y),
        // Fades the far side of the field into the sky so the boundary wall
        // does not end against a hard horizon line.
        DistanceFog {
            color: SKY_COLOR,
            falloff: FogFalloff::Linear { start: 190.0, end: 400.0 },
            ..default()
        },
    ));
}

/// Raises, lowers, pans and zooms the overview.
///
/// Runs before [`follow`], which only reads the result.
pub fn overview_controls(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut wheel: MessageReader<MouseWheel>,
    mut motion: MessageReader<MouseMotion>,
    windows: Query<&Window>,
    menu: Res<MenuState>,
    state: Res<GameState>,
    input: Res<LocalInput>,
    mut overview: ResMut<Overview>,
) {
    // Off the field after losing a harvester: there is no vehicle to chase, so
    // the overview is the only sensible thing to show, whether it was asked for
    // or not.
    let waiting = state.local().is_some_and(|p| p.respawn_in > 0);

    if !menu.open && keys.just_pressed(KeyCode::KeyO) {
        overview.toggled = !overview.toggled;
        if overview.toggled && !overview.active {
            // Start over whatever you were driving, so raising the camera does
            // not also lose your place.
            if let Some(pos) = state.local().and_then(|p| {
                p.vehicle(input.controlling).or_else(|| p.vehicle(input.controlling.other()))
            }) {
                // Pulled back onto the field by the clamp below if the vehicle
                // is hard against an edge, so the view never opens half full of
                // sky.
                overview.centre = pos.pos;
            }
        }
    }

    let was_active = overview.active;
    overview.active = overview.toggled || waiting;
    if overview.active && !was_active && waiting {
        // Nothing left on the field, so open on your own corner -- that is where
        // you are about to come back.
        overview.centre = orewar_shared::world::base_position(state.local_player.unwrap_or(0));
    }

    if !overview.active || menu.open {
        wheel.clear();
        motion.clear();
        return;
    }

    // Zoom multiplicatively, so a notch covers the same proportion of the view
    // whether you are looking at one base or the whole field.
    let notches: f32 = wheel.read().map(|e| e.y).sum();
    if notches != 0.0 {
        overview.height =
            (overview.height * (1.0 - ZOOM_STEP * notches)).clamp(MIN_HEIGHT, MAX_HEIGHT);
    }

    // Right button drags the map. The left one is left alone so the buttons
    // floating over the harvester stay clickable from up here.
    let dragged: Vec2 = if buttons.pressed(MouseButton::Right) {
        motion.read().map(|m| m.delta).sum()
    } else {
        motion.clear();
        Vec2::ZERO
    };
    if dragged != Vec2::ZERO {
        // One pixel of drag moves the ground under the cursor by one pixel's
        // worth of ground, so the map tracks the mouse rather than sliding at
        // some rate of its own.
        let viewport = windows.iter().next().map_or(720.0, |w| w.height());
        let per_pixel = visible_height(overview.height) / viewport.max(1.0);
        overview.centre.x -= dragged.x * per_pixel;
        overview.centre.y -= dragged.y * per_pixel;
    }

    // Zooming changes how much is on screen, so the framing is settled after
    // both, not just after a drag.
    let margin = (visible_height(overview.height) * 0.5).min(WORLD_SIZE * 0.5);
    overview.centre.x = overview.centre.x.clamp(margin, WORLD_SIZE - margin);
    overview.centre.y = overview.centre.y.clamp(margin, WORLD_SIZE - margin);
}

/// How much ground a camera at `height` sees top to bottom, for the default
/// vertical field of view.
fn visible_height(height: f32) -> f32 {
    // 2 * tan(fov / 2) with Bevy's default 45 degree vertical FOV.
    height * 0.828
}

pub fn follow(
    state: Res<GameState>,
    input: Res<LocalInput>,
    overview: Res<Overview>,
    time: Res<Time>,
    mut camera: Query<&mut Transform, With<ChaseCamera>>,
    mut aim: Local<Option<Vec3>>,
) {
    let Ok(mut transform) = camera.single_mut() else { return };
    let dt = time.delta_secs();
    let half = WORLD_SIZE * 0.5;

    // Follow whichever vehicle is being driven; if it is gone (destroyed, or
    // captured), fall back to the other one, then to the field itself.
    let subject = state.local().and_then(|player| {
        player
            .vehicle(input.controlling)
            .or_else(|| player.vehicle(input.controlling.other()))
            .map(|v| (v.pos, v.yaw))
    });

    let (desired_position, desired_aim) = match subject.filter(|_| !overview.active) {
        // Free camera: looking down at wherever it has been panned to, from
        // however high it has been zoomed.
        _ if overview.active => {
            let look = coords::sim_to_world(overview.centre);
            (
                look + Vec3::new(0.0, overview.height, overview.height * OVERVIEW_TILT),
                look,
            )
        }
        Some((pos, yaw)) => {
            let center = coords::sim_to_world(pos);
            let heading = coords::yaw_to_quat(yaw) * Vec3::X;
            (
                center - heading * TRAIL + Vec3::Y * HEIGHT,
                center + heading * LOOK_AHEAD + Vec3::Y * LOOK_HEIGHT,
            )
        }
        None => (
            Vec3::new(half, 95.0, half + 130.0),
            Vec3::new(half, 0.0, half),
        ),
    };

    let current_aim = aim.unwrap_or(desired_aim);
    let smoothed_aim = current_aim.lerp(desired_aim, smoothing(AIM_HALF_LIFE, dt));
    *aim = Some(smoothed_aim);

    transform.translation =
        transform.translation.lerp(desired_position, smoothing(POSITION_HALF_LIFE, dt));
    transform.look_at(smoothed_aim, Vec3::Y);

}

/// Frame-rate independent smoothing factor for a given half-life.
fn smoothing(half_life: f32, dt: f32) -> f32 {
    if half_life <= 0.0 {
        return 1.0;
    }
    1.0 - (-dt * std::f32::consts::LN_2 / half_life).exp()
}
