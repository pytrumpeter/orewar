//! World layout, economy constants, and deterministic map generation.

use crate::math::{Vec2, vec2};
use crate::rng::Rng;

/// Field is a `GRID_CELLS` square of `CELL_SIZE` tiles.
///
/// The cell is what grows when the field does; the grid stays 64 squares so the
/// ground texture and the drawn lines keep the same relationship to it.
pub const GRID_CELLS: u32 = 64;
pub const CELL_SIZE: f32 = 7.5;
pub const WORLD_SIZE: f32 = GRID_CELLS as f32 * CELL_SIZE;

/// One corner each, so the field supports four players.
pub const MAX_PLAYERS: usize = 4;

pub const TICK_HZ: u32 = 30;
pub const TICK_DT: f32 = 1.0 / TICK_HZ as f32;

pub const DEFAULT_PORT: u16 = 45_701;

/// How far a base pad sits in from the two edges of its corner.
pub const BASE_INSET: f32 = 24.0;
/// A harvester within this distance of its own pad unloads its cargo.
pub const BASE_RADIUS: f32 = 13.0;

/// Per-player identity colors, chosen to stay distinct against green grass.
pub const PLAYER_COLORS: [[u8; 3]; MAX_PLAYERS] =
    [[222, 62, 62], [58, 140, 232], [238, 178, 48], [172, 92, 224]];

pub const PLAYER_COLOR_NAMES: [&str; MAX_PLAYERS] = ["Crimson", "Azure", "Amber", "Violet"];

/// Rotates a point a quarter turn about the field center, `times` times.
///
/// The whole map is built from this one operation, which is what guarantees all
/// four corners are strategically identical.
pub fn rotate_quarter(p: Vec2, times: u32) -> Vec2 {
    let c = Vec2::splat(WORLD_SIZE * 0.5);
    let mut d = p - c;
    for _ in 0..(times % 4) {
        d = vec2(-d.y, d.x);
    }
    c + d
}

/// Quarter-turns from the south-west corner to each player's corner.
///
/// Players 0 and 1 are placed diagonally rather than adjacently, so a two-player
/// game starts at maximum separation.
const CORNER_ORDER: [u32; MAX_PLAYERS] = [0, 2, 1, 3];

/// Center of `player`'s home pad, where their harvester unloads ore.
pub fn base_position(player: u8) -> Vec2 {
    let idx = player as usize % MAX_PLAYERS;
    rotate_quarter(vec2(BASE_INSET, BASE_INSET), CORNER_ORDER[idx])
}

/// Ore in the ground. `amount` depletes as it is harvested; `capacity` is what
/// it started with, which the client uses to size the deposit's visual.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OreDeposit {
    pub pos: Vec2,
    pub amount: f32,
    pub capacity: f32,
}

/// Kept sparse on purpose: a handful of deposits per quadrant makes each one
/// worth driving to and worth denying an opponent.
// Counts scale with area, not with width: a quadrant went from 160 to 240 a
// side, which is 2.25 times the ground. Holding the count would have left the
// same picture stretched, with long empty drives between patches.
const DEPOSITS_PER_QUADRANT: usize = 13;
/// Rich, contested deposits in the middle of the field.
const CENTER_DEPOSITS: usize = 4;

pub const CENTER_DEPOSIT_CAPACITY: f32 = 1600.0;
const MIN_DEPOSIT_CAPACITY: f32 = 420.0;
const MAX_DEPOSIT_CAPACITY: f32 = 900.0;

/// Builds the ore field for a seed.
///
/// The map is generated in one quadrant and then rotated into the other three,
/// so every player faces an identical distribution of ore relative to their own
/// base. Both the server and every client run this from the seed in `Welcome`,
/// which is why the seed travels instead of the deposit list.
pub fn generate_ore(seed: u64) -> Vec<OreDeposit> {
    // Mixed so that map layout is uncorrelated with anything else derived from
    // the same world seed.
    const ORE_SEED_MIX: u64 = 0x0DE5_1A11_0F1C_E501;
    let mut rng = Rng::new(seed ^ ORE_SEED_MIX);
    let center = Vec2::splat(WORLD_SIZE * 0.5);
    let home = base_position(0);

    let mut quadrant: Vec<(Vec2, f32)> = Vec::with_capacity(DEPOSITS_PER_QUADRANT);
    let mut attempts = 0;
    while quadrant.len() < DEPOSITS_PER_QUADRANT && attempts < 8000 {
        attempts += 1;
        let half = WORLD_SIZE * 0.5;
        // Absolute margins, not scaled ones: a wider field should hold the
        // same deposits further apart, not the same picture stretched.
        let p = vec2(rng.range_f32(16.0, half - 8.0), rng.range_f32(16.0, half - 8.0));
        // Keep the spawn pad clear so nobody starts parked on top of ore.
        if p.distance(home) < BASE_RADIUS + 14.0 {
            continue;
        }
        // Leave room for the central cluster.
        if p.distance(center) < 34.0 {
            continue;
        }
        if quadrant.iter().any(|(q, _)| q.distance(p) < 22.0) {
            continue;
        }
        let capacity = rng.range_f32(MIN_DEPOSIT_CAPACITY, MAX_DEPOSIT_CAPACITY);
        quadrant.push((p, capacity));
    }

    let mut out = Vec::with_capacity(quadrant.len() * 4 + CENTER_DEPOSITS);
    for turn in 0..4u32 {
        for &(p, capacity) in &quadrant {
            out.push(OreDeposit { pos: rotate_quarter(p, turn), amount: capacity, capacity });
        }
    }

    // The center cluster, also rotationally symmetric.
    let first = center + vec2(19.0, 0.0);
    for turn in 0..CENTER_DEPOSITS as u32 {
        out.push(OreDeposit {
            pos: rotate_quarter(first, turn),
            amount: CENTER_DEPOSIT_CAPACITY,
            capacity: CENTER_DEPOSIT_CAPACITY,
        });
    }

    out
}

// ---------------------------------------------------------------------------
// Terrain
// ---------------------------------------------------------------------------

/// A hill: ground a vehicle has to drive around rather than over.
///
/// The simulation knows a hill only as a circle. The mesh the client drapes
/// over it is decoration -- what a hull collides with, and what a shell stops
/// against, is this radius, at every height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hill {
    pub pos: Vec2,
    pub radius: f32,
}

const HILLS_PER_QUADRANT: usize = 9;
const MIN_HILL_RADIUS: f32 = 6.0;
const MAX_HILL_RADIUS: f32 = 11.0;

/// Builds the terrain for a seed, on the same rotational plan as the ore.
///
/// Hills are placed after the ore and around it: a hill close enough to a
/// deposit to deny a harvester its parking space would quietly delete that
/// deposit from the game, which is worse than a slightly emptier map.
pub fn generate_hills(seed: u64) -> Vec<Hill> {
    const HILL_SEED_MIX: u64 = 0x_1A11_5EED_0C0F_FEE5;
    let mut rng = Rng::new(seed ^ HILL_SEED_MIX);
    let center = Vec2::splat(WORLD_SIZE * 0.5);
    let home = base_position(0);
    let ore = generate_ore(seed);
    let half = WORLD_SIZE * 0.5;

    // The sampling box stops well short of the two centre lines. Rotating a
    // point out of this box always lands it at least twice the widest hill away
    // from the box, which is what keeps a hill from overlapping its own copy in
    // the next quadrant without a second, cross-quadrant spacing pass.
    let (lo, hi) = (20.0, half - 16.0);

    let mut quadrant: Vec<(Vec2, f32)> = Vec::with_capacity(HILLS_PER_QUADRANT);
    let mut attempts = 0;
    while quadrant.len() < HILLS_PER_QUADRANT && attempts < 8000 {
        attempts += 1;
        let radius = rng.range_f32(MIN_HILL_RADIUS, MAX_HILL_RADIUS);
        let p = vec2(rng.range_f32(lo, hi), rng.range_f32(lo, hi));
        // Nobody starts boxed in by terrain.
        if p.distance(home) < BASE_RADIUS + 20.0 + radius {
            continue;
        }
        // The contested middle stays open ground; cover there would change what
        // the centre cluster is worth.
        if p.distance(center) < 40.0 + radius {
            continue;
        }
        // Room to park a harvester on every deposit, all the way around it.
        if ore.iter().any(|o| o.pos.distance(p) < radius + crate::sim::HARVEST_RADIUS + 6.0) {
            continue;
        }
        if quadrant.iter().any(|(q, r)| q.distance(p) < radius + r + 12.0) {
            continue;
        }
        quadrant.push((p, radius));
    }

    let mut out = Vec::with_capacity(quadrant.len() * 4);
    for turn in 0..4u32 {
        for &(p, radius) in &quadrant {
            out.push(Hill { pos: rotate_quarter(p, turn), radius });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Power-ups
// ---------------------------------------------------------------------------

/// Things a player buys with refined ore.
///
/// Most are permanent flags recorded in a bitmask; [`PowerUp::MissilePack`] is
/// the exception and is bought repeatedly for ammunition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerUp {
    Radar = 0,
    ShieldBooster = 1,
    Turbo = 2,
    HarvesterArmor = 3,
    AutoTurret = 4,
    MissilePack = 5,
}

impl PowerUp {
    pub const ALL: [PowerUp; 6] = [
        PowerUp::Radar,
        PowerUp::ShieldBooster,
        PowerUp::Turbo,
        PowerUp::HarvesterArmor,
        PowerUp::AutoTurret,
        PowerUp::MissilePack,
    ];

    pub fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.get(v as usize).copied()
    }

    pub fn cost(self) -> u32 {
        match self {
            PowerUp::Radar => 600,
            PowerUp::ShieldBooster => 400,
            PowerUp::Turbo => 350,
            PowerUp::HarvesterArmor => 450,
            PowerUp::AutoTurret => 800,
            PowerUp::MissilePack => 250,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PowerUp::Radar => "Radar",
            PowerUp::ShieldBooster => "Shield Booster",
            PowerUp::Turbo => "Turbo Drive",
            PowerUp::HarvesterArmor => "Harvester Armor",
            PowerUp::AutoTurret => "Auto Turret",
            PowerUp::MissilePack => "Missile Pack",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            PowerUp::Radar => "HUD contact circle showing every vehicle on the field",
            PowerUp::ShieldBooster => "+60 max shield and faster regeneration, both vehicles",
            PowerUp::Turbo => "+30% top speed, both vehicles",
            PowerUp::HarvesterArmor => "+60 harvester hull and +25% cargo capacity",
            PowerUp::AutoTurret => "Harvester defends itself against nearby enemies",
            PowerUp::MissilePack => "+6 missiles for your tank",
        }
    }

    /// Consumables can be bought repeatedly; flags only once.
    pub fn is_consumable(self) -> bool {
        matches!(self, PowerUp::MissilePack)
    }

    /// Bit in a player's power-up mask. Consumables occupy no bit.
    pub fn bit(self) -> u16 {
        if self.is_consumable() { 0 } else { 1 << (self as u16) }
    }

    pub fn held(self, mask: u16) -> bool {
        !self.is_consumable() && mask & self.bit() != 0
    }
}

/// Missiles granted per [`PowerUp::MissilePack`].
pub const MISSILES_PER_PACK: u8 = 6;
/// Missiles a tank starts with.
pub const STARTING_MISSILES: u8 = 3;
/// Ore credits every player starts with, enough for one early purchase.
pub const STARTING_CREDITS: u32 = 150;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic() {
        let a = generate_ore(12345);
        let b = generate_ore(12345);
        assert_eq!(a, b);
        assert_ne!(generate_ore(1), generate_ore(2));

        // The client and the server each generate the terrain from the seed
        // rather than sending it, so anything but byte-identical output here
        // means they disagree about where a shell stops.
        assert_eq!(generate_hills(12345), generate_hills(12345));
        assert_ne!(generate_hills(1), generate_hills(2));
    }

    #[test]
    fn every_corner_gets_an_identical_field() {
        let ore = generate_ore(0xABCDEF);
        assert_eq!(ore.len(), DEPOSITS_PER_QUADRANT * 4 + CENTER_DEPOSITS);

        // Each player's ore, expressed relative to their own base, must match.
        let profile = |player: u8| {
            let base = base_position(player);
            let mut d: Vec<i64> =
                ore.iter().map(|o| (o.pos.distance(base) * 100.0) as i64).collect();
            d.sort_unstable();
            d
        };
        let reference = profile(0);
        for player in 1..MAX_PLAYERS as u8 {
            assert_eq!(profile(player), reference, "player {player} has a different map");
        }
    }

    #[test]
    fn deposits_stay_inside_the_field_and_off_the_pads() {
        let ore = generate_ore(7);
        for o in &ore {
            assert!(o.pos.x > 0.0 && o.pos.x < WORLD_SIZE, "{:?}", o.pos);
            assert!(o.pos.y > 0.0 && o.pos.y < WORLD_SIZE, "{:?}", o.pos);
            for p in 0..MAX_PLAYERS as u8 {
                assert!(o.pos.distance(base_position(p)) > BASE_RADIUS, "deposit on a base pad");
            }
        }
    }

    #[test]
    fn hills_are_generated_identically_for_every_corner() {
        let hills = generate_hills(0xABCDEF);
        assert_eq!(hills.len(), HILLS_PER_QUADRANT * 4, "the quadrant failed to fill");

        let profile = |player: u8| {
            let base = base_position(player);
            let mut d: Vec<i64> =
                hills.iter().map(|h| (h.pos.distance(base) * 100.0) as i64).collect();
            d.sort_unstable();
            d
        };
        let reference = profile(0);
        for player in 1..MAX_PLAYERS as u8 {
            assert_eq!(profile(player), reference, "player {player} faces different terrain");
        }
    }

    /// Every constraint that stops the terrain from breaking the map, checked
    /// across enough seeds that a rare placement cannot slip through.
    #[test]
    fn hills_never_block_a_deposit_a_base_or_each_other() {
        for seed in 0..300u64 {
            let seed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let hills = generate_hills(seed);
            assert_eq!(hills.len(), HILLS_PER_QUADRANT * 4, "seed {seed} under-filled");

            for (i, h) in hills.iter().enumerate() {
                assert!(
                    h.pos.x - h.radius > 0.0 && h.pos.x + h.radius < WORLD_SIZE,
                    "seed {seed}: hill leaves the field"
                );
                assert!(h.pos.y - h.radius > 0.0 && h.pos.y + h.radius < WORLD_SIZE);

                for p in 0..MAX_PLAYERS as u8 {
                    assert!(
                        h.pos.distance(base_position(p)) > BASE_RADIUS + h.radius,
                        "seed {seed}: hill on a base pad"
                    );
                }
                // A harvester must be able to park anywhere around a deposit.
                for o in generate_ore(seed) {
                    assert!(
                        o.pos.distance(h.pos) > h.radius + crate::sim::HARVEST_RADIUS,
                        "seed {seed}: hill covers a deposit's parking space"
                    );
                }
                // Overlapping hills would need more than one push-out pass in
                // `step_vehicle`, which is a bug waiting to trap a vehicle.
                for other in &hills[i + 1..] {
                    assert!(
                        h.pos.distance(other.pos) > h.radius + other.radius,
                        "seed {seed}: two hills overlap"
                    );
                }
            }
        }
    }

    #[test]
    fn bases_sit_in_four_distinct_corners() {
        let mut seen = Vec::new();
        for p in 0..MAX_PLAYERS as u8 {
            let b = base_position(p);
            assert!(seen.iter().all(|s: &Vec2| s.distance(b) > 100.0), "corners too close");
            seen.push(b);
        }
        // Players 0 and 1 are the diagonal pair.
        assert!(base_position(0).distance(base_position(1)) > base_position(0).distance(base_position(2)));
    }

    #[test]
    fn powerup_bits_are_unique_and_consumables_have_none() {
        let mut mask = 0u16;
        for p in PowerUp::ALL {
            if p.is_consumable() {
                assert_eq!(p.bit(), 0);
                continue;
            }
            assert_eq!(mask & p.bit(), 0, "{p:?} reuses a bit");
            mask |= p.bit();
            assert!(p.held(mask));
        }
        assert_eq!(PowerUp::from_u8(PowerUp::AutoTurret as u8), Some(PowerUp::AutoTurret));
        assert_eq!(PowerUp::from_u8(200), None);
    }
}
