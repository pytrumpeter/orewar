//! Chase camera.
//!
//! Sits low and close behind the vehicle you are driving, looking along its
//! heading at a shallow angle -- near enough to first person that the field
//! reads as a place you are in, rather than a board seen from above.

use bevy::prelude::*;
use bevy::render::view::NoIndirectDrawing;
use orewar_shared::world::WORLD_SIZE;

use crate::coords;
use crate::field::SKY_COLOR;
use crate::input::LocalInput;
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

#[derive(Component)]
pub struct ChaseCamera;

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

pub fn follow(
    state: Res<GameState>,
    input: Res<LocalInput>,
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

    let (desired_position, desired_aim) = match subject {
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
