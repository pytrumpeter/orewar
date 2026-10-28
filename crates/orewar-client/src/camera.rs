//! Chase camera, and the overview it lifts to.
//!
//! The chase camera sits low and close behind the vehicle you are driving,
//! looking along its heading at a shallow angle -- near enough to first person
//! that the field reads as a place you are in, rather than a board seen from
//! above.
//!
//! The overview is the other half of that: a high, free camera you can pan and
//! zoom over the whole field. It comes up on its own for the minute you are off
//! the field after losing a miner, since there is nothing to chase, and can
//! be raised at any other time with the overview key.

use bevy::input::mouse::{MouseMotion, MouseScrollUnit, MouseWheel};
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
/// What one wheel notch multiplies or divides the height by.
pub const ZOOM_PER_NOTCH: f32 = 1.15;
/// How far behind the point it looks at the overview camera sits, as a fraction
/// of its height, before anybody tilts it. Straight down reads as a map; a
/// little tilt keeps the hills and vehicles standing up.
const OVERVIEW_TILT: f32 = 0.32;

/// How far the chase camera may be pulled in or pushed out, as multiples of the
/// distance it starts at. Much closer and the hull fills the screen; much
/// further and it is a slow overview that still turns with the vehicle.
const CHASE_ZOOM_MIN: f32 = 0.6;
const CHASE_ZOOM_MAX: f32 = 2.6;
/// The chase camera's look-down angle, either side of where it starts. Low
/// enough to see the horizon over the hull, but not so low that a pulled-in
/// camera sits in the grass; high enough to look nearly down at the vehicle
/// without losing which way it is heading.
const CHASE_PITCH_MIN: f32 = 12.0 * (std::f32::consts::PI / 180.0);
const CHASE_PITCH_MAX: f32 = 72.0 * (std::f32::consts::PI / 180.0);
/// The overview's angle above the ground. The top end stops short of straight
/// down, where "up the screen" stops meaning anything -- for looking at, and
/// for aiming the tablet's stick.
const OVERVIEW_PITCH_MIN: f32 = 30.0 * (std::f32::consts::PI / 180.0);
const OVERVIEW_PITCH_MAX: f32 = 88.0 * (std::f32::consts::PI / 180.0);
/// What the tilt keys turn the camera by per second held.
const KEY_TILT_RATE: f32 = 0.9;
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

/// How the player has set the two cameras up: how far out the chase camera
/// sits, and how steeply each of them looks down.
///
/// Kept apart from [`Overview`] because it outlives it: a chase camera pulled
/// back stays pulled back after a trip up to the overview and down again, and
/// across swapping vehicles.
#[derive(Resource)]
pub struct CameraRig {
    /// Multiple of the default trailing distance.
    chase_zoom: f32,
    /// Angle above the vehicle's heading the chase camera sits at, in radians.
    chase_pitch: f32,
    /// Angle above the ground the overview looks down from, in radians.
    overview_pitch: f32,
}

impl Default for CameraRig {
    fn default() -> Self {
        CameraRig {
            chase_zoom: 1.0,
            chase_pitch: default_chase_pitch(),
            overview_pitch: default_overview_pitch(),
        }
    }
}

impl CameraRig {
    /// Where the chase camera sits and looks, along the vehicle's heading:
    /// how far behind it, how far above it, and how far ahead it aims.
    ///
    /// At the defaults this is exactly [`TRAIL`], [`HEIGHT`] and
    /// [`LOOK_AHEAD`].
    fn chase_frame(&self) -> (f32, f32, f32) {
        let distance = TRAIL.hypot(HEIGHT) * self.chase_zoom;
        // The look-ahead shrinks as the camera climbs over the hull. Looking
        // down from above, a point far ahead would put the vehicle itself at
        // the bottom edge of the screen, or under it.
        let ahead = LOOK_AHEAD * self.chase_pitch.cos() / default_chase_pitch().cos();
        (distance * self.chase_pitch.cos(), distance * self.chase_pitch.sin(), ahead)
    }

    /// Where the overview camera sits relative to the point it looks at.
    ///
    /// Tilting swings it round that point at a fixed distance rather than
    /// sliding it sideways at a fixed height. Distance is what the fog is
    /// measured by, so a tilted overview sees as far as an untilted one.
    fn overview_offset(&self, height: f32) -> Vec3 {
        let distance = height * (1.0 + OVERVIEW_TILT * OVERVIEW_TILT).sqrt();
        distance * Vec3::new(0.0, self.overview_pitch.sin(), self.overview_pitch.cos())
    }

    /// Zooms and tilts the chase camera. Positive `zoom` is in notches toward
    /// the vehicle; positive `tilt` is radians toward looking straight down.
    fn adjust_chase(&mut self, zoom: f32, tilt: f32) {
        self.chase_zoom =
            (self.chase_zoom * ZOOM_PER_NOTCH.powf(-zoom)).clamp(CHASE_ZOOM_MIN, CHASE_ZOOM_MAX);
        self.chase_pitch = (self.chase_pitch + tilt).clamp(CHASE_PITCH_MIN, CHASE_PITCH_MAX);
    }

    fn tilt_overview(&mut self, tilt: f32) {
        self.overview_pitch =
            (self.overview_pitch + tilt).clamp(OVERVIEW_PITCH_MIN, OVERVIEW_PITCH_MAX);
    }
}

fn default_chase_pitch() -> f32 {
    HEIGHT.atan2(TRAIL)
}

fn default_overview_pitch() -> f32 {
    1.0f32.atan2(OVERVIEW_TILT)
}

/// A request to zoom or tilt whichever camera is up, from input this module
/// does not read for itself -- fingers on a tablet.
///
/// Sent without knowing which camera that is, because whoever sends it should
/// not have to: a pinch means closer whether it lands on the chase camera or
/// the overview.
#[derive(Message, Clone, Copy, Default, Debug, PartialEq)]
pub struct CameraNudge {
    /// In wheel notches; positive is closer.
    pub zoom: f32,
    /// In radians; positive is steeper, toward looking straight down.
    pub tilt: f32,
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

/// Raises, lowers, pans and zooms the overview, and zooms and tilts whichever
/// camera is up.
///
/// Runs before [`follow`], which only reads the result.
pub fn overview_controls(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut wheel: MessageReader<MouseWheel>,
    mut motion: MessageReader<MouseMotion>,
    mut nudges: MessageReader<CameraNudge>,
    windows: Query<&Window>,
    menu: Res<MenuState>,
    state: Res<GameState>,
    input: Res<LocalInput>,
    time: Res<Time>,
    mut overview: ResMut<Overview>,
    mut rig: ResMut<CameraRig>,
) {
    // Off the field after losing a miner: there is no vehicle to chase, so
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
    if menu.open {
        wheel.clear();
        motion.clear();
        nudges.clear();
        return;
    }
    if !focused {
        // Fingers are not a pointing device left idling over the window, so
        // what they send still counts.
        wheel.clear();
        motion.clear();
    }

    let (zoom, tilt) = camera_moves(&keys, focused, &mut wheel, &mut nudges, time.delta_secs());
    let reset = focused && keys.just_pressed(KeyCode::Home);

    if !overview.active {
        // The mouse is aiming the turret down here, not dragging anything.
        motion.clear();
        if reset {
            let fresh = CameraRig::default();
            rig.chase_zoom = fresh.chase_zoom;
            rig.chase_pitch = fresh.chase_pitch;
        }
        rig.adjust_chase(zoom, tilt);
        return;
    }

    if reset {
        overview.height = WORKING_HEIGHT;
        rig.overview_pitch = default_overview_pitch();
    }
    rig.tilt_overview(tilt);

    if zoom != 0.0 {
        // Exponential, so a notch covers the same proportion of the view whether
        // you are looking at one base or the whole field -- and, unlike a linear
        // step, no quantity of scrolling can drive the height through zero and
        // out the other side.
        overview.height =
            (overview.height * ZOOM_PER_NOTCH.powf(-zoom)).clamp(MIN_HEIGHT, MAX_HEIGHT);
    }

    // Right button drags the map. The left one is left alone so the buttons
    // floating over the miner stay clickable from up here.
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

/// This frame's zoom, in notches toward the ground, and tilt, in radians
/// toward looking straight down -- from the wheel, the keys and anything that
/// sent a [`CameraNudge`], whichever camera they end up moving.
fn camera_moves(
    keys: &ButtonInput<KeyCode>,
    focused: bool,
    wheel: &mut MessageReader<MouseWheel>,
    nudges: &mut MessageReader<CameraNudge>,
    dt: f32,
) -> (f32, f32) {
    let mut zoom = 0.0;
    let mut tilt = 0.0;
    for nudge in nudges.read() {
        zoom += nudge.zoom;
        tilt += nudge.tilt;
    }
    if !focused {
        return (zoom, tilt);
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
    if keys.pressed(KeyCode::Equal) || keys.pressed(KeyCode::NumpadAdd) {
        zoom += KEY_ZOOM_RATE * dt;
    }
    if keys.pressed(KeyCode::Minus) || keys.pressed(KeyCode::NumpadSubtract) {
        zoom -= KEY_ZOOM_RATE * dt;
    }
    zoom += notches;

    // Page Up looks further down, toward a map; Page Down lifts the view
    // toward the horizon.
    if keys.pressed(KeyCode::PageUp) {
        tilt += KEY_TILT_RATE * dt;
    }
    if keys.pressed(KeyCode::PageDown) {
        tilt -= KEY_TILT_RATE * dt;
    }
    (zoom, tilt)
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
    rig: Res<CameraRig>,
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
        player.vehicle(slot).map(|v| (v.pos, v.yaw, v.alt))
    });

    let (desired_position, desired_aim) = match subject.filter(|_| !overview.active) {
        // Free camera: looking down at wherever it has been panned to, from
        // however high it has been zoomed.
        _ if overview.active => {
            let look = coords::sim_to_world(overview.centre);
            (look + rig.overview_offset(overview.height), look)
        }
        Some((pos, yaw, alt)) => {
            // An aircraft is followed at its own altitude, so the camera rides
            // with it rather than watching it from the grass. It is otherwise
            // the same chase: the trail and the look-ahead are what make a
            // vehicle feel driven, and that does not change with height.
            //
            // Taken from the vehicle rather than from a constant, so the camera
            // climbs and dives with the aircraft. Ground vehicles report zero,
            // which is what the constant used to be for them.
            let center = coords::sim_to_world(pos) + Vec3::Y * alt;
            let heading = coords::yaw_to_quat(yaw) * Vec3::X;
            let (behind, above, ahead) = rig.chase_frame();
            (
                center - heading * behind + Vec3::Y * above,
                center + heading * ahead + Vec3::Y * LOOK_HEIGHT,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    /// Nobody who never touches the new controls should see a different
    /// camera: the defaults have to land exactly where the constants used to.
    #[test]
    fn an_untouched_rig_frames_the_vehicle_as_before() {
        let rig = CameraRig::default();
        let (behind, above, ahead) = rig.chase_frame();
        assert!(close(behind, TRAIL) && close(above, HEIGHT) && close(ahead, LOOK_AHEAD));

        let offset = rig.overview_offset(100.0);
        assert!(close(offset.x, 0.0));
        assert!(close(offset.y, 100.0));
        assert!(close(offset.z, 100.0 * OVERVIEW_TILT));
    }

    /// However far either control is pushed, the camera stays inside the
    /// range it was given -- in particular, no amount of zooming in puts it
    /// inside the hull.
    #[test]
    fn the_chase_camera_stops_at_its_limits() {
        let mut rig = CameraRig::default();
        rig.adjust_chase(1000.0, 1000.0);
        assert!(close(rig.chase_zoom, CHASE_ZOOM_MIN));
        assert!(close(rig.chase_pitch, CHASE_PITCH_MAX));
        rig.adjust_chase(-1000.0, -1000.0);
        assert!(close(rig.chase_zoom, CHASE_ZOOM_MAX));
        assert!(close(rig.chase_pitch, CHASE_PITCH_MIN));
    }

    /// Looking down at the vehicle from above, the point aimed at must come in
    /// toward it, or the hull slides off the bottom of the screen.
    #[test]
    fn a_steeper_chase_camera_looks_nearer_the_hull() {
        let mut rig = CameraRig::default();
        let (_, _, before) = rig.chase_frame();
        rig.adjust_chase(0.0, 1.0);
        let (behind, above, after) = rig.chase_frame();
        assert!(after < before);
        assert!(above > behind, "a steep camera should be more above than behind");
    }

    /// Tilting swings the overview round the point it looks at, so it never
    /// comes nearer to it or goes further from it -- that is what the fog is
    /// measured by.
    #[test]
    fn tilting_the_overview_keeps_its_distance() {
        let mut rig = CameraRig::default();
        let before = rig.overview_offset(150.0).length();
        rig.tilt_overview(-0.5);
        let after = rig.overview_offset(150.0);
        assert!(close(after.length(), before));
        assert!(after.y < 150.0, "tilted toward the horizon, it should sit lower");
        rig.tilt_overview(1000.0);
        assert!(close(rig.overview_pitch, OVERVIEW_PITCH_MAX));
    }
}
