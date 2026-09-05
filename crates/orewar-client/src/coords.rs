//! The single place the simulation's 2D world is mapped into Bevy's 3D one.
//!
//! Getting this wrong is uniquely nasty to debug -- vehicles drive sideways,
//! turrets aim at mirror images -- and it is easy to get wrong in two places
//! differently. So every conversion in the client goes through these four
//! functions, and the test at the bottom pins the relationship down.

use bevy::prelude::*;
use orewar_shared::math::Vec2 as SimVec2;
use orewar_shared::world;

/// The ground plane.
pub const GROUND_Y: f32 = 0.0;

/// Simulation `(x, y)` becomes world `(x, 0, y)`: the sim's second axis is
/// Bevy's `Z`, because Bevy reserves `Y` for up.
#[inline]
pub fn sim_to_world(p: SimVec2) -> Vec3 {
    Vec3::new(p.x, GROUND_Y, p.y)
}

/// As [`sim_to_world`], at a given height.
#[inline]
pub fn sim_to_world_at(p: SimVec2, y: f32) -> Vec3 {
    Vec3::new(p.x, y, p.y)
}

#[inline]
pub fn world_to_sim(v: Vec3) -> SimVec2 {
    SimVec2::new(v.x, v.z)
}

/// Converts a simulation heading into a Bevy rotation.
///
/// The sign is the subtle part. A simulation yaw of zero faces `+X` and
/// increasing yaw turns toward `+Y`, which is world `+Z`. But a *positive*
/// rotation about Bevy's `+Y` axis carries `+X` toward `−Z`. The two conventions
/// run opposite ways, so the angle is negated. Models are therefore built facing
/// `+X`.
#[inline]
pub fn yaw_to_quat(yaw: f32) -> Quat {
    Quat::from_rotation_y(-yaw)
}

/// A player's identity color.
pub fn player_color(id: u8) -> Color {
    let [r, g, b] = world::PLAYER_COLORS[id as usize % world::MAX_PLAYERS];
    Color::srgb_u8(r, g, b)
}

/// A dimmer version of a player's color, for panels and trim.
pub fn player_color_dim(id: u8, factor: f32) -> Color {
    let c = player_color(id).to_srgba();
    Color::srgb(c.red * factor, c.green * factor, c.blue * factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing invariant: a vehicle rendered with `yaw_to_quat` must
    /// point where the simulation says it is heading. If this fails, pressing
    /// forward moves the vehicle somewhere other than where it faces.
    #[test]
    fn rendered_heading_matches_simulation_heading() {
        for step in 0..64 {
            let yaw = step as f32 * std::f32::consts::TAU / 64.0;
            let sim_heading = sim_to_world(SimVec2::from_angle(yaw));
            let rendered = yaw_to_quat(yaw) * Vec3::X;
            assert!(
                sim_heading.distance(rendered) < 1e-5,
                "yaw {yaw}: simulation heads {sim_heading:?} but the model faces {rendered:?}"
            );
        }
    }

    /// Fixes which way is "right", which steering and the radar both depend on.
    ///
    /// A camera trailing the vehicle has screen-right `cross(heading, up)`.
    /// Simulation `perp` maps onto exactly that, which means increasing yaw
    /// turns the vehicle toward its right -- so the left key produces
    /// *negative* steer, and a radar contact's rightward offset is
    /// `offset.dot(heading.perp())`.
    #[test]
    fn sim_perp_is_screen_right() {
        for step in 0..32 {
            let yaw = step as f32 * std::f32::consts::TAU / 32.0;
            let heading = sim_to_world(SimVec2::from_angle(yaw));
            let perp = sim_to_world(SimVec2::from_angle(yaw).perp());
            let camera_right = heading.cross(Vec3::Y);
            assert!(
                perp.distance(camera_right) < 1e-5,
                "yaw {yaw}: perp is {perp:?} but screen-right is {camera_right:?}"
            );
        }
    }

    /// Increasing yaw must turn the vehicle toward its right.
    #[test]
    fn increasing_yaw_turns_right() {
        let before = sim_to_world(SimVec2::from_angle(0.0));
        let after = sim_to_world(SimVec2::from_angle(0.15));
        let right = before.cross(Vec3::Y);
        assert!(
            (after - before).dot(right) > 0.0,
            "a positive yaw change should move the heading rightward"
        );
    }

    #[test]
    fn world_and_sim_round_trip() {
        let p = SimVec2::new(12.5, -33.25);
        assert_eq!(world_to_sim(sim_to_world(p)), p);
        assert_eq!(world_to_sim(sim_to_world_at(p, 9.0)), p);
    }
}
