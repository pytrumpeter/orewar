//! Vehicle physics, tuning, and combat geometry.
//!
//! Every function here is a pure function of state and input. The server runs
//! them to produce authoritative state; the client runs the *same* functions at
//! the *same* fixed rate to predict its own vehicle. Any divergence between the
//! two shows up as prediction error the client has to smooth away, so this
//! module must stay free of wall-clock time, randomness, and floating frame
//! deltas.

use crate::math::{Vec2, angle_approach, approach, wrap_angle};
use crate::world::{self, Hill, PowerUp, WORLD_SIZE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum VehicleKind {
    Tank = 0,
    Harvester = 1,
}

impl VehicleKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(VehicleKind::Tank),
            1 => Some(VehicleKind::Harvester),
            _ => None,
        }
    }
}

pub struct VehicleTuning {
    pub max_speed: f32,
    pub reverse_speed: f32,
    pub accel: f32,
    pub brake: f32,
    pub turn_rate: f32,
    /// Collision radius, also used for keeping vehicles inside the field.
    pub radius: f32,
    pub base_shield: f32,
    pub base_hull: f32,
}

pub fn tuning(kind: VehicleKind) -> VehicleTuning {
    match kind {
        VehicleKind::Tank => VehicleTuning {
            max_speed: 19.0,
            reverse_speed: 9.0,
            accel: 26.0,
            brake: 40.0,
            turn_rate: 2.3,
            radius: 2.4,
            base_shield: 110.0,
            base_hull: 100.0,
        },
        VehicleKind::Harvester => VehicleTuning {
            max_speed: 11.5,
            reverse_speed: 6.0,
            accel: 15.0,
            brake: 26.0,
            turn_rate: 1.6,
            radius: 2.9,
            base_shield: 150.0,
            base_hull: 130.0,
        },
    }
}

// ---------------------------------------------------------------------------
// Power-up derived stats
//
// The client recomputes these from the power-up mask in each snapshot, so
// maximums never travel on the wire.
// ---------------------------------------------------------------------------

pub fn max_shield(kind: VehicleKind, powerups: u16) -> f32 {
    let base = tuning(kind).base_shield;
    if PowerUp::ShieldBooster.held(powerups) { base + 60.0 } else { base }
}

pub fn max_hull(kind: VehicleKind, powerups: u16) -> f32 {
    let base = tuning(kind).base_hull;
    if kind == VehicleKind::Harvester && PowerUp::HarvesterArmor.held(powerups) {
        base + 60.0
    } else {
        base
    }
}

pub fn speed_multiplier(powerups: u16) -> f32 {
    if PowerUp::Turbo.held(powerups) { 1.3 } else { 1.0 }
}

pub fn cargo_capacity(powerups: u16) -> f32 {
    let base = 60.0;
    if PowerUp::HarvesterArmor.held(powerups) { base * 1.25 } else { base }
}

pub fn shield_regen_rate(powerups: u16) -> f32 {
    if PowerUp::ShieldBooster.held(powerups) { 18.0 } else { 12.0 }
}

// ---------------------------------------------------------------------------
// Movement
// ---------------------------------------------------------------------------

/// The part of a vehicle's state that movement integration touches.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveState {
    pub pos: Vec2,
    /// Hull heading in radians. Zero faces +X.
    pub yaw: f32,
    /// Signed speed along `yaw`; negative is reversing.
    pub speed: f32,
}

/// Advances a vehicle by one tick.
///
/// `throttle` and `steer` are each clamped to `-1..=1`. Both vehicles are
/// tracked, so they pivot in place rather than needing forward motion to turn.
///
/// `hills` is taken here rather than resolved by the caller because this is
/// the one function that owns where a vehicle ends up. The client predicts with
/// it and the server decides with it; if terrain were applied outside, one side
/// could forget and every hill would become a rubber-banding bug.
pub fn step_vehicle(
    state: &mut MoveState,
    throttle: f32,
    steer: f32,
    kind: VehicleKind,
    powerups: u16,
    hills: &[Hill],
    dt: f32,
) {
    let t = tuning(kind);
    let throttle = throttle.clamp(-1.0, 1.0);
    let steer = steer.clamp(-1.0, 1.0);
    let mult = speed_multiplier(powerups);

    let target = if throttle >= 0.0 {
        throttle * t.max_speed * mult
    } else {
        throttle * t.reverse_speed * mult
    };

    // Slowing down is quicker than speeding up, which is what makes the
    // vehicles feel heavy rather than floaty.
    let closing = target.abs() < state.speed.abs() || target.signum() != state.speed.signum();
    let rate = if closing { t.brake } else { t.accel };
    state.speed = approach(state.speed, target, rate * dt);

    state.yaw = wrap_angle(state.yaw + steer * t.turn_rate * dt);
    state.pos += Vec2::from_angle(state.yaw) * (state.speed * dt);

    // Hills are solid at every height: you go around, never over. Resolved
    // by pushing back out along the radius, which leaves a vehicle that
    // arrives at an angle sliding around the flank rather than sticking.
    for hill in hills {
        let block = hill.radius + t.radius;
        let offset = state.pos - hill.pos;
        let dist = offset.length();
        if dist >= block {
            continue;
        }
        // Dead centre offers no direction to leave by; back out along the
        // hull's own heading rather than dividing by zero.
        let out = if dist > 1e-4 { offset / dist } else { Vec2::from_angle(state.yaw) };
        state.pos = hill.pos + out * block;
        // Damped by how squarely the hill was met, not by a flat factor. Driving
        // into a face loses the same 60% a wall takes; grazing one barely costs
        // anything, which is what lets a hill be steered around rather than
        // becoming flypaper for anything that brushes it.
        let head_on = (-out).dot(Vec2::from_angle(state.yaw)).clamp(0.0, 1.0);
        state.speed *= 1.0 - 0.6 * head_on;
    }

    // The field is walled; running into the edge stops you rather than
    // letting you slide along at full speed. Applied after the terrain so
    // that nothing can push a vehicle out of the world.
    let lo = t.radius;
    let hi = WORLD_SIZE - t.radius;
    if state.pos.x < lo || state.pos.x > hi {
        state.pos.x = state.pos.x.clamp(lo, hi);
        state.speed *= 0.4;
    }
    if state.pos.y < lo || state.pos.y > hi {
        state.pos.y = state.pos.y.clamp(lo, hi);
        state.speed *= 0.4;
    }
}

/// Rotates a turret toward `desired`, respecting its slew rate.
pub fn step_turret(current: f32, desired: f32, dt: f32) -> f32 {
    angle_approach(current, desired, TURRET_TURN_RATE * dt)
}

// ---------------------------------------------------------------------------
// Combat tuning
// ---------------------------------------------------------------------------

pub const TURRET_TURN_RATE: f32 = 2.8;

pub const BULLET_SPEED: f32 = 90.0;
pub const BULLET_DAMAGE: f32 = 14.0;
pub const BULLET_LIFETIME: f32 = 2.2;
pub const BULLET_COOLDOWN: f32 = 0.22;

pub const MISSILE_LAUNCH_SPEED: f32 = 34.0;
pub const MISSILE_MAX_SPEED: f32 = 82.0;
pub const MISSILE_ACCEL: f32 = 34.0;
pub const MISSILE_DAMAGE: f32 = 58.0;
pub const MISSILE_TURN_RATE: f32 = 2.4;
pub const MISSILE_LIFETIME: f32 = 6.0;
pub const MISSILE_COOLDOWN: f32 = 2.4;
/// A missile only steers toward targets inside this cone, so it can be dodged.
pub const MISSILE_SEEK_CONE: f32 = 1.05;
pub const MISSILE_SEEK_RANGE: f32 = 120.0;

/// Seconds without taking damage before shields begin to regenerate.
pub const SHIELD_REGEN_DELAY: f32 = 4.0;

pub const AUTO_TURRET_RANGE: f32 = 42.0;
pub const AUTO_TURRET_COOLDOWN: f32 = 0.9;

/// Seconds before a destroyed tank returns to its base pad.
pub const TANK_RESPAWN_DELAY: f32 = 8.0;

// Economy.
pub const HARVEST_RADIUS: f32 = 7.5;
pub const HARVEST_RATE: f32 = 15.0;
/// A harvester must be nearly stationary to draw ore.
pub const HARVEST_MAX_SPEED: f32 = 4.0;
pub const UNLOAD_RATE: f32 = 50.0;

// Capture.
pub const CAPTURE_RADIUS: f32 = 11.0;
/// Seconds an enemy tank must hold station to take a disabled harvester.
pub const CAPTURE_TIME: f32 = 4.0;
/// How fast an owner's tank repairs their own disabled harvester.
pub const RESCUE_REPAIR_RATE: f32 = 11.0;
/// Hull fraction at which a rescued harvester comes back online.
pub const REENABLE_HULL_FRACTION: f32 = 0.25;

/// Applies damage to shields first, then hull. Returns the remaining hull.
///
/// Shields soak everything until they break; overflow from the blow that breaks
/// them carries into the hull, so a big hit is not wasted on a sliver of shield.
pub fn apply_damage(shield: &mut f32, hull: &mut f32, amount: f32) -> f32 {
    let to_shield = amount.min(*shield);
    *shield -= to_shield;
    let overflow = amount - to_shield;
    if overflow > 0.0 {
        *hull = (*hull - overflow).max(0.0);
    }
    *hull
}

// ---------------------------------------------------------------------------
// Collision geometry
// ---------------------------------------------------------------------------

/// Earliest fraction along the segment `a -> b` at which it comes within
/// `radius` of `center`, or `None`.
///
/// Projectiles must be tested as swept segments, not as points. A bullet covers
/// 3 units per tick against a 2.4-unit tank radius, so a point test would let it
/// pass straight through a target between one tick and the next.
pub fn segment_circle_hit(a: Vec2, b: Vec2, center: Vec2, radius: f32) -> Option<f32> {
    let d = b - a;
    let f = a - center;
    let r2 = radius * radius;

    // Already overlapping at the start of the step.
    if f.length_squared() <= r2 {
        return Some(0.0);
    }

    let aa = d.length_squared();
    if aa < 1e-12 {
        return None; // Stationary and not already touching.
    }
    let bb = 2.0 * f.dot(d);
    let cc = f.length_squared() - r2;
    let disc = bb * bb - 4.0 * aa * cc;
    if disc < 0.0 {
        return None;
    }
    let sqrt_disc = disc.sqrt();
    // Near root first; it is the entry point.
    let t = (-bb - sqrt_disc) / (2.0 * aa);
    if (0.0..=1.0).contains(&t) {
        return Some(t);
    }
    let t2 = (-bb + sqrt_disc) / (2.0 * aa);
    if (0.0..=1.0).contains(&t2) { Some(t2) } else { None }
}

/// Earliest fraction along `a -> b` at which it runs into a hill.
///
/// Swept for the same reason vehicle hits are: a bullet crosses 4.5 units in
/// a tick, so a point test would let it flash through the shoulder of a hill.
pub fn segment_hill_hit(a: Vec2, b: Vec2, hills: &[Hill]) -> Option<f32> {
    hills
        .iter()
        .filter_map(|h| segment_circle_hit(a, b, h.pos, h.radius))
        .min_by(|x, y| x.partial_cmp(y).expect("hit fractions are never NaN"))
}

/// True when `pos` is close enough to `player`'s pad to unload ore.
pub fn is_at_base(pos: Vec2, player: u8) -> bool {
    pos.distance(world::base_position(player)) <= world::BASE_RADIUS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::vec2;

    #[test]
    fn a_vehicle_drives_along_its_heading() {
        let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0 };
        for _ in 0..60 {
            step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[], world::TICK_DT);
        }
        // Facing +X at yaw 0, so it must have moved in +X and nowhere else.
        assert!(s.pos.x > 110.0, "expected forward motion, got {:?}", s.pos);
        assert!((s.pos.y - 100.0).abs() < 1e-3, "should not drift sideways");
        assert!(s.speed <= tuning(VehicleKind::Tank).max_speed + 1e-3);
    }

    #[test]
    fn turbo_makes_a_vehicle_faster() {
        let run = |powerups: u16| {
            let mut s = MoveState::default();
            s.pos = vec2(100.0, 100.0);
            for _ in 0..120 {
                step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, powerups, &[], world::TICK_DT);
            }
            s.speed
        };
        let plain = run(0);
        let boosted = run(PowerUp::Turbo.bit());
        assert!(boosted > plain * 1.25, "turbo {boosted} vs plain {plain}");
    }

    #[test]
    fn vehicles_cannot_leave_the_field() {
        let mut s = MoveState { pos: vec2(10.0, 10.0), yaw: std::f32::consts::PI, speed: 0.0 };
        for _ in 0..600 {
            step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[], world::TICK_DT);
        }
        let r = tuning(VehicleKind::Tank).radius;
        assert!(s.pos.x >= r - 1e-3 && s.pos.x <= WORLD_SIZE - r + 1e-3, "{:?}", s.pos);
        assert!(s.pos.y >= r - 1e-3 && s.pos.y <= WORLD_SIZE - r + 1e-3, "{:?}", s.pos);
    }

    /// The point of the feature: a hill is not a slope, it is a wall.
    #[test]
    fn a_vehicle_cannot_drive_onto_a_hill() {
        let hill = Hill { pos: vec2(140.0, 100.0), radius: 9.0 };
        let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0 };
        for _ in 0..300 {
            step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
        }
        let clear = hill.radius + tuning(VehicleKind::Tank).radius;
        assert!(
            s.pos.distance(hill.pos) >= clear - 1e-3,
            "ended {:.2} from the hill centre, needs {clear:.2}",
            s.pos.distance(hill.pos)
        );
    }

    /// Clipping a flank should carry you around it, not pin you to it.
    ///
    /// Two things produce this: pushing back out along the radius leaves the
    /// tangential part of the travel intact, and the speed penalty scales with
    /// how squarely the hill was met, so a graze costs little.
    #[test]
    fn a_glancing_approach_slides_around_a_hill() {
        let hill = Hill { pos: vec2(130.0, 100.0), radius: 9.0 };
        let clear = hill.radius + tuning(VehicleKind::Tank).radius;
        // Offset enough that the hull catches the shoulder rather than the face.
        let mut s = MoveState { pos: vec2(100.0, 110.0), yaw: 0.0, speed: 0.0 };
        for _ in 0..150 {
            step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
        }
        assert!(s.pos.distance(hill.pos) >= clear - 1e-3, "ended inside the hill: {:?}", s.pos);
        assert!(s.pos.x > hill.pos.x + hill.radius, "should have got past, at {:?}", s.pos);
        assert!(s.pos.y > 110.0, "should have been pushed aside, at {:?}", s.pos);
        // The one that matters: a graze is not a stop.
        assert!(
            s.speed > tuning(VehicleKind::Tank).max_speed * 0.8,
            "left the hill at {:.1}, barely slowed is the point",
            s.speed
        );
    }

    /// A vehicle that somehow starts inside a hill has to be let out, not
    /// trapped: the alternative is a vehicle stuck for the rest of the match.
    #[test]
    fn a_vehicle_inside_a_hill_is_pushed_clear() {
        let hill = Hill { pos: vec2(100.0, 100.0), radius: 9.0 };
        let mut s = MoveState { pos: hill.pos, yaw: 0.7, speed: 0.0 };
        step_vehicle(&mut s, 0.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
        let clear = hill.radius + tuning(VehicleKind::Tank).radius;
        assert!((s.pos.distance(hill.pos) - clear).abs() < 1e-3, "{:?}", s.pos);
    }

    #[test]
    fn a_shell_stops_at_the_near_face_of_a_hill() {
        let hills = [Hill { pos: vec2(40.0, 0.0), radius: 5.0 }];
        let t = segment_hill_hit(vec2(0.0, 0.0), vec2(100.0, 0.0), &hills).unwrap();
        assert!((t - 0.35).abs() < 1e-4, "entered at {t}, expected the near face at 0.35");
        // Past the shoulder is a clean miss.
        assert!(segment_hill_hit(vec2(0.0, 8.0), vec2(100.0, 8.0), &hills).is_none());
        assert!(segment_hill_hit(vec2(0.0, 0.0), vec2(100.0, 0.0), &[]).is_none());
    }

    #[test]
    fn stepping_is_deterministic() {
        let run = || {
            let mut s = MoveState { pos: vec2(40.0, 60.0), yaw: 0.3, speed: 2.0 };
            for i in 0..500 {
                let throttle = if i % 7 == 0 { -1.0 } else { 1.0 };
                let steer = ((i % 13) as f32 - 6.0) / 6.0;
                step_vehicle(&mut s, throttle, steer, VehicleKind::Harvester, 0, &[], world::TICK_DT);
            }
            s
        };
        assert_eq!(run(), run());
    }

    /// The reason projectiles are swept rather than point-tested.
    ///
    /// A bullet advances 3 units per tick against a 4.8-unit-wide tank, so a
    /// shot through the middle would still be caught by sampling the endpoints.
    /// A *glancing* shot would not: the chord it crosses is under 2 units, so
    /// both tick positions fall outside the hull and a point test scores a clean
    /// miss on what the player plainly saw connect.
    #[test]
    fn a_glancing_bullet_cannot_tunnel_through_a_tank() {
        let target = vec2(100.0, 100.0);
        let radius = tuning(VehicleKind::Tank).radius;
        let step = BULLET_SPEED * world::TICK_DT;

        // Offset just inside the hull, so the traversed chord is short.
        let offset = radius - 0.2;
        let chord = 2.0 * (radius * radius - offset * offset).sqrt();
        assert!(chord < step, "chord {chord} must be shorter than a {step} unit step");

        let a = vec2(target.x - step * 0.5, target.y + offset);
        let b = vec2(target.x + step * 0.5, target.y + offset);
        assert!(
            a.distance(target) > radius && b.distance(target) > radius,
            "both tick positions must lie outside the hull for this to be a tunneling case"
        );
        assert!(segment_circle_hit(a, b, target, radius).is_some(), "bullet tunneled through");
    }

    #[test]
    fn segment_circle_reports_the_entry_point_and_misses() {
        let c = vec2(10.0, 0.0);
        let t = segment_circle_hit(vec2(0.0, 0.0), vec2(20.0, 0.0), c, 2.0).unwrap();
        assert!((t - 0.4).abs() < 1e-4, "entry at {t}, expected 0.4");

        assert!(segment_circle_hit(vec2(0.0, 9.0), vec2(20.0, 9.0), c, 2.0).is_none());
        // Starting already inside counts as an immediate hit.
        assert_eq!(segment_circle_hit(c, vec2(20.0, 0.0), c, 2.0), Some(0.0));
        // A stationary projectile away from the target does not hit.
        assert!(segment_circle_hit(vec2(0.0, 0.0), vec2(0.0, 0.0), c, 2.0).is_none());
    }

    #[test]
    fn damage_spills_from_shield_into_hull() {
        let (mut shield, mut hull) = (10.0f32, 100.0f32);
        apply_damage(&mut shield, &mut hull, 30.0);
        assert_eq!(shield, 0.0);
        assert_eq!(hull, 80.0, "the 20 points past the shield must reach the hull");

        let (mut shield, mut hull) = (50.0f32, 100.0f32);
        apply_damage(&mut shield, &mut hull, 30.0);
        assert_eq!((shield, hull), (20.0, 100.0));

        // Hull never goes negative.
        let (mut shield, mut hull) = (0.0f32, 5.0f32);
        assert_eq!(apply_damage(&mut shield, &mut hull, 999.0), 0.0);
    }

    #[test]
    fn armor_raises_only_the_harvester_hull() {
        let mask = PowerUp::HarvesterArmor.bit();
        assert!(max_hull(VehicleKind::Harvester, mask) > max_hull(VehicleKind::Harvester, 0));
        assert_eq!(max_hull(VehicleKind::Tank, mask), max_hull(VehicleKind::Tank, 0));
        assert!(cargo_capacity(mask) > cargo_capacity(0));
    }
}
