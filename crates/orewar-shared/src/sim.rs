//! Vehicle physics, tuning, and combat geometry.
//!
//! Every function here is a pure function of state and input. The server runs
//! them to produce authoritative state; the client runs the *same* functions at
//! the *same* fixed rate to predict its own vehicle. Any divergence between the
//! two shows up as prediction error the client has to smooth away, so this
//! module must stay free of wall-clock time, randomness, and floating frame
//! deltas.

use crate::math::{Vec2, angle_approach, angle_delta, approach, wrap_angle};
use crate::world::{self, Hill, PowerUp, WORLD_SIZE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum VehicleKind {
    Tank = 0,
    Miner = 1,
    /// The bomber. Flown, not driven: it holds one cruising speed, turns with
    /// the stick, and passes over everything the other two have to go around.
    Plane = 2,
}

impl VehicleKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(VehicleKind::Tank),
            1 => Some(VehicleKind::Miner),
            2 => Some(VehicleKind::Plane),
            _ => None,
        }
    }

    /// Whether this one is in the air, and so out of everything that happens
    /// on the ground: hills, hulls, shells, and being captured.
    pub fn flies(self) -> bool {
        matches!(self, VehicleKind::Plane)
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
        VehicleKind::Miner => VehicleTuning {
            max_speed: 11.5,
            reverse_speed: 6.0,
            accel: 15.0,
            brake: 26.0,
            turn_rate: 1.6,
            radius: 2.9,
            base_shield: 150.0,
            base_hull: 130.0,
        },
        // Faster than anything on the ground and slower than a missile, and
        // with no reverse: the throttle trims the cruise rather than stopping
        // it, because an aircraft that can be parked in the air is a gun
        // platform and not a bombing run. It turns more slowly than a tank
        // pivots, so a run has to be set up before it is flown.
        //
        // `radius` is what keeps it inside the field. It has no bearing on
        // collision: nothing at altitude is in the collision pass at all.
        VehicleKind::Plane => VehicleTuning {
            max_speed: PLANE_CRUISE,
            reverse_speed: 0.0,
            accel: 30.0,
            brake: 30.0,
            turn_rate: 1.5,
            radius: 5.0,
            base_shield: 0.0,
            base_hull: 1.0,
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
    if kind == VehicleKind::Miner && PowerUp::MinerArmor.held(powerups) {
        base + 60.0
    } else {
        base
    }
}

pub fn speed_multiplier(powerups: u16) -> f32 {
    if PowerUp::Turbo.held(powerups) { 1.3 } else { 1.0 }
}

/// How long a shell fired by a player's *tank* lives.
pub fn bullet_lifetime(powerups: u16) -> f32 {
    if PowerUp::LongBarrel.held(powerups) {
        BULLET_LIFETIME * LONG_BARREL_MULTIPLIER
    } else {
        BULLET_LIFETIME
    }
}

/// Shell life for a gun that engages at a fixed range of its own.
///
/// The emplacement in a player's corner and the miner's auto turret stop
/// firing at [`SENTINEL_RANGE`] and [`AUTO_TURRET_RANGE`], so their shells are
/// sized from those rather than from the tank's gun -- which is tuned for how a
/// gunfight should feel and has been shortened twice.
///
/// A gun that fires at what its shell cannot reach is just noise: it leaves a
/// ring at the edge of its range where it keeps shooting and nothing lands.
/// Sharing the tank's number had already put the auto turret about two units
/// inside that, and left the emplacement under one unit clear of it.
///
/// The quarter on top is for a target still moving away when the shot leaves.
pub fn shell_life_covering(range: f32) -> f32 {
    range * 1.25 / BULLET_SPEED
}

pub fn cargo_capacity(powerups: u16) -> f32 {
    let base = 60.0;
    if PowerUp::MinerArmor.held(powerups) { base * 1.25 } else { base }
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
    /// Bank angle in radians; positive rolls right. Only the aircraft uses it,
    /// and for it this is not decoration -- the bank is what makes the turn, so
    /// it is state the server owns and the client has to predict like any
    /// other. Ground vehicles leave it at zero.
    pub roll: f32,
}

/// What a step ran into, for a caller that needs to do more than move.
///
/// [`step_vehicle`] stays a pure function of movement and never applies damage:
/// damage is the server's to decide, and the client runs this same function to
/// predict its own vehicle. So a collision is reported back rather than acted
/// on, and the client is free to ignore it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StepOutcome {
    /// Speed carried into terrain this tick, measured before the impact bled it
    /// off. Zero when nothing was hit.
    pub terrain_impact: f32,
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
#[must_use = "the outcome reports impacts the caller may need to charge for"]
pub fn step_vehicle(
    state: &mut MoveState,
    throttle: f32,
    steer: f32,
    kind: VehicleKind,
    powerups: u16,
    hills: &[Hill],
    dt: f32,
) -> StepOutcome {
    let mut outcome = StepOutcome::default();
    let t = tuning(kind);
    let throttle = throttle.clamp(-1.0, 1.0);
    let steer = steer.clamp(-1.0, 1.0);
    let mult = speed_multiplier(powerups);

    if kind.flies() {
        step_plane(state, throttle, steer, dt);
        return outcome;
    }

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
        // Which way the hull is actually travelling, not which way it faces:
        // reversing into a slope is the same collision as driving into one,
        // and under turbo a tank backs up faster than the threshold that
        // hurts. Taking yaw alone made backwards a free direction.
        let travel = Vec2::from_angle(state.yaw) * state.speed.signum();
        let head_on = (-out).dot(travel).clamp(0.0, 1.0);
        // Read before the impact bleeds it off, and keep the worst of the tick:
        // this is what the server charges the hull for.
        outcome.terrain_impact = outcome.terrain_impact.max(state.speed.abs() * head_on);
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

    outcome
}

/// Advances the bomber by one tick.
///
/// Split out of [`step_vehicle`] rather than folded into it because almost
/// nothing there applies: an aircraft has no reverse, no hills to go round, and
/// no wall to be stopped by. What it shares is that both the server and the
/// client run it, so it stays as pure as the rest of this module.
///
/// The throttle trims the cruise rather than setting it. Forward and back move
/// the airspeed [`PLANE_SPEED_TRIM`] either way and no further, so it can be
/// pushed along or held back but never brought near a stop -- an aircraft that
/// can be parked in the air is a gun platform, and the fuel clock stops meaning
/// much if you can sit still on it.
///
/// What the trim buys is a real choice rather than a bigger number. Slow, and a
/// run is easier to line up and the bombs land closer together; the fuel clock
/// is unchanged, so the same eighteen seconds covers less ground. Fast, and the
/// map shrinks at the cost of a run that has to be set up much further out.
///
/// **The stick rolls; the roll turns.** Left and right do not steer the
/// aircraft -- they bank it, and a banked aircraft comes round on its own at a
/// rate set by how far over it is. Let go and the bank *stays* where it was
/// put, so the turn continues until it is rolled back level. That is the whole
/// difference between flying this and driving the other two: a tank goes where
/// it is pointed the moment you ask, and an aircraft has to be set up into a
/// turn and then taken out of it again.
///
/// The rate comes off the tangent of the bank, which is what it is in the air:
/// the horizontal component of lift is what pulls an aircraft round, and it
/// grows faster than the angle does. In the hand it means a shallow bank is a
/// wide, correctable arc and the last few degrees of a hard one are where the
/// turn really bites.
///
/// Flying out of the field ends the sortie, and nothing stops you doing it.
/// The edge used to bank the aircraft back inside, which kept every sortie over
/// the field but meant the boundary quietly flew the aeroplane for you -- and
/// near a corner base, which is one of the things worth crossing the map to
/// bomb, it wrestled you off the run every time you lined one up. Leaving is
/// now simply a way to lose the aircraft, which is a price the player can see
/// coming and choose to pay.
fn step_plane(state: &mut MoveState, throttle: f32, steer: f32, dt: f32) {
    let t = tuning(VehicleKind::Plane);
    let target = PLANE_CRUISE + throttle.clamp(-1.0, 1.0) * PLANE_SPEED_TRIM;
    state.speed = approach(state.speed, target, t.accel * dt);
    state.roll =
        (state.roll + steer * PLANE_ROLL_RATE * dt).clamp(-PLANE_MAX_BANK, PLANE_MAX_BANK);

    // Lift leans over with the wings, and its sideways part is the turn.
    // Normalised on the tangent at full bank so `PLANE_TURN_RATE` stays the
    // number that says how fast a hard turn comes round.
    let rate = t.turn_rate * state.roll.tan() / PLANE_MAX_BANK.tan();
    state.yaw = wrap_angle(state.yaw + rate * dt);
    state.pos += Vec2::from_angle(state.yaw) * (state.speed * dt);
}

/// Whether a sortie has flown out of the match.
///
/// Measured past the wall rather than at it, so the aircraft is seen to leave
/// -- crossing the boundary and vanishing on the same frame reads as a bug
/// rather than as a decision the player just made.
pub fn plane_has_left(pos: Vec2) -> bool {
    pos.x < -PLANE_EXIT_MARGIN
        || pos.y < -PLANE_EXIT_MARGIN
        || pos.x > WORLD_SIZE + PLANE_EXIT_MARGIN
        || pos.y > WORLD_SIZE + PLANE_EXIT_MARGIN
}

/// Where a bomb released now will land.
///
/// A bomb keeps the velocity it was let go with and falls for
/// [`BOMB_FALL_TIME`], so the impact point is pure geometry. Both the server
/// and the bombsight on the client call this, which is what makes the sight
/// honest rather than an approximation of it.
pub fn bomb_impact(pos: Vec2, yaw: f32, speed: f32) -> Vec2 {
    pos + Vec2::from_angle(yaw) * (speed * BOMB_FALL_TIME)
}

/// Damage a blast does at `distance` from where it went off.
///
/// Full damage at the centre falling linearly to nothing at the rim, rather
/// than all-or-nothing inside a circle: a near miss should hurt and a far one
/// should not, and a cliff edge between the two makes a weapon that is either
/// wasted or decisive with nothing in between.
pub fn blast_damage(distance: f32, radius: f32, max: f32) -> f32 {
    if distance >= radius {
        return 0.0;
    }
    max * (1.0 - distance / radius)
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

/// How long a shell flies before it falls short, and so how far it reaches:
/// [`BULLET_SPEED`] times this, about 50 units of a 480-unit field.
///
/// A tenth of the field, and [`PowerUp::LongBarrel`] doubles it to a fifth. At
/// that reach a gunfight is fought where both hulls are already committed:
/// there is no standing off and trading.
///
/// Neither figure comes near [`SENTINEL_RANGE`], which is deliberate -- an
/// emplacement is not something a tank can outrange, and a missile is what
/// answers one from outside its reach.
pub const BULLET_LIFETIME: f32 = 0.55;
pub const BULLET_COOLDOWN: f32 = 0.22;

/// What [`PowerUp::LongBarrel`] multiplies a tank shell's reach by.
pub const LONG_BARREL_MULTIPLIER: f32 = 2.0;

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

// ---------------------------------------------------------------------------
// The bomber
// ---------------------------------------------------------------------------

/// How high the bomber flies, in world units.
///
/// High enough to be plainly out of the fight rather than a very fast tank:
/// nothing on the ground reaches it, and it reaches the ground only by letting
/// something fall. It is also what the bombs fall through, so it and
/// [`BOMB_FALL_TIME`] have to move together.
pub const PLANE_ALTITUDE: f32 = 26.0;

/// Airspeed with the stick centred. Getting on twice a tank's top speed, so
/// crossing ground a tank has to fight across is most of what the aircraft is
/// for.
pub const PLANE_CRUISE: f32 = 34.0;

/// How far the throttle moves the airspeed either side of [`PLANE_CRUISE`].
///
/// Bounded by the fastest thing on the ground rather than picked for feel: held
/// hard back the aircraft still has to outrun a tank under Turbo, or the slow
/// end stops reading as an aircraft at all. That leaves a shade under nine, and
/// eight is the round number inside it.
///
/// It is enough to matter. Across the range the bombs land some twenty-two
/// units apart -- nearly two blast radii -- which is the difference between
/// walking a stick onto a target and dropping it short.
pub const PLANE_SPEED_TRIM: f32 = 8.0;

/// How fast the stick rolls the aircraft, in radians per second.
///
/// A full stick crosses the whole range of bank in about three quarters of a
/// second. Quick enough that a turn can be started without ceremony, slow
/// enough that rolling out of one is a thing you have to plan a moment ahead --
/// which is what stops the aircraft handling like a tank that happens to fly.
pub const PLANE_ROLL_RATE: f32 = 1.9;

/// How far over it will go, in radians. Sixty degrees.
pub const PLANE_MAX_BANK: f32 = 1.047;

/// How far past the wall a sortie flies before it is gone for good.
///
/// Comfortably more than the aircraft is long, so it leaves the field rather
/// than blinking out on the boundary.
pub const PLANE_EXIT_MARGIN: f32 = 14.0;

/// Seconds of fuel in one sortie.
///
/// At [`PLANE_CRUISE`] this is about 610 units of flying, against a field 480
/// across and 679 corner to corner: enough to reach anywhere from your own
/// corner and bomb it, and not enough to loiter once you are there.
pub const PLANE_FUEL: f32 = 18.0;

/// Seconds between one sortie ending and the next being available.
///
/// Measured from the fuel running out, so a short run does not buy a quick
/// second one. Long enough that the aircraft is an event in a match rather
/// than a weapon you fly around in.
pub const SORTIE_COOLDOWN: f32 = 45.0;

/// How long a bomb falls before it goes off.
///
/// With [`PLANE_CRUISE`] this throws the impact point about 47 units ahead of
/// where the aircraft was when the bomb left it, which is why there is a sight
/// on the ground: judging that by eye from 26 units up is guesswork.
pub const BOMB_FALL_TIME: f32 = 1.4;

/// Seconds between releases.
pub const BOMB_COOLDOWN: f32 = 0.55;

/// How far from the middle of a blast anything is hurt at all.
///
/// Wide enough that a bomb is aimed at a place rather than at a hull, which is
/// the difference between this and every other weapon in the game.
pub const BOMB_BLAST_RADIUS: f32 = 13.0;

/// Damage a bomb does at the very centre of its blast, falling off to nothing
/// at [`BOMB_BLAST_RADIUS`].
///
/// Set by what a direct hit should be worth rather than by comparison with the
/// other weapons, because landing one is nothing like firing them. A bomb is
/// released some 47 units before it arrives, from an aircraft that is committed
/// to a line and cannot stop, out of a sortie that comes round every
/// forty-five seconds at best. Against anything that is moving and paying
/// attention, most bombs miss.
///
/// So the number is the answer to "what is a hit worth": everything a
/// miner's shield has, and half of what is under it. That is 150 and 65 of
/// a miner's 130, and it falls out of [`apply_damage`] spilling the
/// remainder from one into the other.
///
/// Two consequences worth knowing. A base tank has 210 between shield and hull
/// and so does not survive a square hit at all -- which is the point of a
/// weapon this hard to land. And because this is flat damage, like every other
/// weapon here, upgrades are still worth having: a miner carrying both
/// Shield Booster and Armour keeps almost all of its hull.
pub const BOMB_DAMAGE: f32 = 215.0;

// ---------------------------------------------------------------------------
// Impacts
// ---------------------------------------------------------------------------

/// Speed at which running into something starts to hurt.
///
/// Below this a bump is just a bump: nosing up to a hill, or shunting a hull
/// out of the way at low speed, costs nothing. Half a tank's top speed, so it
/// takes a deliberate run-up.
pub const IMPACT_THRESHOLD: f32 = 9.5;

/// Hull damage per unit of speed above [`IMPACT_THRESHOLD`] when a vehicle
/// drives into terrain. A tank at full tilt into a hillside pays about what one
/// shell costs it.
pub const TERRAIN_IMPACT_DAMAGE: f32 = 2.0;

/// Damage per unit of *closing* speed above [`IMPACT_THRESHOLD`] when two
/// vehicles meet, charged to both of them. Lower than terrain because two tanks
/// driving at each other close at twice their own speed.
pub const RAM_DAMAGE: f32 = 0.6;

/// How much of the speed a hull was carrying into another hull it loses.
pub const RAM_SPEED_LOSS: f32 = 0.55;

/// Seconds without taking damage before shields begin to regenerate.
pub const SHIELD_REGEN_DELAY: f32 = 4.0;

/// How long losing your miner keeps you off the field.
///
/// Long enough to be the worst thing that can happen to you and short enough
/// that it is a setback rather than the end of your match -- as long as there
/// is somebody else still playing to come back to. In a two-player match there
/// is not, and losing your miner loses it: the win goes to whoever is left
/// on the field, and it goes the moment the capture lands.
pub const CAPTURE_LOCKOUT: f32 = 60.0;

/// Captures that win a match outright, without waiting to clear the field.
///
/// The ending that matters is being the last one on the field. This is the
/// other way home for a bigger match, where a capture keeps putting somebody
/// off for a minute and everybody keeps coming back: take three miners and
/// it is yours whoever is still standing.
pub const CAPTURES_TO_WIN: u8 = 3;

/// How far a base emplacement will engage.
///
/// It is the thing that makes walking into somebody's corner cost something,
/// and it reaches a long way out to do it: a third of the field, far enough
/// that a tank crosses open ground under fire long before it can answer.
/// Nothing a tank can bring outranges it -- a plain shell reaches about a
/// tenth of the field and [`PowerUp::LongBarrel`] a fifth -- so a base is
/// taken by closing on it and wearing the gun down, not by standing off.
///
/// Its shells are sized from this by [`shell_life_covering`], so moving it
/// cannot leave the gun firing at what it can no longer hit. What this does
/// not buy is accuracy: the emplacement aims where a target *is*, and a shell
/// takes nearly two seconds to cross that range, so out at the edge it
/// punishes anything parked, approaching, or leaving rather than anything
/// crossing.
pub const SENTINEL_RANGE: f32 = 165.0;
/// One shell a second.
pub const SENTINEL_COOLDOWN: f32 = 1.0;
pub const SENTINEL_HULL: f32 = 120.0;
/// Seconds of rubble before it comes back at full hull. Long enough that
/// silencing one buys a real window, short enough that it is not a kill.
pub const SENTINEL_REBUILD: f32 = 20.0;
/// Collision radius for incoming fire.
pub const SENTINEL_RADIUS: f32 = 2.6;

pub const AUTO_TURRET_RANGE: f32 = 42.0;
pub const AUTO_TURRET_COOLDOWN: f32 = 0.9;

/// Seconds before a destroyed tank returns to its base pad.
pub const TANK_RESPAWN_DELAY: f32 = 8.0;

// Economy.
pub const MINING_RADIUS: f32 = 7.5;
pub const MINING_RATE: f32 = 15.0;
/// A miner must be nearly stationary to draw ore.
pub const MINING_MAX_SPEED: f32 = 4.0;
pub const UNLOAD_RATE: f32 = 50.0;

// Capture.
/// How close a tank has to be to a disabled miner to work on it.
///
/// Just past touching: the two hulls meet at 2.4 + 2.9 = 5.3 and vehicles no
/// longer share space, so anything at or below that could never be reached.
/// Small on purpose -- at 11.0 a tank covered its own miner from where it
/// spawned, so a defender never had to do anything to deny a capture.
pub const CAPTURE_RADIUS: f32 = 6.5;
/// Seconds an enemy tank must hold station to take a disabled miner.
pub const CAPTURE_TIME: f32 = 4.0;
/// How fast an owner's tank repairs their own disabled miner.
///
/// Deliberately slow enough that a rescue takes longer than [`CAPTURE_TIME`].
/// At 11.0 it took three seconds against a four-second capture, so disabling a
/// miner achieved nothing: the attacker still had to cross the ground to
/// the wreck and then hold it, while the defender only had to already be
/// standing there -- which they are, since a tank spawns and respawns inside
/// [`CAPTURE_RADIUS`] of its own miner. The wreck was repaired before the
/// attacker could arrive, every time.
pub const RESCUE_REPAIR_RATE: f32 = 4.5;
/// Hull fraction at which a rescued miner comes back online.
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

/// How far two overlapping circles have to move apart, and along which axis.
///
/// `None` when they are already clear. The axis points from `a` toward `b`, so
/// `a` backs off along it and `b` moves forward; how the distance is split
/// between them is the caller's call, because a wreck gives no ground.
pub fn overlap_push(a: Vec2, ra: f32, b: Vec2, rb: f32) -> Option<(Vec2, f32)> {
    let touching = ra + rb;
    let offset = b - a;
    let dist = offset.length();
    if dist >= touching {
        return None;
    }
    // Exactly co-located leaves no axis to separate along. Pick one rather than
    // dividing by zero and welding the two hulls together for the rest of the
    // match.
    let axis = if dist > 1e-4 { offset / dist } else { Vec2::new(1.0, 0.0) };
    Some((axis, touching - dist))
}

// ---------------------------------------------------------------------------
// Autopilot
// ---------------------------------------------------------------------------

/// Cruise throttle for an unattended miner, as a fraction of full.
///
/// Deliberately slow. An autopilot miner is a floor on your income, not a
/// replacement for driving one -- taking it over yourself should be visibly
/// worth doing. It also keeps the cruise well under [`IMPACT_THRESHOLD`], so a
/// miner left to itself can never crash into a hillside hard enough to hurt.
pub const AUTOPILOT_CRUISE: f32 = 0.4;

/// How wide of a hill the autopilot tries to pass.
pub const AUTOPILOT_CLEARANCE: f32 = 4.0;

/// Below this the autopilot decides it is wedged and backs off.
pub const AUTOPILOT_STALL_SPEED: f32 = 0.5;

/// Heading to actually steer for, given where the autopilot wants to end up.
///
/// The direct bearing, unless a hill sits across it -- then the tangent past the
/// side the target is already on. Driving straight at a target behind a hill
/// parks the miner against the slope indefinitely: the push-out in
/// [`step_vehicle`] holds it clear, and it drives straight back in.
pub fn autopilot_bearing(from: Vec2, to: Vec2, hills: &[Hill]) -> f32 {
    let direct = (to - from).to_angle();
    let travel = from.distance(to);

    // The nearest hill that the path actually crosses. Only the first one
    // matters; rounding it changes the picture for whatever comes after.
    let mut blocking: Option<(f32, Vec2, f32)> = None;
    for hill in hills {
        let radius = hill.radius + AUTOPILOT_CLEARANCE;
        let offset = hill.pos - from;
        let along = offset.dot(Vec2::from_angle(direct));
        // Behind us, or beyond the target: not in the way.
        if along <= 0.0 || along - radius > travel {
            continue;
        }
        let lateral = (offset.length_squared() - along * along).max(0.0).sqrt();
        if lateral >= radius {
            continue;
        }
        if blocking.is_none_or(|(best, _, _)| along < best) {
            blocking = Some((along, hill.pos, radius));
        }
    }

    let Some((_, centre, radius)) = blocking else { return direct };

    let offset = centre - from;
    let distance = offset.length();
    let to_hill = offset.to_angle();
    if distance <= radius {
        // Already inside the margin: the only useful heading is straight out.
        return wrap_angle(to_hill + std::f32::consts::PI);
    }
    // Pass on the side the target is already on, so rounding the hill makes
    // progress instead of doubling the journey.
    let side = if angle_delta(to_hill, direct) >= 0.0 { 1.0 } else { -1.0 };
    wrap_angle(to_hill + side * (radius / distance).clamp(-1.0, 1.0).asin())
}

/// Throttle and steer for a miner driving itself to `target`.
///
/// Returns neutral once inside `stop_within`, so the vehicle brakes and settles
/// under [`MINING_MAX_SPEED`] rather than circling the thing it came for.
pub fn autopilot(state: &MoveState, target: Vec2, stop_within: f32, hills: &[Hill]) -> (f32, f32) {
    if state.pos.distance(target) <= stop_within {
        return (0.0, 0.0);
    }
    let bearing = autopilot_bearing(state.pos, target, hills);
    let turn = angle_delta(state.yaw, bearing);
    // Proportional, and saturating well before the error is large, so it holds
    // a line instead of weaving.
    let steer = (turn * 2.0).clamp(-1.0, 1.0);
    // Straighten up before building speed; a hard turn under power swings wide.
    let throttle = if turn.abs() > 1.0 { 0.0 } else { AUTOPILOT_CRUISE };
    (throttle, steer)
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
        let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        for _ in 0..60 {
            let _ = step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[], world::TICK_DT);
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
                let _ = step_vehicle(
                    &mut s,
                    1.0,
                    0.0,
                    VehicleKind::Tank,
                    powerups,
                    &[],
                    world::TICK_DT,
                );
            }
            s.speed
        };
        let plain = run(0);
        let boosted = run(PowerUp::Turbo.bit());
        assert!(boosted > plain * 1.25, "turbo {boosted} vs plain {plain}");
    }

    #[test]
    fn vehicles_cannot_leave_the_field() {
        let mut s = MoveState { pos: vec2(10.0, 10.0), yaw: std::f32::consts::PI, speed: 0.0, roll: 0.0 };
        for _ in 0..600 {
            let _ = step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[], world::TICK_DT);
        }
        let r = tuning(VehicleKind::Tank).radius;
        assert!(s.pos.x >= r - 1e-3 && s.pos.x <= WORLD_SIZE - r + 1e-3, "{:?}", s.pos);
        assert!(s.pos.y >= r - 1e-3 && s.pos.y <= WORLD_SIZE - r + 1e-3, "{:?}", s.pos);
    }

    /// The point of the feature: a hill is not a slope, it is a wall.
    #[test]
    fn a_vehicle_cannot_drive_onto_a_hill() {
        let hill = Hill { pos: vec2(140.0, 100.0), radius: 9.0 };
        let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        for _ in 0..300 {
            let _ = step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
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
        let mut s = MoveState { pos: vec2(100.0, 110.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        for _ in 0..150 {
            let _ = step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
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
        let mut s = MoveState { pos: hill.pos, yaw: 0.7, speed: 0.0, roll: 0.0 };
        let _ = step_vehicle(&mut s, 0.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
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

    /// A hill stops a tank and means nothing to an aircraft.
    ///
    /// This is the whole point of the bomber: it reaches ground a tank has to
    /// go around. `step_vehicle` takes the hills for everyone, so the plane
    /// branch has to be the thing that ignores them -- if it ever fell through
    /// to the ground path, the aircraft would be shouldered aside in mid-air by
    /// terrain it is 26 units above.
    #[test]
    fn hills_are_nothing_to_an_aircraft() {
        let hill = Hill { pos: vec2(160.0, 100.0), radius: 12.0 };
        let fly = |hills: &[Hill]| {
            let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: PLANE_CRUISE, roll: 0.0 };
            for _ in 0..90 {
                let _ = step_vehicle(&mut s, 0.0, 0.0, VehicleKind::Plane, 0, hills, world::TICK_DT);
            }
            s.pos
        };
        assert_eq!(fly(&[]), fly(&[hill]), "the hill moved the aircraft");
        assert!(fly(&[hill]).x > 180.0, "it should have flown clean over and past");

        // The same run on the ground is stopped by it.
        let mut tank = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        for _ in 0..90 {
            let _ = step_vehicle(&mut tank, 1.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
        }
        assert!(tank.pos.x < 150.0, "the tank should be stopped at the hill: {:?}", tank.pos);
    }

    /// The throttle trims the airspeed, and cannot bring it near a stop.
    ///
    /// Both halves matter. Without the trim there is no reason to touch the
    /// throttle at all; without the floor under it the bomber becomes something
    /// that can be parked over a target, which is a gun platform rather than an
    /// aircraft and makes the fuel clock meaningless.
    #[test]
    fn the_throttle_trims_the_airspeed_but_never_stops_it() {
        let settle = |throttle: f32| {
            let mut s =
                MoveState { pos: vec2(240.0, 240.0), yaw: 0.0, speed: PLANE_CRUISE, roll: 0.0 };
            for _ in 0..120 {
                let _ =
                    step_vehicle(&mut s, throttle, 0.0, VehicleKind::Plane, 0, &[], world::TICK_DT);
            }
            s.speed
        };

        let slow = settle(-1.0);
        let level = settle(0.0);
        let fast = settle(1.0);
        assert!((level - PLANE_CRUISE).abs() < 1e-3, "hands off settled at {level}");
        assert!((slow - (PLANE_CRUISE - PLANE_SPEED_TRIM)).abs() < 1e-3, "slow was {slow}");
        assert!((fast - (PLANE_CRUISE + PLANE_SPEED_TRIM)).abs() < 1e-3, "fast was {fast}");

        // Even held hard back it outruns anything on the ground, so slowing
        // down is a choice about the run and never a way to loiter.
        let fastest_tank = tuning(VehicleKind::Tank).max_speed * speed_multiplier(
            crate::world::PowerUp::Turbo.bit(),
        );
        assert!(slow > fastest_tank, "held back it does {slow}, under a tank's {fastest_tank}");
    }

    /// Changing speed changes where the bombs go, and the sight knows it.
    ///
    /// The throw ahead is the airspeed times the fall, so a slower run puts the
    /// bombs closer in. That is most of the reason to touch the throttle, and
    /// it only works because the sight is computed from the speed the aircraft
    /// is actually doing rather than from the cruise it started at.
    #[test]
    fn a_slower_run_drops_its_bombs_shorter() {
        let throw = |throttle: f32| {
            let mut s =
                MoveState { pos: vec2(240.0, 240.0), yaw: 0.0, speed: PLANE_CRUISE, roll: 0.0 };
            for _ in 0..120 {
                let _ =
                    step_vehicle(&mut s, throttle, 0.0, VehicleKind::Plane, 0, &[], world::TICK_DT);
            }
            bomb_impact(s.pos, s.yaw, s.speed).distance(s.pos)
        };
        let slow = throw(-1.0);
        let fast = throw(1.0);
        assert!(fast > slow + 20.0, "the throttle barely moved the aim point: {slow} to {fast}");
    }

    /// The stick banks the aircraft, and the bank is what turns it.
    ///
    /// Three things at once, because they are one behaviour: nothing happens to
    /// the heading until a wing goes down, the bank stays where it was put when
    /// the stick is let go, and the aircraft keeps coming round for as long as
    /// it is over. This is what makes flying it different from driving a tank,
    /// where letting go of the stick stops the turn dead.
    #[test]
    fn the_stick_banks_the_aircraft_and_the_bank_turns_it() {
        let fly = |steer: f32, ticks: usize, s: &mut MoveState| {
            for _ in 0..ticks {
                let _ = step_vehicle(s, 0.0, steer, VehicleKind::Plane, 0, &[], world::TICK_DT);
            }
        };

        // Wings level, stick central: it flies straight.
        let mut s = MoveState { pos: vec2(240.0, 240.0), yaw: 0.0, speed: PLANE_CRUISE, roll: 0.0 };
        fly(0.0, 30, &mut s);
        assert_eq!(s.roll, 0.0, "the wings dropped on their own");
        assert!(s.yaw.abs() < 1e-4, "it turned without being banked: {}", s.yaw);

        // Hold the stick over: it rolls, and once it is over it comes round.
        fly(1.0, 30, &mut s);
        assert!(s.roll > 0.4, "a second of stick only reached {} of bank", s.roll);
        let turned_while_rolling = s.yaw;
        assert!(turned_while_rolling > 0.0, "banked and still flying straight");

        // Let go: the bank is *held*, so the turn carries on by itself. This is
        // the property that was asked for -- an aircraft does not level itself.
        let held = s.roll;
        fly(0.0, 30, &mut s);
        assert!(
            (s.roll - held).abs() < 1e-4,
            "the bank washed out on its own: {held} became {}",
            s.roll
        );
        assert!(
            s.yaw > turned_while_rolling + 0.5,
            "the turn stopped when the stick was let go"
        );

        // And it only stops when it is rolled back level. Rolled back until the
        // wings pass level rather than for a fixed time: the stick keeps
        // rolling for as long as it is held, so holding it too long simply
        // arrives at the same bank the other way round.
        let mut ticks = 0;
        while s.roll > 0.0 && ticks < 60 {
            fly(-1.0, 1, &mut s);
            ticks += 1;
        }
        assert!(s.roll.abs() < 0.08, "rolling back did not level it: {}", s.roll);
        let heading = s.yaw;
        fly(0.0, 30, &mut s);
        // Near enough level is near enough straight. Not exactly zero: the
        // stick rolls at a fixed rate, so stopping it lands a hair either side
        // of level and a hair of bank is a hair of turn. What matters is that
        // the turn has gone from the rate a full bank holds to nothing worth
        // measuring.
        let drift = angle_delta(heading, s.yaw).abs();
        assert!(drift < 0.05, "wings level and still turning {drift} in a second");
    }

    /// Bank harder, turn tighter.
    ///
    /// The rate comes off the tangent of the bank, so it is not merely that
    /// more bank turns faster -- the last part of the range has to be worth
    /// reaching for, or there is no reason to ever be at anything but full
    /// deflection and the stick may as well have been a rudder.
    #[test]
    fn a_harder_bank_is_a_tighter_turn() {
        let turn_from = |roll: f32| {
            let mut s =
                MoveState { pos: vec2(240.0, 240.0), yaw: 0.0, speed: PLANE_CRUISE, roll };
            for _ in 0..30 {
                let _ = step_vehicle(&mut s, 0.0, 0.0, VehicleKind::Plane, 0, &[], world::TICK_DT);
            }
            s.yaw
        };

        let shallow = turn_from(PLANE_MAX_BANK * 0.25);
        let half = turn_from(PLANE_MAX_BANK * 0.5);
        let full = turn_from(PLANE_MAX_BANK);
        assert!(shallow > 0.0 && half > shallow && full > half, "{shallow} {half} {full}");
        // A full turn is the rate the tuning advertises.
        let expected = tuning(VehicleKind::Plane).turn_rate;
        assert!((full - expected).abs() < 0.05, "full bank came round at {full}, not {expected}");
        // Tangent, not a straight line: half the bank is well under half the
        // rate, which is what makes a shallow bank a usable wide arc.
        assert!(half < full * 0.45, "half bank did {half} against a full {full}");
    }

    /// Nothing holds the aircraft inside the field, and leaving is leaving.
    ///
    /// The boundary used to bank it back in, which kept every sortie over the
    /// field at the cost of the boundary flying the aeroplane -- and it fought
    /// the player hardest exactly where a corner base makes it worth being.
    /// Flying out is now a thing you can do and a way to lose the aircraft, so
    /// the wall has to let it through and `plane_has_left` has to say when it
    /// has gone far enough to count.
    #[test]
    fn an_aircraft_can_be_flown_out_of_the_match() {
        let mut s =
            MoveState { pos: vec2(WORLD_SIZE - 20.0, 100.0), yaw: 0.0, speed: PLANE_CRUISE, roll: 0.0 };
        assert!(!plane_has_left(s.pos), "it started outside the field");

        // Held straight at the wall, wings level, with no stick input at all.
        let mut crossed_the_wall = None;
        let mut gone = None;
        for tick in 0..120 {
            let _ = step_vehicle(&mut s, 0.0, 0.0, VehicleKind::Plane, 0, &[], world::TICK_DT);
            if crossed_the_wall.is_none() && s.pos.x > WORLD_SIZE {
                crossed_the_wall = Some(tick);
            }
            if gone.is_none() && plane_has_left(s.pos) {
                gone = Some(tick);
            }
        }
        let crossed = crossed_the_wall.expect("it never reached the wall");
        let gone = gone.expect("it never counted as having left");
        assert!(
            gone > crossed,
            "the aircraft has to be seen to leave, not blink out on the boundary"
        );
        assert_eq!(s.roll, 0.0, "the boundary is not allowed to fly it any more");
    }

    /// Being near the edge is not being over it.
    #[test]
    fn a_sortie_over_its_own_corner_has_not_left() {
        // A base pad sits `BASE_INSET` in from two walls, and bombing one is
        // among the things worth crossing the map for, so flying over one must
        // not end the sortie.
        for player in 0..world::MAX_PLAYERS as u8 {
            let base = world::base_position(player);
            assert!(!plane_has_left(base), "a base pad counts as off the field");
        }
        assert!(!plane_has_left(vec2(0.0, 0.0)), "the corner itself counts as off the field");
        assert!(!plane_has_left(vec2(WORLD_SIZE, WORLD_SIZE)));
    }

    /// The bombsight has to be exactly where the bomb lands.
    ///
    /// Both the sight the client draws and the server's release call
    /// `bomb_impact`, so the only way they can disagree is if the bomb does not
    /// actually travel the way this says. Flying the plane and then flying the
    /// bomb is what checks that, rather than restating the formula.
    #[test]
    fn a_bomb_lands_where_the_sight_says_it_will() {
        let mut plane = MoveState { pos: vec2(120.0, 200.0), yaw: 0.7, speed: PLANE_CRUISE, roll: 0.0 };
        for _ in 0..30 {
            let _ = step_vehicle(&mut plane, 0.0, 0.0, VehicleKind::Plane, 0, &[], world::TICK_DT);
        }
        let aimed = bomb_impact(plane.pos, plane.yaw, plane.speed);

        // The bomb keeps the velocity it was released with and falls.
        let mut bomb = plane.pos;
        let step = Vec2::from_angle(plane.yaw) * (plane.speed * world::TICK_DT);
        let ticks = (BOMB_FALL_TIME / world::TICK_DT).round() as u32;
        for _ in 0..ticks {
            bomb += step;
        }
        assert!(
            bomb.distance(aimed) < 0.5,
            "the sight promised {aimed:?} and the bomb reached {bomb:?}"
        );
        assert!(
            aimed.distance(plane.pos) > 40.0,
            "the throw ahead should be worth drawing a sight for, was {}",
            aimed.distance(plane.pos)
        );
    }

    /// What a direct hit is worth, which is what `BOMB_DAMAGE` is set from.
    ///
    /// A bomb is thrown 47 units ahead of an aircraft that cannot stop, out of
    /// a sortie that comes round every forty-five seconds, so most of them
    /// miss. The ones that do not have to be worth the wait: a miner
    /// caught square loses its shield entirely and half of the hull under it.
    ///
    /// Pinned here because it is a balance decision that a later change to any
    /// of three separate numbers -- the damage, the falloff, or a miner's
    /// own tuning -- would quietly undo.
    #[test]
    fn a_direct_hit_strips_a_miner_and_halves_what_is_left() {
        let kind = VehicleKind::Miner;
        let mut shield = max_shield(kind, 0);
        let mut hull = max_hull(kind, 0);
        let full = max_hull(kind, 0);

        let _ = apply_damage(
            &mut shield,
            &mut hull,
            blast_damage(0.0, BOMB_BLAST_RADIUS, BOMB_DAMAGE),
        );
        assert_eq!(shield, 0.0, "the shield should be gone outright");
        assert!(
            (hull / full - 0.5).abs() < 0.02,
            "it left {hull} of {full}, which is {:.0}% rather than half",
            hull / full * 100.0
        );

        // A tank has less between it and the ground than a miner does, so
        // the same hit is the end of it. That is deliberate: this is the one
        // weapon in the game that has to be worth a thousand ore to land.
        let mut shield = max_shield(VehicleKind::Tank, 0);
        let mut hull = max_hull(VehicleKind::Tank, 0);
        let left = apply_damage(
            &mut shield,
            &mut hull,
            blast_damage(0.0, BOMB_BLAST_RADIUS, BOMB_DAMAGE),
        );
        assert_eq!(left, 0.0, "a tank walked away from a bomb landing on it");

        // Upgrades still buy something, which is what keeps the flat number
        // honest rather than making Shield Booster pointless against the air.
        let up = crate::world::PowerUp::ShieldBooster.bit()
            | crate::world::PowerUp::MinerArmor.bit();
        let mut shield = max_shield(kind, up);
        let mut hull = max_hull(kind, up);
        let left = apply_damage(
            &mut shield,
            &mut hull,
            blast_damage(0.0, BOMB_BLAST_RADIUS, BOMB_DAMAGE),
        );
        assert!(left > max_hull(kind, up) * 0.9, "the upgrades bought nothing: {left} left");
    }

    /// A blast falls off, so a bomb is aimed at a place rather than a hull.
    #[test]
    fn a_blast_fades_from_the_centre_to_nothing_at_the_rim() {
        let r = BOMB_BLAST_RADIUS;
        assert_eq!(blast_damage(0.0, r, BOMB_DAMAGE), BOMB_DAMAGE);
        assert_eq!(blast_damage(r, r, BOMB_DAMAGE), 0.0);
        assert_eq!(blast_damage(r * 2.0, r, BOMB_DAMAGE), 0.0);
        let half = blast_damage(r * 0.5, r, BOMB_DAMAGE);
        assert!((half - BOMB_DAMAGE * 0.5).abs() < 1e-3, "halfway out did {half}");
        // Monotonic, so there is never a ring that hurts more than the middle.
        let mut last = f32::MAX;
        for i in 0..=20 {
            let d = blast_damage(r * i as f32 / 20.0, r, BOMB_DAMAGE);
            assert!(d <= last, "damage rose again at {i}");
            last = d;
        }
    }

    /// Terrain impact has to reflect how hard the hill was actually met, or
    /// the server cannot tell a crash from parking against a slope.
    #[test]
    fn a_hill_reports_how_hard_it_was_hit() {
        let hill = Hill { pos: vec2(140.0, 100.0), radius: 9.0 };

        // Open ground reports nothing at all.
        let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        let mut worst: f32 = 0.0;
        for _ in 0..60 {
            let out = step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[], world::TICK_DT);
            worst = worst.max(out.terrain_impact);
        }
        assert_eq!(worst, 0.0, "nothing was hit");

        // Straight into the face at speed: the impact is most of the top speed.
        let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        let mut worst: f32 = 0.0;
        for _ in 0..90 {
            let out = step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
            worst = worst.max(out.terrain_impact);
        }
        assert!(
            worst > IMPACT_THRESHOLD,
            "a full-speed crash reported {worst}, under the {IMPACT_THRESHOLD} that hurts"
        );

        // Clipping a shoulder is not a crash.
        let mut s = MoveState { pos: vec2(100.0, 110.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        let mut worst: f32 = 0.0;
        for _ in 0..150 {
            let out = step_vehicle(&mut s, 1.0, 0.0, VehicleKind::Tank, 0, &[hill], world::TICK_DT);
            worst = worst.max(out.terrain_impact);
        }
        assert!(worst < IMPACT_THRESHOLD, "a graze reported {worst} and would have cost hull");
    }

    /// Backing into a hillside is the same crash as driving into one.
    ///
    /// Turbo puts a tank's reverse above [`IMPACT_THRESHOLD`], so reading the
    /// impact off the hull's facing rather than off where it was actually going
    /// left backwards as a free direction.
    #[test]
    fn reversing_into_a_hill_is_still_a_crash() {
        let hill = Hill { pos: vec2(60.0, 100.0), radius: 9.0 };
        let turbo = crate::world::PowerUp::Turbo.bit();

        // Facing away from the hill and reversing straight into it.
        let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        let mut worst: f32 = 0.0;
        for _ in 0..90 {
            let out =
                step_vehicle(&mut s, -1.0, 0.0, VehicleKind::Tank, turbo, &[hill], world::TICK_DT);
            worst = worst.max(out.terrain_impact);
        }
        assert!(
            worst > IMPACT_THRESHOLD,
            "backing into a hill under turbo reported {worst}, so reverse costs nothing"
        );
    }

    #[test]
    fn the_autopilot_steers_straight_at_an_unobstructed_target() {
        let from = vec2(100.0, 100.0);
        let to = vec2(160.0, 100.0);
        assert!(autopilot_bearing(from, to, &[]).abs() < 1e-4, "should be due +x");

        // A hill behind, or off to the side, is not in the way.
        let behind = Hill { pos: vec2(60.0, 100.0), radius: 10.0 };
        let aside = Hill { pos: vec2(130.0, 140.0), radius: 10.0 };
        let bearing = autopilot_bearing(from, to, &[behind, aside]);
        assert!(bearing.abs() < 1e-4, "nothing crosses the path, got {bearing}");
    }

    /// The case that would otherwise wedge a miner forever: a hill sitting
    /// squarely between it and where it is going.
    #[test]
    fn the_autopilot_rounds_a_hill_on_the_way_to_its_target() {
        let hill = Hill { pos: vec2(130.0, 100.0), radius: 10.0 };
        let mut s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        let target = vec2(180.0, 100.0);

        // Straight through the middle, so the direct bearing is useless.
        let bearing = autopilot_bearing(s.pos, target, &[hill]);
        assert!(bearing.abs() > 0.2, "should aim off the hill, got {bearing}");

        // And driving on it actually arrives.
        for _ in 0..900 {
            let (throttle, steer) = autopilot(&s, target, 6.0, &[hill]);
            let _ = step_vehicle(
                &mut s,
                throttle,
                steer,
                VehicleKind::Miner,
                0,
                &[hill],
                world::TICK_DT,
            );
            if s.pos.distance(target) <= 6.0 {
                break;
            }
        }
        assert!(
            s.pos.distance(target) <= 6.0,
            "ended {:.1} away, so it never got round the hill",
            s.pos.distance(target)
        );
    }

    #[test]
    fn the_autopilot_stops_when_it_arrives() {
        let s = MoveState { pos: vec2(100.0, 100.0), yaw: 0.0, speed: 0.0, roll: 0.0 };
        let (throttle, steer) = autopilot(&s, vec2(103.0, 100.0), 6.0, &[]);
        assert_eq!((throttle, steer), (0.0, 0.0), "inside the stop radius it coasts");
    }

    /// The cruise has to stay under the speed that hurts, or a miner left
    /// to itself would grind its own hull away on the scenery.
    #[test]
    fn the_autopilot_cruises_too_slowly_to_hurt_itself() {
        let top = tuning(VehicleKind::Miner).max_speed;
        assert!(top * AUTOPILOT_CRUISE < IMPACT_THRESHOLD);
    }

    #[test]
    fn overlapping_hulls_are_pushed_apart_along_their_axis() {
        let (ra, rb) = (2.4f32, 2.9f32);
        // Clear of each other: nothing to do.
        assert!(overlap_push(vec2(0.0, 0.0), ra, vec2(20.0, 0.0), rb).is_none());
        assert!(overlap_push(vec2(0.0, 0.0), ra, vec2(ra + rb, 0.0), rb).is_none());

        let (axis, overlap) = overlap_push(vec2(0.0, 0.0), ra, vec2(4.0, 0.0), rb).unwrap();
        assert!((axis - vec2(1.0, 0.0)).length() < 1e-5, "axis points at the other hull");
        assert!((overlap - (ra + rb - 4.0)).abs() < 1e-5, "overlap is the shortfall");

        // Exactly co-located still separates rather than dividing by zero.
        let (axis, overlap) = overlap_push(vec2(5.0, 5.0), ra, vec2(5.0, 5.0), rb).unwrap();
        assert!(axis.length() > 0.99, "a usable axis even with no direction to take");
        assert!((overlap - (ra + rb)).abs() < 1e-5);
    }

    #[test]
    fn stepping_is_deterministic() {
        let run = || {
            let mut s = MoveState { pos: vec2(40.0, 60.0), yaw: 0.3, speed: 2.0, roll: 0.0 };
            for i in 0..500 {
                let throttle = if i % 7 == 0 { -1.0 } else { 1.0 };
                let steer = ((i % 13) as f32 - 6.0) / 6.0;
                let _ = step_vehicle(
                    &mut s,
                    throttle,
                    steer,
                    VehicleKind::Miner,
                    0,
                    &[],
                    world::TICK_DT,
                );
            }
            s
        };
        assert_eq!(run(), run());
    }

    /// A gun that fires at what its shell cannot reach is just noise, and it
    /// would leave a ring around every base at which nobody could hit anybody.
    ///
    /// The tank's gun is tuned for play and keeps getting shorter; these two
    /// are not, and must not be shortened along with it by accident.
    #[test]
    fn the_fixed_guns_outreach_what_they_shoot_at() {
        for range in [SENTINEL_RANGE, AUTO_TURRET_RANGE] {
            let reach = BULLET_SPEED * shell_life_covering(range);
            assert!(reach > range, "a gun firing at {range} throws a shell {reach}");
        }
    }

    /// The gun is a short-range weapon now, and the upgrade is what makes it
    /// a medium-range one.
    #[test]
    fn a_long_barrel_doubles_how_far_a_shell_reaches() {
        let plain = BULLET_SPEED * bullet_lifetime(0);
        let upgraded = BULLET_SPEED * bullet_lifetime(PowerUp::LongBarrel.bit());

        assert!(
            (upgraded - plain * 2.0).abs() < 0.01,
            "{upgraded} should be twice {plain}"
        );
        // A tenth of the field plain, a fifth upgraded: a gunfight is something
        // you drive into, not something you open from your own quadrant.
        assert!(plain < world::WORLD_SIZE * 0.15, "a plain shell reaches {plain}");
        assert!(upgraded < world::WORLD_SIZE * 0.25, "even upgraded it reaches {upgraded}");
        // Nobody else's gun is bought a longer reach.
        assert_eq!(bullet_lifetime(PowerUp::Turbo.bit()), BULLET_LIFETIME);
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
    fn armor_raises_only_the_miner_hull() {
        let mask = PowerUp::MinerArmor.bit();
        assert!(max_hull(VehicleKind::Miner, mask) > max_hull(VehicleKind::Miner, 0));
        assert_eq!(max_hull(VehicleKind::Tank, mask), max_hull(VehicleKind::Tank, 0));
        assert!(cargo_capacity(mask) > cargo_capacity(0));
    }
}
