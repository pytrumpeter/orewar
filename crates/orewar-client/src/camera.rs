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

use bevy::input::mouse::{MouseMotion, MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::render::view::NoIndirectDrawing;
use orewar_shared::math::Vec2 as SimVec2;
use orewar_shared::protocol::VehicleSlot;
use orewar_shared::sim;
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
/// What one wheel notch multiplies or divides the height by.
const ZOOM_PER_NOTCH: f32 = 1.15;
/// How far behind the point it looks at the overview camera sits, as a fraction
/// of its height. Straight down reads as a map; a little tilt keeps the hills
/// and vehicles standing up.
const OVERVIEW_TILT: f32 = 0.32;
/// Height the overview opens at when raised by hand: enough of the field to
/// plan with, close enough to tell vehicles apart.
const WORKING_HEIGHT: f32 = WORLD_SIZE * 0.55;
/// The most one frame's worth of scrolling may change the zoom by. A precision
/// touchpad can deliver a great many small events in a single frame, and
/// without a cap one flick lands on a limit instead of where it was aimed.
const MAX_NOTCHES_PER_FRAME: f32 = 4.0;

/// What the zoom keys change the height by per second held.
const KEY_ZOOM_RATE: f32 = 1.9;

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
    time: Res<Time>,
    mut overview: ResMut<Overview>,
) {
    // Off the field after losing a harvester: there is no vehicle to chase, so
    // the overview is the only sensible thing to show, whether it was asked for
    // or not.
    let waiting = state.local().is_some_and(|p| p.respawn_in > 0);

    if !menu.open && keys.just_pressed(KeyCode::KeyO) {
        overview.toggled = !overview.toggled;
        if overview.toggled && !overview.active {
            // Opened at a known height rather than wherever the zoom was left,
            // so raising the camera always gives the same usable framing.
            overview.height = WORKING_HEIGHT;
            // Start over whatever you were driving, so raising the camera does
            // not also lose your place.
            if let Some(pos) = state.local().and_then(|p| {
                p.vehicle(input.controlling)
                    .or_else(|| p.vehicle(input.controlling.next_available(|s| p.vehicle(s).is_some())))
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
        // you are about to come back -- and pulled far enough out to watch the
        // whole match while you wait it out.
        overview.centre = orewar_shared::world::base_position(state.local_player.unwrap_or(0));
        overview.height = MAX_HEIGHT;
    }

    // Only a focused window is being scrolled at on purpose. Without this the
    // camera reads whatever the pointing device happens to be emitting while
    // the player is somewhere else entirely, and a stream of small scroll
    // events walks the zoom to a limit and holds it there.
    let focused = windows.iter().any(|w| w.focused);
    if !overview.active || menu.open || !focused {
        wheel.clear();
        motion.clear();
        return;
    }

    // A wheel event arrives either as lines or as raw pixels depending on the
    // device and the platform, and Windows sends 120 pixels per notch. Summing
    // the raw values and treating them as notches makes one flick of the wheel
    // read as hundreds, which is not a difference the rest of this can absorb.
    let notches: f32 = wheel
        .read()
        .map(|e| match e.unit {
            MouseScrollUnit::Line => e.y,
            MouseScrollUnit::Pixel => e.y / 120.0,
        })
        .sum::<f32>()
        .clamp(-MAX_NOTCHES_PER_FRAME, MAX_NOTCHES_PER_FRAME);
    // Keys do the same job. They are also the dependable way to zoom: some
    // pointing devices deliver a stream of scroll events for as long as the
    // cursor is over the window, whether or not anyone has touched the wheel,
    // and there is no way to tell those apart from a real scroll by their
    // contents. Where that happens the wheel is unusable and these are not.
    let mut zoom = 0.0;
    if keys.pressed(KeyCode::Equal) || keys.pressed(KeyCode::NumpadAdd) {
        zoom += KEY_ZOOM_RATE * time.delta_secs();
    }
    if keys.pressed(KeyCode::Minus) || keys.pressed(KeyCode::NumpadSubtract) {
        zoom -= KEY_ZOOM_RATE * time.delta_secs();
    }
    zoom += notches;

    if zoom != 0.0 {
        // Exponential, so a notch covers the same proportion of the view whether
        // you are looking at one base or the whole field -- and, unlike a linear
        // step, no quantity of scrolling can drive the height through zero and
        // out the other side.
        overview.height =
            (overview.height * ZOOM_PER_NOTCH.powf(-zoom)).clamp(MIN_HEIGHT, MAX_HEIGHT);
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

    // Follow whichever vehicle is being driven; if it is gone (destroyed,
    // captured, or out of fuel), fall back to anything else the player still
    // has, then to the field itself.
    let subject = state.local().and_then(|player| {
        let slot = if player.vehicle(input.controlling).is_some() {
            input.controlling
        } else {
            input.controlling.next_available(|s| player.vehicle(s).is_some())
        };
        player.vehicle(slot).map(|v| (v.pos, v.yaw, slot))
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
        Some((pos, yaw, slot)) => {
            // An aircraft is followed at its own altitude, so the camera rides
            // with it rather than watching it from the grass. It is otherwise
            // the same chase: the trail and the look-ahead are what make a
            // vehicle feel driven, and that does not change with height.
            let lift = if slot == VehicleSlot::Plane { sim::PLANE_ALTITUDE } else { 0.0 };
            let center = coords::sim_to_world(pos) + Vec3::Y * lift;
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
