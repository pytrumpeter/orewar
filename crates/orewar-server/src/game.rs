//! The authoritative simulation.
//!
//! This is the only place game state actually changes. Clients send intent and
//! render what comes back; everything that decides an outcome -- who was hit,
//! who owns which miner, whether a purchase can be afforded -- happens here.
//!
//! Player state is keyed by a client-supplied token rather than by socket
//! address, so a player who drops out keeps their ore, power-ups, and vehicles
//! and resumes the same slot when they reconnect.

use std::collections::HashSet;

use orewar_shared::math::{Vec2, angle_delta, wrap_angle};
use orewar_shared::protocol::{
    DenyReason, GameEvent, GameStatus, MinerMode, HitFx, HitKind, InputFrame,
    MAX_HITS_PER_SNAPSHOT, OreUpdate, PlaneSnapshot, PlayerInfo, PlayerSnapshot, ProjectileKind,
    SentinelSnapshot, ProjectileSnapshot, RejectReason, Snapshot, VehicleSlot, VehicleSnapshot,
    MAX_PROJECTILES_PER_SNAPSHOT,
};
use orewar_shared::sim::{self, MoveState, VehicleKind};
use orewar_shared::world::{
    self, Hill, MAX_PLAYERS, MISSILES_PER_PACK, OreDeposit, PowerUp, STARTING_CREDITS,
    STARTING_MISSILES, STARTING_POWERUPS,
};

/// How long a player's last input frame stays in effect.
///
/// Input arrives unreliably and a client that lags out simply stops sending.
/// Without an expiry the server would keep applying whatever was last held --
/// a dropped player would drive on at full throttle, firing, indefinitely.
pub const INPUT_TIMEOUT: f32 = 0.5;

/// One of a player's two vehicles.
#[derive(Clone, Debug)]
pub struct Vehicle {
    pub mv: MoveState,
    pub turret_yaw: f32,
    pub shield: f32,
    pub hull: f32,
    pub cargo: f32,
    /// A miner at zero hull. It cannot move or mine, and an enemy tank
    /// can take it.
    pub disabled: bool,
    /// Capture progress while disabled, `0..=1`.
    pub capture_progress: f32,
    /// Seconds since last taking damage, gating shield regeneration.
    pub since_damage: f32,
    pub gun_cooldown: f32,
    pub missile_cooldown: f32,
    /// Seconds of flying left. Only the bomber burns it; on the ground it stays
    /// at zero and is never read.
    pub fuel: f32,
}

impl Vehicle {
    fn spawn(kind: VehicleKind, pos: Vec2, yaw: f32, powerups: u16) -> Self {
        Vehicle {
            mv: MoveState { pos, yaw, speed: 0.0, roll: 0.0, alt: 0.0 },
            turret_yaw: yaw,
            shield: sim::max_shield(kind, powerups),
            hull: sim::max_hull(kind, powerups),
            cargo: 0.0,
            disabled: false,
            capture_progress: 0.0,
            since_damage: sim::SHIELD_REGEN_DELAY,
            gun_cooldown: 0.0,
            missile_cooldown: 0.0,
            fuel: if kind.flies() { sim::PLANE_FUEL } else { 0.0 },
        }
    }

    fn to_snapshot(&self) -> VehicleSnapshot {
        VehicleSnapshot {
            pos: self.mv.pos,
            yaw: self.mv.yaw,
            turret_yaw: self.turret_yaw,
            speed: self.mv.speed,
            shield: self.shield,
            hull: self.hull,
            cargo: self.cargo,
            disabled: self.disabled,
            capture_progress: self.capture_progress,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Player {
    pub id: u8,
    pub name: String,
    /// Stable identity across reconnects.
    pub token: u64,
    pub connected: bool,
    pub eliminated: bool,
    pub credits: u32,
    pub ore_mined: u32,
    pub powerups: u16,
    pub missiles: u8,
    pub captures: u8,
    pub tank: Option<Vehicle>,
    pub miner: Option<Vehicle>,
    /// A sortie in the air. A `Vehicle` like the other two so that everything
    /// which takes a slot -- input, prediction, the switch key -- works on it
    /// without a second shape to special-case, even though most of a vehicle
    /// means nothing to it.
    pub plane: Option<Vehicle>,
    /// Seconds until another sortie can be called. Counts from the last one
    /// ending, so a short run does not buy a quick second one.
    pub sortie_cooldown: f32,
    pub sentinel: Sentinel,
    /// What the miner does when the player is driving something else.
    pub miner_mode: MinerMode,
    /// Seconds the autopilot has spent asking for throttle and going nowhere.
    /// Steering rounds a hill, but a miner can still end up wedged; this is
    /// what notices.
    stuck_for: f32,
    /// Seconds left of backing out of being wedged.
    unstick_for: f32,
    /// Counts down while the tank is destroyed.
    pub respawn_timer: f32,
    /// Counts down while the player is off the field entirely, having had their
    /// miner taken. Zero means they are in the match.
    pub down_for: f32,
    pub input: InputFrame,
    /// Seconds since a fresh input frame arrived.
    pub input_age: f32,
    /// Highest input tick accepted, echoed back so the client can reconcile.
    pub acked_input: u32,
}

/// Where a player's vehicles stand at the start of a match, and which way they
/// face. Shared by the opening spawn and by coming back from a capture, so the
/// two cannot drift apart.
fn starting_placement(id: u8) -> (Vec2, Vec2, f32) {
    let base = world::base_position(id);
    // Face the middle of the field, which is where the action is.
    let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - base).to_angle();
    (
        base + Vec2::from_angle(inward) * 7.0,
        base + Vec2::from_angle(inward).perp() * 7.0,
        inward,
    )
}

impl Player {
    fn new(id: u8, token: u64, name: String) -> Self {
        let (tank_pos, miner_pos, inward) = starting_placement(id);
        Player {
            id,
            name,
            token,
            connected: true,
            eliminated: false,
            credits: STARTING_CREDITS,
            ore_mined: 0,
            powerups: STARTING_POWERUPS,
            missiles: STARTING_MISSILES,
            captures: 0,
            tank: Some(Vehicle::spawn(
                VehicleKind::Tank,
                tank_pos,
                inward,
                STARTING_POWERUPS,
            )),
            miner: Some(Vehicle::spawn(
                VehicleKind::Miner,
                miner_pos,
                inward,
                STARTING_POWERUPS,
            )),
            plane: None,
            sortie_cooldown: 0.0,
            sentinel: Sentinel::new(id),
            miner_mode: MinerMode::default(),
            stuck_for: 0.0,
            unstick_for: 0.0,
            respawn_timer: 0.0,
            down_for: 0.0,
            input: InputFrame::default(),
            input_age: 0.0,
            acked_input: 0,
        }
    }

    /// The input to actually simulate with.
    ///
    /// Once frames stop arriving the controls go neutral, so the vehicle coasts
    /// to a stop and stops shooting. Aim is kept, since a turret left pointing
    /// where it was is less jarring than one that snaps.
    fn effective_input(&self) -> InputFrame {
        let mut frame = if self.input_age > INPUT_TIMEOUT {
            InputFrame {
                controlling: self.input.controlling,
                aim: self.input.aim,
                tick: self.input.tick,
                ..Default::default()
            }
        } else {
            self.input
        };

        // Driving a vehicle that is not there is driving nothing. A destroyed
        // tank used to strand the player this way, and an aircraft running out
        // of fuel underneath them would do the same: the slot simply stops
        // existing mid-flight, so the fallback has to be checked every tick
        // rather than announced.
        if self.vehicle(frame.controlling).is_none() {
            frame.controlling =
                frame.controlling.next_available(|slot| self.vehicle(slot).is_some());
        }
        frame
    }

    pub fn vehicle(&self, slot: VehicleSlot) -> Option<&Vehicle> {
        match slot {
            VehicleSlot::Tank => self.tank.as_ref(),
            VehicleSlot::Miner => self.miner.as_ref(),
            VehicleSlot::Plane => self.plane.as_ref(),
        }
    }

    fn vehicle_mut(&mut self, slot: VehicleSlot) -> Option<&mut Vehicle> {
        match slot {
            VehicleSlot::Tank => self.tank.as_mut(),
            VehicleSlot::Miner => self.miner.as_mut(),
            VehicleSlot::Plane => self.plane.as_mut(),
        }
    }

    fn to_snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            id: self.id,
            connected: self.connected,
            eliminated: self.eliminated,
            credits: self.credits,
            ore_mined: self.ore_mined,
            powerups: self.powerups,
            missiles: self.missiles,
            captures: self.captures,
            miner_mode: self.miner_mode,
            plane: self.plane.as_ref().map(|v| PlaneSnapshot {
                pos: v.mv.pos,
                yaw: v.mv.yaw,
                roll: v.mv.roll,
                speed: v.mv.speed,
                fuel: (v.fuel / sim::PLANE_FUEL).clamp(0.0, 1.0),
                alt: v.mv.alt,
            }),
            plane_ready_in: self.sortie_cooldown.ceil().max(0.0) as u8,
            // Rounded up, so a countdown on screen reaches zero at the moment
            // the vehicles actually come back rather than a beat before.
            respawn_in: self.down_for.max(0.0).ceil().min(255.0) as u8,
            tank: self.tank.as_ref().map(Vehicle::to_snapshot),
            miner: self.miner.as_ref().map(Vehicle::to_snapshot),
            // Absent while it is rubble, which is how the client knows to draw
            // the wreck instead of the gun.
            sentinel: self.sentinel.standing().then(|| SentinelSnapshot {
                hull: self.sentinel.hull,
                turret_yaw: self.sentinel.turret_yaw,
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Projectile {
    pub id: u16,
    pub kind: ProjectileKind,
    pub owner: u8,
    pub pos: Vec2,
    pub yaw: f32,
    pub speed: f32,
    pub life: f32,
}

/// The gun emplacement in a player's home corner.
///
/// Not a vehicle: it never moves, has no shield, and is not something the player
/// drives. It exists to make walking into somebody's base cost something even
/// when they are away fighting.
#[derive(Clone, Debug)]
pub struct Sentinel {
    pub hull: f32,
    pub turret_yaw: f32,
    gun_cooldown: f32,
    /// Seconds of rubble left. Zero means it is standing.
    rebuild_timer: f32,
    /// Which of the enemies in range is being engaged. Advanced only after a
    /// shot actually leaves, so it commits to one target long enough to finish
    /// slewing onto it instead of thrashing between two.
    cursor: usize,
}

impl Sentinel {
    fn new(player: u8) -> Self {
        // Facing the middle of the field, so it starts pointed where trouble
        // comes from rather than at its own corner.
        let pos = world::sentinel_position(player);
        let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - pos).to_angle();
        Sentinel {
            hull: sim::SENTINEL_HULL,
            turret_yaw: inward,
            gun_cooldown: 0.0,
            rebuild_timer: 0.0,
            cursor: 0,
        }
    }

    pub fn standing(&self) -> bool {
        self.rebuild_timer <= 0.0
    }
}

/// What a projectile ran into.
///
/// Vehicles and emplacements are both worth hitting but are damaged through
/// entirely different paths, so the swept-collision pass reports which it found
/// rather than trying to describe one in terms of the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hittable {
    Vehicle(VehicleSlot),
    Sentinel,
}

/// A candidate for a projectile to hit.
struct Target {
    player: u8,
    what: Hittable,
    pos: Vec2,
    radius: f32,
}

/// One vehicle as [`Game::step_collisions`] sees it: a circle with a heading.
///
/// Lifted out of the players so the whole field can be compared against itself
/// without holding a borrow on any one of them, then written back.
#[derive(Clone, Copy)]
struct Body {
    player: u8,
    slot: VehicleSlot,
    pos: Vec2,
    radius: f32,
    /// Velocity. Both how fast two hulls were closing and, once normalised,
    /// which way each was actually travelling -- which is not the way it
    /// faces when it is reversing.
    vel: Vec2,
    /// What is left of the speed after the meeting.
    speed_scale: f32,
    /// A wreck has no engine to be shoved with, so it holds its ground and the
    /// hull that ran into it gives all of it.
    fixed: bool,
}

pub struct Game {
    pub seed: u64,
    pub tick: u32,
    pub status: GameStatus,
    pub winner: Option<u8>,
    pub players: Vec<Option<Player>>,
    pub ore: Vec<OreDeposit>,
    /// Impassable ground. Regenerated from the seed, never sent on the wire.
    pub hills: Vec<Hill>,
    pub projectiles: Vec<Projectile>,
    next_projectile_id: u16,
    /// Deposits whose amount changed since the last full ore sync.
    dirty_ore: HashSet<u16>,
    /// Drained and broadcast each tick.
    pub events: Vec<GameEvent>,
    /// Impacts from this tick, for clients to draw. Cosmetic, so they ride
    /// the unreliable snapshot and are dropped rather than queued.
    fx: Vec<HitFx>,
    /// Players who have ever joined, so a solo player is not declared winner.
    joined: u8,
    /// Cheat mode: everything on the build list is free, and a sortie never
    /// runs out of fuel. Deliberately match-wide -- see
    /// [`ClientMessage::ToggleCheats`] -- and deliberately survives a restart,
    /// because it is a way of looking at the game rather than part of a match.
    pub cheats: bool,
}

impl Game {
    pub fn new(seed: u64) -> Self {
        Game {
            seed,
            tick: 0,
            status: GameStatus::Waiting,
            winner: None,
            players: vec![None; MAX_PLAYERS],
            ore: world::generate_ore(seed),
            hills: world::generate_hills(seed),
            projectiles: Vec::new(),
            next_projectile_id: 0,
            dirty_ore: HashSet::new(),
            events: Vec::new(),
            fx: Vec::new(),
            joined: 0,
            cheats: false,
        }
    }

    pub fn player(&self, id: u8) -> Option<&Player> {
        self.players.get(id as usize).and_then(|p| p.as_ref())
    }

    pub fn player_mut(&mut self, id: u8) -> Option<&mut Player> {
        self.players.get_mut(id as usize).and_then(|p| p.as_mut())
    }

    pub fn roster(&self) -> Vec<PlayerInfo> {
        self.players
            .iter()
            .flatten()
            .map(|p| PlayerInfo { id: p.id, name: p.name.clone(), connected: p.connected })
            .collect()
    }

    pub fn connected_count(&self) -> usize {
        self.players.iter().flatten().filter(|p| p.connected).count()
    }

    /// Finds the slot holding an identity, whether or not it is connected.
    pub fn player_by_token(&self, token: u64) -> Option<&Player> {
        self.players.iter().flatten().find(|p| p.token == token)
    }

    /// Admits a player, resuming an existing slot when the token is recognised.
    ///
    /// Returning the same slot for a known token is what makes a dropped
    /// connection recoverable: ore, power-ups, and both vehicles are exactly
    /// where they were left.
    pub fn join(&mut self, token: u64, name: &str) -> Result<u8, DenyReason> {
        // A name belongs to one player for the length of a match. Two of them
        // under the same name is not a cosmetic problem: the client derives its
        // identity token from the name, so the roster, the scoreboard, and the
        // slot itself would all be shared between two people.
        if !name.is_empty()
            && self.players.iter().flatten().any(|p| p.token != token && p.name == name)
        {
            return Err(DenyReason::NameTaken);
        }

        if let Some(p) = self.players.iter_mut().flatten().find(|p| p.token == token) {
            p.connected = true;
            if !name.is_empty() {
                p.name = name.to_owned();
            }
            // Input ticks are numbered per connection, and a client that has
            // restarted begins again from one. `set_input` drops anything not
            // newer than `acked_input`, so carrying the old high-water mark
            // across a resume would reject every frame the returning client
            // ever sends: it would sit on the field, fully connected and
            // unable to move. The stale frame goes too, so a key held at the
            // moment the old connection dropped does not drive the vehicle
            // until the first real frame lands.
            p.acked_input = 0;
            p.input = InputFrame::default();
            p.input_age = 0.0;
            let id = p.id;
            self.events.push(GameEvent::PlayerJoined { player: id });
            return Ok(id);
        }

        // Finished matches take no new players; a fresh one would have nothing
        // to do but watch.
        if self.status == GameStatus::Finished {
            return Err(DenyReason::MatchFinished);
        }

        let slot = self
            .players
            .iter()
            .position(Option::is_none)
            .ok_or(DenyReason::ServerFull)? as u8;
        let display = if name.is_empty() {
            world::PLAYER_COLOR_NAMES[slot as usize].to_owned()
        } else {
            name.to_owned()
        };
        self.players[slot as usize] = Some(Player::new(slot, token, display));
        self.joined = self.joined.saturating_add(1);
        self.events.push(GameEvent::PlayerJoined { player: slot });
        Ok(slot)
    }

    /// Marks a player offline. Their state and vehicles stay on the field.
    /// Restarts the match on a fresh map, keeping who is in it.
    ///
    /// Everything a player *earned* goes: credits, power-ups, mined totals,
    /// captures, and both vehicles, which return to their pads. Everything that
    /// identifies them stays -- slot, name, token -- so a restart is not a
    /// reconnect and nobody has to rejoin.
    ///
    /// `tick` deliberately keeps counting. Clients discard any snapshot whose
    /// tick is not newer than the last one they hold, so winding it back would
    /// make them ignore the entire new match.
    pub fn restart(&mut self, seed: u64, by: u8) {
        self.seed = seed;
        self.ore = world::generate_ore(seed);
        self.hills = world::generate_hills(seed);
        self.dirty_ore.clear();
        self.fx.clear();
        self.projectiles.clear();
        self.next_projectile_id = 0;
        self.winner = None;
        self.status = GameStatus::Waiting;

        for slot in self.players.iter_mut() {
            let Some(old) = slot.as_ref() else { continue };
            // Rebuilt rather than field-by-field reset, so a field added to
            // `Player` later cannot be forgotten here.
            let mut fresh = Player::new(old.id, old.token, old.name.clone());
            fresh.connected = old.connected;
            *slot = Some(fresh);
        }

        self.events.push(GameEvent::MatchReset { world_seed: seed, by });
        // A restart with enough players already present starts immediately;
        // this is also what re-announces `MatchStarted`.
        self.update_status();
    }

    pub fn disconnect(&mut self, id: u8) {
        if let Some(p) = self.player_mut(id) {
            if p.connected {
                p.connected = false;
                self.events.push(GameEvent::PlayerLeft { player: id });
            }
        }
    }

    /// Accepts an input frame, ignoring ones that arrive out of order.
    ///
    /// Inputs are unreliable and can be reordered by the network; replaying a
    /// stale frame would jerk the vehicle backwards.
    /// Changes what a player's miner does when left to itself.
    ///
    /// Takes effect on the next tick the player is not driving it; there is
    /// nothing to validate, since every mode is always available.
    pub fn set_miner_mode(&mut self, id: u8, mode: MinerMode) {
        if let Some(p) = self.player_mut(id) {
            p.miner_mode = mode;
            // A mode change is a fresh instruction; whatever it was stuck
            // against a moment ago is no longer the plan.
            p.stuck_for = 0.0;
            p.unstick_for = 0.0;
        }
    }

    pub fn set_input(&mut self, id: u8, frame: InputFrame) {
        if let Some(p) = self.player_mut(id) {
            if frame.tick >= p.acked_input || p.acked_input == 0 {
                p.acked_input = frame.tick;
                p.input = frame;
                p.input_age = 0.0;
            }
        }
    }

    /// Turns cheat mode on or off for everybody.
    pub fn toggle_cheats(&mut self) {
        self.cheats = !self.cheats;
    }

    pub fn purchase(&mut self, id: u8, powerup: PowerUp) {
        // Read before the player is borrowed.
        let cost = if self.cheats { 0 } else { powerup.cost() };
        let Some(p) = self.player_mut(id) else { return };
        let event = if p.eliminated {
            GameEvent::PurchaseRejected { powerup, reason: RejectReason::Eliminated }
        } else if powerup.held(p.powerups) {
            GameEvent::PurchaseRejected { powerup, reason: RejectReason::AlreadyOwned }
        } else if p.credits < cost {
            GameEvent::PurchaseRejected { powerup, reason: RejectReason::NotEnoughCredits }
        } else {
            p.credits -= cost;
            if powerup.is_consumable() {
                p.missiles = p.missiles.saturating_add(MISSILES_PER_PACK);
            } else {
                p.powerups |= powerup.bit();
                // Capacity upgrades take effect immediately rather than on the
                // next respawn, which is what a player expects after paying.
                let powerups = p.powerups;
                if let Some(v) = p.tank.as_mut() {
                    v.shield = v.shield.min(sim::max_shield(VehicleKind::Tank, powerups));
                    v.hull = v.hull.min(sim::max_hull(VehicleKind::Tank, powerups));
                }
                if let Some(v) = p.miner.as_mut() {
                    v.shield = v.shield.min(sim::max_shield(VehicleKind::Miner, powerups));
                    v.hull = v.hull.min(sim::max_hull(VehicleKind::Miner, powerups));
                }
            }
            GameEvent::PurchaseAccepted { powerup, credits: p.credits }
        };
        self.events.push(event);
    }

    /// Calls up a sortie, if the player has bought the aircraft and the last
    /// one is far enough behind them.
    ///
    /// Declines quietly. Every way this can fail is something the client
    /// already knows -- it has the power-up mask and the cooldown in every
    /// snapshot, and greys the key out -- so a rejection event here would only
    /// ever fire on a race, and a message about it would be noise.
    pub fn launch_plane(&mut self, id: u8) {
        let Some(p) = self.player_mut(id) else { return };
        if p.eliminated || p.plane.is_some() || p.sortie_cooldown > 0.0 {
            return;
        }
        if !PowerUp::Bomber.held(p.powerups) {
            return;
        }
        // It comes in over the player's own corner heading for the middle of
        // the field, which is the one bearing that is the same for everybody.
        // Starting it under the player's hand rather than flying itself in
        // would mean a stretch of the fuel spent getting somewhere, and there
        // is not enough of it for that.
        let base = world::base_position(id);
        let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - base).to_angle();
        let mut plane = Vehicle::spawn(VehicleKind::Plane, base, inward, p.powerups);
        plane.mv.speed = sim::PLANE_CRUISE;
        // Low in the band. Height is something a sortie has to spend fuel
        // climbing for, so arriving on top of a fight is not free.
        plane.mv.alt = sim::PLANE_ALTITUDE;
        p.plane = Some(plane);
    }

    // -----------------------------------------------------------------------
    // Simulation
    // -----------------------------------------------------------------------

    pub fn step(&mut self, dt: f32) {
        self.tick = self.tick.wrapping_add(1);
        self.update_status();
        for p in self.players.iter_mut().flatten() {
            p.input_age += dt;
        }

        if self.status == GameStatus::Finished {
            // Let projectiles finish flying, but stop everything else.
            self.step_projectiles(dt);
            return;
        }

        self.step_vehicles(dt);
        // After the ground and before the weapons, so a bomb released this tick
        // leaves from where the aircraft actually is. Outside `step_collisions`
        // entirely: nothing up there shares ground with anything.
        self.step_planes(dt);
        // Before weapons and capture, so both read where the hulls actually
        // ended up rather than where they were before being pushed apart.
        self.step_collisions();
        self.step_weapons(dt);
        self.step_sentinels(dt);
        self.step_projectiles(dt);
        self.step_economy(dt);
        self.step_capture(dt);
        self.step_respawns(dt);
        self.check_victory();
    }

    fn update_status(&mut self) {
        if self.status == GameStatus::Waiting && self.connected_count() >= 2 {
            self.status = GameStatus::Running;
            self.events.push(GameEvent::MatchStarted);
        }
    }

    /// Where an unattended miner should head, and how close counts as
    /// arrived. `None` means hold station -- either the player asked for that,
    /// or there is nothing left to go and get.
    fn autopilot_target(&self, player: u8) -> Option<(Vec2, f32)> {
        let p = self.player(player)?;
        let m = p.miner.as_ref()?;
        let home = (world::base_position(player), world::BASE_RADIUS * 0.6);

        match p.miner_mode {
            MinerMode::Stop => None,
            MinerMode::Home => Some(home),
            MinerMode::Auto => {
                // A full miner cannot mine, so the load is only worth
                // anything once it is back at the pad.
                if m.cargo >= sim::cargo_capacity(p.powerups) {
                    return Some(home);
                }
                let nearest = self.ore.iter().filter(|d| d.amount > 0.0).min_by(|a, b| {
                    a.pos
                        .distance_squared(m.mv.pos)
                        .total_cmp(&b.pos.distance_squared(m.mv.pos))
                })?;
                // Stop short of the middle, so braking settles it under
                // MINING_MAX_SPEED while still inside MINING_RADIUS.
                Some((nearest.pos, sim::MINING_RADIUS * 0.7))
            }
        }
    }

    fn step_vehicles(&mut self, dt: f32) {
        // Crashes are charged after the loop: applying damage needs the game as
        // a whole, and the loop has one player in hand at a time.
        let mut crashes: Vec<(u8, VehicleSlot, f32, f32)> = Vec::new();

        for slot_index in 0..self.players.len() {
            // Read before the player is borrowed: it looks at the ore field.
            let Some(id) = self.players[slot_index].as_ref().map(|p| p.id) else { continue };
            let auto = self.autopilot_target(id);

            let Some(p) = self.players[slot_index].as_mut() else { continue };
            let powerups = p.powerups;
            let input = p.effective_input();
            let controlling = input.controlling;
            let (throttle, steer, aim) = (input.throttle, input.steer, input.aim);

            // Ground vehicles. The sortie is flown by `step_planes`, which
            // shares almost nothing with this loop.
            //
            // Reached field by field rather than through `vehicle_mut`, which
            // would borrow the whole player and put the autopilot bookkeeping
            // below out of reach.
            for slot in [VehicleSlot::Tank, VehicleSlot::Miner] {
                let kind = slot.kind();
                let Some(v) = (match slot {
                    VehicleSlot::Tank => p.tank.as_mut(),
                    VehicleSlot::Miner => p.miner.as_mut(),
                    VehicleSlot::Plane => None,
                }) else {
                    continue;
                };

                let outcome = if v.disabled {
                    v.mv.speed = 0.0;
                    sim::StepOutcome::default()
                } else if slot == controlling {
                    sim::step_vehicle(&mut v.mv, throttle, steer, 0.0, kind, powerups, &self.hills, dt)
                } else if slot == VehicleSlot::Miner {
                    // Left alone, the miner works the field on whichever
                    // mode the player picked. This branch runs only while they
                    // are driving something else, so taking it over with TAB
                    // overrides the mode for free and letting go resumes it.
                    let (mut auto_throttle, mut auto_steer) = match auto {
                        Some((target, stop_within)) => {
                            sim::autopilot(&v.mv, target, stop_within, &self.hills)
                        }
                        None => (0.0, 0.0),
                    };

                    // Asking for throttle and going nowhere means wedged --
                    // shoved into a corner by another hull, or nosed into a
                    // slope the tangent led back onto.
                    if auto_throttle != 0.0 && v.mv.speed.abs() < sim::AUTOPILOT_STALL_SPEED {
                        p.stuck_for += dt;
                    } else {
                        p.stuck_for = 0.0;
                    }
                    if p.unstick_for > 0.0 {
                        p.unstick_for -= dt;
                        // Back out while turning, so the next attempt starts
                        // from a different heading instead of repeating this one.
                        auto_throttle = -1.0;
                        auto_steer = 1.0;
                    } else if p.stuck_for > 1.0 {
                        p.stuck_for = 0.0;
                        p.unstick_for = 1.0;
                    }

                    sim::step_vehicle(
                        &mut v.mv,
                        auto_throttle,
                        auto_steer,
                        0.0,
                        kind,
                        powerups,
                        &self.hills,
                        dt,
                    )
                } else {
                    // An unattended tank coasts to a halt and holds station.
                    sim::step_vehicle(&mut v.mv, 0.0, 0.0, 0.0, kind, powerups, &self.hills, dt)
                };

                // Driving into a hillside costs hull, above a speed that nosing
                // up to one never reaches. Turbo makes this worse, which is the
                // point: it buys speed, and speed is what hurts here.
                if outcome.terrain_impact > sim::IMPACT_THRESHOLD {
                    let over = outcome.terrain_impact - sim::IMPACT_THRESHOLD;
                    // The hill met the front of the hull, so that is the face the
                    // shield lights up on.
                    crashes.push((id, slot, over * sim::TERRAIN_IMPACT_DAMAGE, v.mv.yaw));
                }

                if slot == VehicleSlot::Tank {
                    let desired = if slot == controlling { aim } else { v.mv.yaw };
                    v.turret_yaw = sim::step_turret(v.turret_yaw, desired, dt);
                }

                // Shields come back only after a lull, so sustained fire stays
                // meaningful.
                v.since_damage += dt;
                if v.since_damage >= sim::SHIELD_REGEN_DELAY && !v.disabled {
                    let max = sim::max_shield(kind, powerups);
                    v.shield = (v.shield + sim::shield_regen_rate(powerups) * dt).min(max);
                }
                v.gun_cooldown = (v.gun_cooldown - dt).max(0.0);
                v.missile_cooldown = (v.missile_cooldown - dt).max(0.0);
            }
        }

        // Nobody to blame but the driver, so the crash is charged to them; a
        // tank lost this way is not a kill for anyone.
        for (player, slot, damage, bearing) in crashes {
            self.damage_vehicle(player, slot, damage, player, bearing);
        }
    }

    /// Flies the sortie, and ends it when the tank runs dry.
    ///
    /// Separate from [`Self::step_vehicles`] because almost none of that loop
    /// applies: there is no autopilot for an aircraft, no hull to crash, no
    /// shield to regenerate, and no turret to slew. What it does share is that
    /// it runs every tick whether or not the player is looking at it -- let go
    /// of, an aircraft keeps flying straight and keeps burning fuel, which is
    /// the difference between it and a tank left parked.
    fn step_planes(&mut self, dt: f32) {
        let cheats = self.cheats;
        for p in self.players.iter_mut().flatten() {
            p.sortie_cooldown = (p.sortie_cooldown - dt).max(0.0);

            let input = p.effective_input();
            let flying = input.controlling == VehicleSlot::Plane;
            let Some(plane) = p.plane.as_mut() else { continue };

            // Hands off the moment the player looks away: an unattended sortie
            // holds its heading, its height and its speed, and burns fuel doing
            // it. Zero on the yoke is "hold this altitude", not "descend".
            let (throttle, steer, climb) =
                if flying { (input.throttle, input.steer, input.climb) } else { (0.0, 0.0, 0.0) };
            let _ = sim::step_vehicle(
                &mut plane.mv,
                throttle,
                steer,
                climb,
                VehicleKind::Plane,
                p.powerups,
                &[],
                dt,
            );
            // The aircraft has no turret of its own: it bombs what it is flying
            // over, so the hull's heading is the only bearing it has.
            plane.turret_yaw = plane.mv.yaw;
            plane.gun_cooldown = (plane.gun_cooldown - dt).max(0.0);
            if !cheats {
                plane.fuel -= dt;
            }

            // One way a sortie ends: the tank runs dry. Flying out of the
            // field used to be the other, and is not any more -- the edge banks
            // the aircraft back inside instead. At sixty seconds of fuel
            // against a field nineteen seconds across, the wall would otherwise
            // have finished nearly every sortie before the fuel did.
            if plane.fuel <= 0.0 {
                p.plane = None;
                // Timed from the sortie ending rather than from the launch, so
                // flying one badly and losing it early is not rewarded with a
                // quicker second go.
                p.sortie_cooldown = sim::SORTIE_COOLDOWN;
            }
        }
    }

    /// Keeps hulls out of each other, and charges both for a hard meeting.
    ///
    /// Vehicles are circles that cannot share ground: anything that ends a tick
    /// inside another hull is pushed back out along the line between the two,
    /// and loses the speed it was carrying in. Meeting fast enough costs both
    /// sides hull, so ramming is a real move with a real price rather than a way
    /// to park inside somebody.
    ///
    /// Server-only. The client predicts its own vehicle with [`sim::step_vehicle`]
    /// and knows nothing about the others, so a push shows up there as a small
    /// correction on the next snapshot. That is fine because contact is
    /// transient by construction -- the hulls separate the same tick they meet --
    /// and the error never approaches the threshold that would snap the camera.
    fn step_collisions(&mut self) {
        let mut bodies: Vec<Body> = Vec::new();
        for p in self.players.iter().flatten() {
            if p.eliminated {
                continue;
            }
            // Ground vehicles only. Nothing at `sim::PLANE_ALTITUDE` shares
            // ground with anything, and a tank bouncing off an aircraft 26
            // units over its head is not a collision anybody would accept.
            for slot in [VehicleSlot::Tank, VehicleSlot::Miner] {
                let Some(v) = p.vehicle(slot) else { continue };
                bodies.push(Body {
                    player: p.id,
                    slot,
                    pos: v.mv.pos,
                    radius: sim::tuning(slot.kind()).radius,
                    vel: Vec2::from_angle(v.mv.yaw) * v.mv.speed,
                    speed_scale: 1.0,
                    fixed: v.disabled,
                });
            }
        }

        let mut rams: Vec<(u8, VehicleSlot, f32, u8, f32)> = Vec::new();
        for i in 0..bodies.len() {
            for j in (i + 1)..bodies.len() {
                // Copied out, so the positions already nudged by earlier pairs
                // in this pass are the ones being compared.
                let (a, b) = (bodies[i], bodies[j]);
                let Some((axis, overlap)) = sim::overlap_push(a.pos, a.radius, b.pos, b.radius)
                else {
                    continue;
                };

                // Ground is given by whoever can give it.
                let (give_a, give_b) = match (a.fixed, b.fixed) {
                    (true, true) => (0.0, 0.0),
                    (true, false) => (0.0, 1.0),
                    (false, true) => (1.0, 0.0),
                    (false, false) => (0.5, 0.5),
                };
                bodies[i].pos -= axis * (overlap * give_a);
                bodies[j].pos += axis * (overlap * give_b);

                // Whichever of them was driving into the other loses most of
                // what it was carrying. Scaled by how head-on it was, so sliding
                // along somebody costs nothing and running them down costs a
                // lot -- the same shape as the hill in `sim::step_vehicle`.
                let into_a = axis.dot(a.vel.normalize_or_zero()).clamp(0.0, 1.0);
                let into_b = (-axis).dot(b.vel.normalize_or_zero()).clamp(0.0, 1.0);
                bodies[i].speed_scale *= 1.0 - sim::RAM_SPEED_LOSS * into_a;
                bodies[j].speed_scale *= 1.0 - sim::RAM_SPEED_LOSS * into_b;

                // Your own two vehicles bump each other constantly; only an
                // enemy costs hull.
                if a.player == b.player {
                    continue;
                }
                // How fast the gap was actually shrinking. Two tanks driving at
                // each other close at twice their own speed; a tank running down
                // a parked one closes at its own.
                let closing = (a.vel - b.vel).dot(axis);
                if closing <= sim::IMPACT_THRESHOLD {
                    continue;
                }
                let damage = (closing - sim::IMPACT_THRESHOLD) * sim::RAM_DAMAGE;
                // Both pay the same, each on the face that met the other. The
                // partner is named as the attacker, so running somebody down is
                // a kill you get credit for.
                rams.push((a.player, a.slot, damage, b.player, axis.to_angle()));
                rams.push((b.player, b.slot, damage, a.player, (-axis).to_angle()));
            }
        }

        for body in &bodies {
            let Some(p) = self.players.get_mut(body.player as usize).and_then(Option::as_mut)
            else {
                continue;
            };
            let Some(v) = p.vehicle_mut(body.slot) else { continue };
            // A push can put a hull past the edge of the world, or into a hill.
            // The wall is clamped here because nothing else will; a hill is left
            // to `step_vehicle`, which shoves everything clear at the top of the
            // next tick anyway.
            let (lo, hi) = (body.radius, world::WORLD_SIZE - body.radius);
            v.mv.pos = Vec2::new(body.pos.x.clamp(lo, hi), body.pos.y.clamp(lo, hi));
            v.mv.speed *= body.speed_scale;
        }

        for (player, slot, damage, attacker, bearing) in rams {
            self.damage_vehicle(player, slot, damage, attacker, bearing);
        }
    }

    fn step_weapons(&mut self, dt: f32) {
        let _ = dt;
        let targets = self.collect_targets();
        let mut spawned: Vec<Projectile> = Vec::new();

        for slot_index in 0..self.players.len() {
            let Some(p) = self.players[slot_index].as_mut() else { continue };
            if p.eliminated {
                continue;
            }
            let owner = p.id;

            // Tank guns, under direct control.
            let input = p.effective_input();
            if input.controlling == VehicleSlot::Tank {
                let fire_primary = input.fire_primary;
                let fire_secondary = input.fire_secondary;
                let missiles = p.missiles;
                let powerups = p.powerups;
                if let Some(tank) = p.tank.as_mut() {
                    let muzzle = tank.mv.pos + Vec2::from_angle(tank.turret_yaw) * 3.4;
                    if fire_primary && tank.gun_cooldown <= 0.0 {
                        tank.gun_cooldown = sim::BULLET_COOLDOWN;
                        spawned.push(Projectile {
                            id: 0,
                            kind: ProjectileKind::Bullet,
                            owner,
                            pos: muzzle,
                            yaw: tank.turret_yaw,
                            speed: sim::BULLET_SPEED,
                            life: sim::bullet_lifetime(powerups),
                        });
                    }
                    if fire_secondary && tank.missile_cooldown <= 0.0 && missiles > 0 {
                        tank.missile_cooldown = sim::MISSILE_COOLDOWN;
                        p.missiles = missiles - 1;
                        spawned.push(Projectile {
                            id: 0,
                            kind: ProjectileKind::Missile,
                            owner,
                            pos: muzzle,
                            yaw: tank.turret_yaw,
                            speed: sim::MISSILE_LAUNCH_SPEED,
                            life: sim::MISSILE_LIFETIME,
                        });
                    }
                }
            }

            // The bomb bay. There is nothing to aim: a bomb leaves with the
            // aircraft's own velocity and falls, so where it lands is decided
            // by where the aircraft was pointing when it was let go. The
            // secondary trigger does nothing up here.
            if input.controlling == VehicleSlot::Plane && input.fire_primary {
                if let Some(plane) = p.plane.as_mut() {
                    if plane.gun_cooldown <= 0.0 {
                        plane.gun_cooldown = sim::BOMB_COOLDOWN;
                        spawned.push(Projectile {
                            id: 0,
                            kind: ProjectileKind::Bomb,
                            owner,
                            pos: plane.mv.pos,
                            yaw: plane.mv.yaw,
                            speed: plane.mv.speed,
                            // The fall is the height it was let go from, so a
                            // bomb from the ceiling is a long throw and one off
                            // the floor lands almost underneath. The client's
                            // sight reads the same function.
                            life: sim::bomb_fall_time(plane.mv.alt),
                        });
                    }
                }
            }

            // The miner cannot be aimed by the player; with the Auto Turret
            // upgrade it defends itself.
            if PowerUp::AutoTurret.held(p.powerups) {
                if let Some(m) = p.miner.as_mut() {
                    if !m.disabled {
                        let nearest = targets
                            .iter()
                            .filter(|t| t.player != owner)
                            .map(|t| (t.pos.distance(m.mv.pos), t.pos))
                            .filter(|(d, _)| *d <= sim::AUTO_TURRET_RANGE)
                            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                        if let Some((_, target_pos)) = nearest {
                            let desired = (target_pos - m.mv.pos).to_angle();
                            m.turret_yaw = sim::step_turret(m.turret_yaw, desired, dt);
                            if m.gun_cooldown <= 0.0
                                && angle_delta(m.turret_yaw, desired).abs() < 0.15
                            {
                                m.gun_cooldown = sim::AUTO_TURRET_COOLDOWN;
                                spawned.push(Projectile {
                                    id: 0,
                                    kind: ProjectileKind::Bullet,
                                    owner,
                                    pos: m.mv.pos + Vec2::from_angle(m.turret_yaw) * 3.2,
                                    yaw: m.turret_yaw,
                                    speed: sim::BULLET_SPEED,
                                    life: sim::shell_life_covering(sim::AUTO_TURRET_RANGE),
                                });
                            }
                        }
                    }
                }
            }
        }

        for mut proj in spawned {
            proj.id = self.next_projectile_id;
            self.next_projectile_id = self.next_projectile_id.wrapping_add(1);
            self.projectiles.push(proj);
        }
    }

    /// Base emplacements: pick a target, slew onto it, fire.
    ///
    /// Deliberately shaped like the Auto Turret block in [`Self::step_weapons`],
    /// which solved the same problem for the miner -- the difference is that
    /// this one works through the enemies in range in turn rather than always
    /// engaging the nearest, so a pair of attackers cannot have one of them
    /// soak every shell while the other works unmolested.
    fn step_sentinels(&mut self, dt: f32) {
        let targets = self.collect_targets();
        let mut spawned: Vec<Projectile> = Vec::new();

        for slot_index in 0..self.players.len() {
            let Some(p) = self.players[slot_index].as_mut() else { continue };
            if p.eliminated {
                continue;
            }
            let owner = p.id;
            let post = world::sentinel_position(owner);
            let s = &mut p.sentinel;

            if !s.standing() {
                s.rebuild_timer -= dt;
                if s.standing() {
                    s.hull = sim::SENTINEL_HULL;
                    self.events.push(GameEvent::SentinelRebuilt { player: owner });
                }
                continue;
            }

            s.gun_cooldown = (s.gun_cooldown - dt).max(0.0);

            let in_range: Vec<Vec2> = targets
                .iter()
                .filter(|t| t.player != owner && t.what != Hittable::Sentinel)
                .filter(|t| t.pos.distance(post) <= sim::SENTINEL_RANGE)
                .map(|t| t.pos)
                .collect();
            if in_range.is_empty() {
                continue;
            }

            // The cursor only moves when a shell leaves, so the emplacement
            // stays committed until it has actually taken its shot.
            let target = in_range[s.cursor % in_range.len()];
            let desired = (target - post).to_angle();
            s.turret_yaw = sim::step_turret(s.turret_yaw, desired, dt);

            if s.gun_cooldown <= 0.0 && angle_delta(s.turret_yaw, desired).abs() < 0.15 {
                s.gun_cooldown = sim::SENTINEL_COOLDOWN;
                s.cursor = s.cursor.wrapping_add(1);
                spawned.push(Projectile {
                    id: 0,
                    kind: ProjectileKind::Bullet,
                    owner,
                    pos: post + Vec2::from_angle(s.turret_yaw) * (sim::SENTINEL_RADIUS + 1.0),
                    yaw: s.turret_yaw,
                    speed: sim::BULLET_SPEED,
                    life: sim::shell_life_covering(sim::SENTINEL_RANGE),
                });
            }
        }

        for mut proj in spawned {
            proj.id = self.next_projectile_id;
            self.next_projectile_id = self.next_projectile_id.wrapping_add(1);
            self.projectiles.push(proj);
        }
    }

    fn collect_targets(&self) -> Vec<Target> {
        let mut out = Vec::new();
        for p in self.players.iter().flatten() {
            if p.eliminated {
                continue;
            }
            if let Some(v) = &p.tank {
                out.push(Target {
                    player: p.id,
                    what: Hittable::Vehicle(VehicleSlot::Tank),
                    pos: v.mv.pos,
                    radius: sim::tuning(VehicleKind::Tank).radius,
                });
            }
            if let Some(v) = &p.miner {
                out.push(Target {
                    player: p.id,
                    what: Hittable::Vehicle(VehicleSlot::Miner),
                    pos: v.mv.pos,
                    radius: sim::tuning(VehicleKind::Miner).radius,
                });
            }
            // The aircraft is deliberately absent. Everything that shoots in
            // this game shoots along the ground, and the one limit on a sortie
            // is the fuel clock -- putting the plane in here would have shells
            // that visibly pass underneath it taking it down. `HitFx` is the
            // second reason: it packs the slot into a single bit, so a hit on
            // the plane could not even be described to a client.
            //
            // Rubble is not worth shooting at.
            if p.sentinel.standing() {
                out.push(Target {
                    player: p.id,
                    what: Hittable::Sentinel,
                    pos: world::sentinel_position(p.id),
                    radius: sim::SENTINEL_RADIUS,
                });
            }
        }
        out
    }

    fn step_projectiles(&mut self, dt: f32) {
        let targets = self.collect_targets();
        // Moved aside for the duration: `retain_mut` below holds a mutable
        // borrow of `self.projectiles`, and the terrain cannot change mid-tick.
        let hills = std::mem::take(&mut self.hills);
        let mut hits: Vec<(u8, Hittable, f32, u8, f32)> = Vec::new();
        // Bombs that reached the ground this tick, as (owner, where).
        // Collected rather than resolved in place: a blast reads the whole
        // target list, and `retain_mut` has the projectiles in hand.
        let mut blasts: Vec<(u8, Vec2)> = Vec::new();
        // Moved aside for the same reason the terrain is: `retain_mut` holds
        // `self.projectiles` for the whole pass.
        let mut fx = std::mem::take(&mut self.fx);

        self.projectiles.retain_mut(|proj| {
            proj.life -= dt;
            if proj.life <= 0.0 {
                // For everything else running out of life is falling short.
                // For a bomb it is the whole point: the life *is* the fall, so
                // reaching the end of it is reaching the ground.
                if proj.kind == ProjectileKind::Bomb {
                    blasts.push((proj.owner, proj.pos));
                }
                return false;
            }

            if proj.kind == ProjectileKind::Bomb {
                // Still in the air, where there is nothing to run into. It
                // holds the velocity the aircraft let it go with, which is what
                // makes `sim::bomb_impact` -- and so the sight on the client --
                // exactly right rather than nearly right.
                proj.pos += Vec2::from_angle(proj.yaw) * (proj.speed * dt);
                return true;
            }

            if proj.kind == ProjectileKind::Missile {
                // Seek the nearest target inside a forward cone, so missiles can
                // be broken off by getting out of their arc.
                let heading = Vec2::from_angle(proj.yaw);
                let best = targets
                    .iter()
                    .filter(|t| t.player != proj.owner)
                    .filter_map(|t| {
                        let to = t.pos - proj.pos;
                        let dist = to.length();
                        if dist > sim::MISSILE_SEEK_RANGE || dist < 1e-3 {
                            return None;
                        }
                        let bearing = to.to_angle();
                        if angle_delta(proj.yaw, bearing).abs() > sim::MISSILE_SEEK_CONE {
                            return None;
                        }
                        Some((dist, bearing))
                    })
                    .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                let _ = heading;
                if let Some((_, bearing)) = best {
                    proj.yaw = orewar_shared::math::angle_approach(
                        proj.yaw,
                        bearing,
                        sim::MISSILE_TURN_RATE * dt,
                    );
                }
                proj.speed = (proj.speed + sim::MISSILE_ACCEL * dt).min(sim::MISSILE_MAX_SPEED);
            }

            let from = proj.pos;
            let to = from + Vec2::from_angle(proj.yaw) * (proj.speed * dt);

            // Swept test: a projectile covers several units per tick, so a
            // point test at each end would let glancing shots pass through.
            let mut earliest: Option<(f32, &Target)> = None;
            for t in targets.iter().filter(|t| t.player != proj.owner) {
                if let Some(hit) = sim::segment_circle_hit(from, to, t.pos, t.radius) {
                    if earliest.map_or(true, |(best, _)| hit < best) {
                        earliest = Some((hit, t));
                    }
                }
            }

            // Terrain is cover: a hill in front of the target eats the shot.
            // Tested against the same swept segment and compared on the same
            // scale, so whichever is actually nearer is what stops it.
            if let Some(terrain) = sim::segment_hill_hit(from, to, &hills) {
                if earliest.map_or(true, |(best, _)| terrain < best) {
                    if proj.kind == ProjectileKind::Missile {
                        fx.push(HitFx::on_terrain(HitKind::Blast, from.lerp(to, terrain)));
                    }
                    return false;
                }
            }

            if let Some((at, target)) = earliest {
                let damage = match proj.kind {
                    ProjectileKind::Bullet => sim::BULLET_DAMAGE,
                    ProjectileKind::Missile => sim::MISSILE_DAMAGE,
                    // A bomb never reaches this: it is at altitude for its
                    // whole flight and is skipped by the pass that gets here.
                    // What it does is done where it lands, to everything within
                    // a radius rather than to the one thing in its way.
                    ProjectileKind::Bomb => 0.0,
                };
                // The bearing from the hull's centre out to where it was
                // struck, which is the face the shield has to flash on.
                let impact = from.lerp(to, at);
                let bearing = (impact - target.pos).to_angle();
                if proj.kind == ProjectileKind::Missile {
                    fx.push(HitFx::on_terrain(HitKind::Blast, impact));
                }
                hits.push((target.player, target.what, damage, proj.owner, bearing));
                return false;
            }

            proj.pos = to;
            let bound = world::WORLD_SIZE;
            proj.pos.x > 0.0 && proj.pos.x < bound && proj.pos.y > 0.0 && proj.pos.y < bound
        });

        self.hills = hills;
        self.fx = fx;

        // A bomb is aimed at a place, not at a hull: everything standing close
        // enough pays, on a slope from the middle of the blast out to nothing
        // at the rim. That is the one weapon in the game where missing by a
        // little still costs the target something, which is what a stick of
        // them dropped across a position is for.
        for (owner, at) in blasts {
            self.fx.push(HitFx::on_terrain(HitKind::Blast, at));
            // Ore in the blast is gone, not reduced. A deposit is the one thing
            // on the field that cannot move out of the way, and denying one
            // outright is the reason to spend a sortie on empty ground rather
            // than on somebody's hull.
            for (i, deposit) in self.ore.iter_mut().enumerate() {
                if deposit.amount > 0.0 && deposit.pos.distance(at) <= sim::BOMB_BLAST_RADIUS {
                    deposit.amount = 0.0;
                    self.dirty_ore.insert(i as u16);
                }
            }
            for t in &targets {
                if t.player == owner {
                    continue;
                }
                let distance = t.pos.distance(at);
                let damage = sim::blast_damage(distance, sim::BOMB_BLAST_RADIUS, sim::BOMB_DAMAGE);
                if damage <= 0.0 {
                    continue;
                }
                // The bearing from the hull out to the blast, so a shield
                // flashes on the side the bomb went off, the same as for a shell.
                let bearing = (at - t.pos).to_angle();
                hits.push((t.player, t.what, damage, owner, bearing));
            }
        }

        for (player, what, damage, attacker, bearing) in hits {
            match what {
                Hittable::Vehicle(slot) => {
                    self.damage_vehicle(player, slot, damage, attacker, bearing)
                }
                Hittable::Sentinel => self.damage_sentinel(player, damage),
            }
        }
    }

    /// An emplacement has no shield to flash and nothing to follow, so a hit on
    /// one is only ever the static blast the missile already reported.
    fn damage_sentinel(&mut self, player: u8, damage: f32) {
        let Some(p) = self.players.get_mut(player as usize).and_then(Option::as_mut) else {
            return;
        };
        if !p.sentinel.standing() {
            return;
        }
        p.sentinel.hull = (p.sentinel.hull - damage).max(0.0);
        if p.sentinel.hull > 0.0 {
            return;
        }
        p.sentinel.rebuild_timer = sim::SENTINEL_REBUILD;
        p.sentinel.gun_cooldown = 0.0;
        self.events.push(GameEvent::SentinelDestroyed { player });
    }

    /// `bearing` is the direction the blow came in on, used only to place
    /// the shield flash on the right face.
    fn damage_vehicle(
        &mut self,
        player: u8,
        slot: VehicleSlot,
        damage: f32,
        attacker: u8,
        bearing: f32,
    ) {
        // Reached through the field rather than through `player_mut`, so that
        // borrowing one player leaves the rest of the game -- `events`, `fx` --
        // still reachable while the vehicle is in hand.
        let Some(p) = self.players.get_mut(player as usize).and_then(Option::as_mut) else {
            return;
        };
        let powerups = p.powerups;
        let Some(v) = p.vehicle_mut(slot) else { return };
        if v.disabled {
            return;
        }

        v.since_damage = 0.0;
        // Read before the damage lands: afterwards a broken shield and a
        // shield that was already down look the same.
        let absorbed = v.shield > 0.0;
        let at = v.mv.pos;
        let hull = sim::apply_damage(&mut v.shield, &mut v.hull, damage);
        if absorbed {
            self.fx.push(HitFx::on_vehicle(HitKind::Shield, at, bearing, player, slot));
        }
        if hull > 0.0 {
            return;
        }

        match slot {
            VehicleSlot::Tank => {
                // Tanks are replaceable; losing one costs you tempo, not the match.
                p.tank = None;
                p.respawn_timer = sim::TANK_RESPAWN_DELAY;
                self.events.push(GameEvent::TankDestroyed { player, by: attacker });
            }
            // Nothing shoots at the aircraft, so nothing damages it and this
            // is unreachable. Left explicit rather than folded into a wildcard:
            // if something ever does learn to shoot upward, this is the line
            // that has to decide what being hit up there means.
            VehicleSlot::Plane => {}
            VehicleSlot::Miner => {
                // Miners are never destroyed. They go dead in the water and
                // become something an enemy tank has to come and take.
                v.disabled = true;
                v.capture_progress = 0.0;
                // The cargo stays aboard. Whoever reaches the wreck first gets
                // it: the owner carries it home on a rescue, an enemy takes it
                // with the capture. Destroying the load outright made disabling
                // a loaded miner worth less than catching it at the pad.
                let _ = powerups;
                self.events.push(GameEvent::MinerDisabled { player });
            }
        }
    }

    fn step_economy(&mut self, dt: f32) {
        for slot_index in 0..self.players.len() {
            let Some(p) = self.players[slot_index].as_mut() else { continue };
            if p.eliminated {
                continue;
            }
            let powerups = p.powerups;
            let id = p.id;
            let Some(m) = p.miner.as_mut() else { continue };
            if m.disabled {
                continue;
            }

            // Draw ore while stopped over a deposit.
            let capacity = sim::cargo_capacity(powerups);
            if m.cargo < capacity && m.mv.speed.abs() <= sim::MINING_MAX_SPEED {
                let mut best: Option<usize> = None;
                let mut best_dist = f32::MAX;
                for (i, deposit) in self.ore.iter().enumerate() {
                    if deposit.amount <= 0.0 {
                        continue;
                    }
                    let d = deposit.pos.distance(m.mv.pos);
                    if d <= sim::MINING_RADIUS && d < best_dist {
                        best_dist = d;
                        best = Some(i);
                    }
                }
                if let Some(i) = best {
                    let take = (sim::MINING_RATE * dt).min(capacity - m.cargo).min(self.ore[i].amount);
                    self.ore[i].amount -= take;
                    m.cargo += take;
                    self.dirty_ore.insert(i as u16);
                }
            }

            // Unload at the home pad.
            if m.cargo > 0.0 && sim::is_at_base(m.mv.pos, id) {
                let moved = (sim::UNLOAD_RATE * dt).min(m.cargo);
                m.cargo -= moved;
                // Credits are whole units; the fraction stays aboard rather
                // than evaporating.
                let whole = moved.floor().max(0.0) as u32;
                let remainder = moved - whole as f32;
                m.cargo += remainder;
                p.credits += whole;
                p.ore_mined += whole;
            }
        }
    }

    fn step_capture(&mut self, dt: f32) {
        // Positions of every tank, so we can see who is standing over a wreck.
        let tanks: Vec<(u8, Vec2)> = self
            .players
            .iter()
            .flatten()
            .filter(|p| !p.eliminated)
            .filter_map(|p| p.tank.as_ref().map(|t| (p.id, t.mv.pos)))
            .collect();

        let mut captures: Vec<(u8, u8)> = Vec::new();
        let mut rescued: Vec<u8> = Vec::new();

        for slot_index in 0..self.players.len() {
            let Some(p) = self.players[slot_index].as_mut() else { continue };
            if p.eliminated {
                continue;
            }
            let owner = p.id;
            let powerups = p.powerups;
            let Some(m) = p.miner.as_mut() else { continue };
            if !m.disabled {
                continue;
            }

            let mut captor: Option<u8> = None;
            let mut owner_present = false;
            for (tank_owner, pos) in &tanks {
                if pos.distance(m.mv.pos) > sim::CAPTURE_RADIUS {
                    continue;
                }
                if *tank_owner == owner {
                    owner_present = true;
                } else if captor.is_none() {
                    captor = Some(*tank_owner);
                }
            }

            match (owner_present, captor) {
                // Both sides standing on the wreck. The defender denies the
                // capture, but repairing under the guns of the tank parked on
                // top of it is not something they get for free: progress holds
                // where it is, and whoever gives up the ground first decides how
                // this ends. Without this the defender simply outran the capture
                // -- a rescue takes about three seconds against a four-second
                // capture -- so a contested miner could never be taken and
                // spent the fight flicking between wreck and running.
                (true, Some(_)) => {}

                // Your own tank alone over your miner patches it up, which
                // is what gives a disabled miner a way back into the match.
                (true, None) => {
                    let max_hull = sim::max_hull(VehicleKind::Miner, powerups);
                    m.hull = (m.hull + sim::RESCUE_REPAIR_RATE * dt).min(max_hull);
                    m.capture_progress = (m.capture_progress - dt / sim::CAPTURE_TIME).max(0.0);
                    if m.hull >= max_hull * sim::REENABLE_HULL_FRACTION {
                        m.disabled = false;
                        m.capture_progress = 0.0;
                        m.shield = 0.0;
                        m.since_damage = 0.0;
                        rescued.push(owner);
                    }
                }

                (false, Some(by)) => {
                    m.capture_progress += dt / sim::CAPTURE_TIME;
                    if m.capture_progress >= 1.0 {
                        captures.push((by, owner));
                    }
                }

                // Nobody in range; progress decays so a partial attempt does
                // not linger indefinitely.
                (false, None) => {
                    m.capture_progress =
                        (m.capture_progress - dt / (sim::CAPTURE_TIME * 2.0)).max(0.0);
                }
            }
        }

        for player in rescued {
            self.events.push(GameEvent::MinerRescued { player });
        }
        for (by, from) in captures {
            self.apply_capture(by, from);
        }
    }

    fn apply_capture(&mut self, by: u8, from: u8) {
        let mut spoils = 0u32;
        if let Some(victim) = self.player_mut(from) {
            // Whole units only, matching how a miner unloads at its own pad;
            // the fraction is lost with the hull rather than rounded up.
            spoils = victim.miner.as_ref().map_or(0.0, |m| m.cargo).floor().max(0.0) as u32;
            victim.miner = None;
            // A player with no miner has nothing left to defend, so their
            // tank leaves the field with it. They are off the field rather than
            // out of the match: `eliminated` means "not here right now", and
            // `down_for` is how long that lasts.
            victim.tank = None;
            // Anything in the air goes with them. The cooldown starts now and
            // runs through the lockout, which is shorter than the lockout is --
            // so they do come back with a sortie available. That is deliberate:
            // they come back with nothing else.
            victim.plane = None;
            victim.sortie_cooldown = sim::SORTIE_COOLDOWN;
            victim.eliminated = true;
            victim.down_for = sim::CAPTURE_LOCKOUT;
            // Nothing to come back to on the tank timer; the whole player is on
            // the clock now.
            victim.respawn_timer = 0.0;
        }
        if let Some(captor) = self.player_mut(by) {
            captor.captures = captor.captures.saturating_add(1);
            captor.credits += spoils;
            captor.ore_mined += spoils;
        }
        self.events.push(GameEvent::MinerCaptured { by, from });
        // Only worth saying when there was something aboard.
        if spoils > 0 {
            self.events.push(GameEvent::OreSeized { by, from, amount: spoils });
        }
        self.events.push(GameEvent::PlayerEliminated { player: from });
        // Anything still in the air belonged to a fight that is now over.
        self.projectiles.retain(|p| p.owner != from);
    }

    fn step_respawns(&mut self, dt: f32) {
        let mut respawned = Vec::new();
        let mut returned = Vec::new();

        for slot_index in 0..self.players.len() {
            let Some(p) = self.players[slot_index].as_mut() else { continue };

            // A player whose miner was taken sits the minute out and then
            // starts again: fresh vehicles, an empty bank, and every upgrade
            // they had bought still theirs. Losing a miner costs a minute
            // and everything liquid, not the match.
            if p.down_for > 0.0 {
                p.down_for -= dt;
                if p.down_for > 0.0 {
                    continue;
                }
                let (tank_pos, miner_pos, inward) = starting_placement(p.id);
                p.down_for = 0.0;
                p.eliminated = false;
                p.credits = 0;
                p.missiles = STARTING_MISSILES;
                p.tank =
                    Some(Vehicle::spawn(VehicleKind::Tank, tank_pos, inward, p.powerups));
                p.miner = Some(Vehicle::spawn(
                    VehicleKind::Miner,
                    miner_pos,
                    inward,
                    p.powerups,
                ));
                // Whatever it was doing when it was taken is not a plan for the
                // new one.
                p.stuck_for = 0.0;
                p.unstick_for = 0.0;
                returned.push(p.id);
                continue;
            }

            if p.eliminated || p.tank.is_some() {
                continue;
            }
            p.respawn_timer -= dt;
            if p.respawn_timer > 0.0 {
                continue;
            }
            let base = world::base_position(p.id);
            let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - base).to_angle();
            p.tank = Some(Vehicle::spawn(
                VehicleKind::Tank,
                base + Vec2::from_angle(inward) * 7.0,
                inward,
                p.powerups,
            ));
            respawned.push(p.id);
        }
        for player in respawned {
            self.events.push(GameEvent::TankRespawned { player });
        }
        for player in returned {
            self.events.push(GameEvent::PlayerReturned { player });
        }
    }

    /// Being the only one left on the field wins it, and so does taking three
    /// miners.
    ///
    /// Last one standing is the ending the game is actually about: you win by
    /// taking everybody else's miner. A capture only puts its victim off the
    /// field for a minute, so it is checked against who is on the field *now* --
    /// in a two-player match that means one capture ends it, before the minute
    /// has a chance to run out and hand the loser a second life nobody is left
    /// to contest.
    ///
    /// Only `apply_capture` ever takes a player off the field, so "one left" is
    /// reachable only through a capture; no separate check for that is needed.
    /// Being disconnected is deliberately not the same as being off the field --
    /// the vehicles of a player who drops stay where they are and can still be
    /// taken, and a match should not change hands because somebody's wifi did.
    ///
    /// The three-capture ending still stands, for the matches with enough
    /// players that everyone keeps coming back.
    fn check_victory(&mut self) {
        if self.status != GameStatus::Running || self.joined < 2 {
            return;
        }
        let mut standing = self.players.iter().flatten().filter(|p| !p.eliminated);
        let last_standing = match (standing.next(), standing.next()) {
            (Some(p), None) => Some(p.id),
            _ => None,
        };
        let leader = self
            .players
            .iter()
            .flatten()
            .find(|p| p.captures >= sim::CAPTURES_TO_WIN)
            .map(|p| p.id);
        if let Some(winner) = last_standing.or(leader) {
            self.status = GameStatus::Finished;
            self.winner = Some(winner);
            self.events.push(GameEvent::GameOver { winner });
        }
    }

    // -----------------------------------------------------------------------
    // Snapshots
    // -----------------------------------------------------------------------

    /// Builds the view of the world sent to one player.
    ///
    /// Projectiles are ranked by distance from that player's own vehicle and cut
    /// to a fixed budget, because bullets are dense and a far-off firefight is
    /// not worth risking a fragmented packet over.
    pub fn snapshot_for(&self, viewer: u8, full_ore: bool) -> Snapshot {
        let eye = self
            .player(viewer)
            .and_then(|p| {
                p.vehicle(p.input.controlling).or(p.tank.as_ref()).or(p.miner.as_ref())
            })
            .map(|v| v.mv.pos)
            .unwrap_or(Vec2::splat(world::WORLD_SIZE * 0.5));

        let mut projectiles: Vec<&Projectile> = self.projectiles.iter().collect();
        if projectiles.len() > MAX_PROJECTILES_PER_SNAPSHOT {
            projectiles.sort_by(|a, b| {
                a.pos.distance_squared(eye).partial_cmp(&b.pos.distance_squared(eye)).unwrap()
            });
            projectiles.truncate(MAX_PROJECTILES_PER_SNAPSHOT);
        }

        // Cosmetic and capped, so when more happens at once than fits, the
        // nearest impacts are the ones worth the bytes.
        let mut hits = self.fx.clone();
        if hits.len() > MAX_HITS_PER_SNAPSHOT {
            let eye = self
                .player(viewer)
                .and_then(|p| p.tank.as_ref().or(p.miner.as_ref()))
                .map_or(Vec2::splat(world::WORLD_SIZE * 0.5), |v| v.mv.pos);
            hits.sort_by(|a, b| {
                a.pos.distance_squared(eye).total_cmp(&b.pos.distance_squared(eye))
            });
            hits.truncate(MAX_HITS_PER_SNAPSHOT);
        }

        let ore = if full_ore {
            self.ore
                .iter()
                .enumerate()
                .map(|(i, o)| OreUpdate { id: i as u16, amount: o.amount.round().max(0.0) as u16 })
                .collect()
        } else {
            self.dirty_ore
                .iter()
                .filter_map(|&id| {
                    self.ore.get(id as usize).map(|o| OreUpdate {
                        id,
                        amount: o.amount.round().max(0.0) as u16,
                    })
                })
                .collect()
        };

        Snapshot {
            tick: self.tick,
            status: self.status,
            winner: self.winner,
            acked_input: self.player(viewer).map_or(0, |p| p.acked_input),
            players: self.players.iter().flatten().map(Player::to_snapshot).collect(),
            projectiles: projectiles
                .into_iter()
                .map(|p| ProjectileSnapshot {
                    id: p.id,
                    kind: p.kind,
                    owner: p.owner,
                    pos: p.pos,
                    yaw: wrap_angle(p.yaw),
                })
                .collect(),
            cheats: self.cheats,
            ore_is_full_sync: full_ore,
            ore,
            hits,
        }
    }

    /// Called once per tick after every client has been served.
    pub fn clear_dirty_ore(&mut self) {
        self.dirty_ore.clear();
        self.fx.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orewar_shared::world::TICK_DT;

    fn two_player_game() -> Game {
        let mut g = Game::new(1234);
        g.join(1, "one").unwrap();
        g.join(2, "two").unwrap();
        g.step(TICK_DT);
        g
    }

    /// A match with a bystander, for everything about a capture that is not the
    /// end of the match.
    ///
    /// With only two players a capture clears the field and the match is over,
    /// so the lockout, the return, and the long road to three captures all need
    /// somebody else still playing to be observable at all.
    ///
    /// The third player's miner holds station at their own base rather than
    /// setting off across the map: these tests step minutes at a time, and an
    /// unattended miner wandering into somebody's sentinels is a variable
    /// none of them are about.
    fn three_player_game() -> Game {
        let mut g = two_player_game();
        g.join(3, "three").unwrap();
        g.set_miner_mode(2, MinerMode::Stop);
        g.step(TICK_DT);
        g
    }

    #[test]
    fn a_match_starts_once_two_players_connect() {
        let mut g = Game::new(1);
        assert_eq!(g.status, GameStatus::Waiting);
        g.join(1, "solo").unwrap();
        g.step(TICK_DT);
        assert_eq!(g.status, GameStatus::Waiting, "one player is not a match");
        g.join(2, "other").unwrap();
        g.step(TICK_DT);
        assert_eq!(g.status, GameStatus::Running);
    }

    #[test]
    fn players_start_in_their_own_corners() {
        let g = two_player_game();
        for id in 0..2u8 {
            let p = g.player(id).unwrap();
            let base = world::base_position(id);
            assert!(p.tank.as_ref().unwrap().mv.pos.distance(base) < 15.0);
            assert!(p.miner.as_ref().unwrap().mv.pos.distance(base) < 15.0);
        }
        let a = g.player(0).unwrap().tank.as_ref().unwrap().mv.pos;
        let b = g.player(1).unwrap().tank.as_ref().unwrap().mv.pos;
        assert!(a.distance(b) > 200.0, "opponents should start far apart");
    }

    #[test]
    fn a_reconnecting_token_resumes_the_same_player() {
        let mut g = two_player_game();
        g.player_mut(0).unwrap().credits = 9999;
        g.player_mut(0).unwrap().powerups = PowerUp::Radar.bit();
        g.disconnect(0);
        assert!(!g.player(0).unwrap().connected);
        // The vehicles stay on the field while the player is away.
        assert!(g.player(0).unwrap().miner.is_some());

        let id = g.join(1, "one").unwrap();
        assert_eq!(id, 0, "same token must resume the same slot");
        assert_eq!(g.player(0).unwrap().credits, 9999, "state must survive the drop");
        assert!(g.player(0).unwrap().connected);
    }

    #[test]
    fn a_restart_keeps_the_players_and_replaces_everything_else() {
        let mut g = two_player_game();
        g.player_mut(0).unwrap().credits = 9999;
        g.player_mut(0).unwrap().powerups = PowerUp::Radar.bit();
        g.player_mut(0).unwrap().ore_mined = 500;
        g.player_mut(0).unwrap().captures = 2;
        g.disconnect(1);
        let ore_before = g.ore.clone();
        let tick_before = g.tick;

        g.restart(999, 0);

        // Identity survives; a restart is not a reconnect.
        assert_eq!(g.player(0).unwrap().name, "one");
        assert_eq!(g.player(0).unwrap().token, 1);
        assert_eq!(g.player(1).unwrap().name, "two");
        assert!(g.player(0).unwrap().connected);
        assert!(!g.player(1).unwrap().connected, "who is here must not change either");

        // Everything earned is gone.
        let p = g.player(0).unwrap();
        assert_eq!(p.credits, STARTING_CREDITS);
        // Back to whatever a match starts you with, which is the point: a
        // restart is a fresh match, not a stripped one.
        assert_eq!(p.powerups, STARTING_POWERUPS);
        assert_eq!(p.ore_mined, 0);
        assert_eq!(p.captures, 0);
        assert!(p.tank.is_some() && p.miner.is_some());
        assert_eq!(p.input, InputFrame::default(), "a held key must not cross over");

        assert_eq!(g.seed, 999);
        assert_ne!(g.ore, ore_before, "a restart is a new map");
        assert_eq!(g.winner, None);
        // Clients drop any snapshot not newer than the last one they hold, so
        // winding the tick back would make them ignore the whole new match.
        assert_eq!(g.tick, tick_before, "the tick must keep counting");
        assert!(g.events.contains(&GameEvent::MatchReset { world_seed: 999, by: 0 }));
    }

    #[test]
    fn a_restart_with_two_players_present_starts_straight_away() {
        let mut g = two_player_game();
        assert_eq!(g.status, GameStatus::Running);
        g.restart(7, 1);
        assert_eq!(g.status, GameStatus::Running, "nobody should have to rejoin");
    }

    /// Reconnecting must not cost you the ability to drive.
    ///
    /// Input frames are numbered per connection and a restarted client counts
    /// from one again, so a server still holding the old high-water mark would
    /// reject every frame forever. The vehicle then sits there while the rest of
    /// the client -- camera, HUD, swapping vehicles -- carries on working, which
    /// makes it look like anything but an input problem.
    #[test]
    fn a_resumed_player_can_still_drive() {
        let mut g = two_player_game();
        // Open ground: this is about whether input is accepted, and a tank
        // parked against a hill would not move either way.
        g.hills.clear();
        for tick in 1..200u32 {
            g.set_input(0, InputFrame {
                tick,
                controlling: VehicleSlot::Tank,
                throttle: 1.0,
                ..Default::default()
            });
            g.step(TICK_DT);
        }
        assert!(g.player(0).unwrap().acked_input > 100, "the tick counter should have climbed");

        // The client restarts: same token, same slot, tick numbering from one.
        g.disconnect(0);
        assert_eq!(g.join(1, "one"), Ok(0), "same token must resume the same slot");

        let before = g.player(0).unwrap().tank.as_ref().unwrap().mv.pos;
        for tick in 1..60u32 {
            g.set_input(0, InputFrame {
                tick,
                controlling: VehicleSlot::Tank,
                throttle: 1.0,
                ..Default::default()
            });
            g.step(TICK_DT);
        }
        let after = g.player(0).unwrap().tank.as_ref().unwrap().mv.pos;
        // Well clear of the four units it coasts through on leftover speed when
        // the frames are being thrown away.
        assert!(
            after.distance(before) > 20.0,
            "a resumed player must be able to drive, went {before:?} -> {after:?}"
        );
    }

    #[test]
    fn a_full_server_turns_away_a_fifth_player() {
        let mut g = Game::new(1);
        for i in 0..MAX_PLAYERS as u64 {
            assert!(g.join(i + 1, &format!("p{i}")).is_ok());
        }
        assert_eq!(g.join(99, "late"), Err(DenyReason::ServerFull));
    }

    /// A name is an identity: the client hashes it into the token the server
    /// keys players by, so two of them under one name would share a slot and
    /// spend the match retiring each other's connection.
    #[test]
    fn a_name_somebody_is_already_using_is_turned_away() {
        let mut g = Game::new(1);
        assert_eq!(g.join(1, "Ash"), Ok(0));
        assert_eq!(g.join(2, "Ash"), Err(DenyReason::NameTaken));
        // The refusal must cost the newcomer nothing else: the slot they would
        // have had is still free under any other name.
        assert_eq!(g.join(2, "Bo"), Ok(1));
    }

    /// Turning away a duplicate must not turn away the player who owns the
    /// name, coming back to their own slot.
    #[test]
    fn reclaiming_your_own_name_is_not_a_duplicate() {
        let mut g = two_player_game();
        g.disconnect(0);
        assert_eq!(g.join(1, "one"), Ok(0), "your own name is yours to come back to");
        assert!(g.player(0).unwrap().connected);
    }

    /// A finished match has to say so, rather than reporting itself full.
    #[test]
    fn a_finished_match_turns_away_a_newcomer_for_the_right_reason() {
        let mut g = two_player_game();
        g.status = GameStatus::Finished;
        assert_eq!(g.join(3, "late"), Err(DenyReason::MatchFinished));
    }

    #[test]
    fn mining_moves_ore_into_cargo_then_into_credits() {
        let mut g = two_player_game();
        let deposit = g.ore[0].pos;
        {
            let m = g.player_mut(0).unwrap().miner.as_mut().unwrap();
            m.mv.pos = deposit;
            m.mv.speed = 0.0;
        }
        let before = g.ore[0].amount;
        for _ in 0..30 {
            g.step(TICK_DT);
        }
        assert!(g.ore[0].amount < before, "deposit should deplete");
        let cargo = g.player(0).unwrap().miner.as_ref().unwrap().cargo;
        assert!(cargo > 0.0, "miner should be carrying ore");

        // Teleport home and let it unload.
        g.player_mut(0).unwrap().miner.as_mut().unwrap().mv.pos = world::base_position(0);
        let credits_before = g.player(0).unwrap().credits;
        for _ in 0..60 {
            g.step(TICK_DT);
        }
        assert!(g.player(0).unwrap().credits > credits_before, "ore should convert to credits");
    }

    #[test]
    fn a_moving_miner_cannot_mine() {
        let mut g = two_player_game();
        let deposit = g.ore[0].pos;
        {
            let p = g.player_mut(0).unwrap();
            p.input.controlling = VehicleSlot::Miner;
            p.input.throttle = 1.0;
            let m = p.miner.as_mut().unwrap();
            m.mv.pos = deposit;
            m.mv.speed = sim::MINING_MAX_SPEED + 3.0;
        }
        g.step(TICK_DT);
        assert_eq!(g.player(0).unwrap().miner.as_ref().unwrap().cargo, 0.0);
    }

    #[test]
    fn purchases_are_validated_against_credits_and_duplicates() {
        let mut g = two_player_game();
        g.player_mut(0).unwrap().credits = 0;
        g.events.clear();
        g.purchase(0, PowerUp::Radar);
        assert!(matches!(
            g.events.last(),
            Some(GameEvent::PurchaseRejected { reason: RejectReason::NotEnoughCredits, .. })
        ));

        g.player_mut(0).unwrap().credits = 10_000;
        g.events.clear();
        g.purchase(0, PowerUp::Radar);
        assert!(matches!(g.events.last(), Some(GameEvent::PurchaseAccepted { .. })));
        assert!(PowerUp::Radar.held(g.player(0).unwrap().powerups));
        assert_eq!(g.player(0).unwrap().credits, 10_000 - PowerUp::Radar.cost());

        g.events.clear();
        g.purchase(0, PowerUp::Radar);
        assert!(matches!(
            g.events.last(),
            Some(GameEvent::PurchaseRejected { reason: RejectReason::AlreadyOwned, .. })
        ));
    }

    #[test]
    fn missile_packs_stack_because_they_are_consumable() {
        let mut g = two_player_game();
        g.player_mut(0).unwrap().credits = 10_000;
        let before = g.player(0).unwrap().missiles;
        g.purchase(0, PowerUp::MissilePack);
        g.purchase(0, PowerUp::MissilePack);
        assert_eq!(g.player(0).unwrap().missiles, before + MISSILES_PER_PACK * 2);
    }

    #[test]
    fn a_destroyed_tank_respawns_at_base() {
        let mut g = two_player_game();
        g.damage_vehicle(0, VehicleSlot::Tank, 100_000.0, 1, 0.0);
        assert!(g.player(0).unwrap().tank.is_none());
        assert!(!g.player(0).unwrap().eliminated, "losing a tank is not elimination");

        for _ in 0..((sim::TANK_RESPAWN_DELAY / TICK_DT) as usize + 4) {
            g.step(TICK_DT);
        }
        let tank = g.player(0).unwrap().tank.as_ref().expect("tank should return");
        assert!(tank.mv.pos.distance(world::base_position(0)) < 15.0);
    }

    #[test]
    fn a_miner_is_disabled_rather_than_destroyed() {
        let mut g = two_player_game();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);
        let m = g.player(0).unwrap().miner.as_ref().expect("miner must remain");
        assert!(m.disabled);
        assert_eq!(m.hull, 0.0);
        assert!(!g.player(0).unwrap().eliminated, "it has to be captured, not just shot");
    }

    /// Takes one miner and lets the clock run out on the victim.
    ///
    /// Returns the game with the capture done, so the tests below can each pick
    /// up the part of the aftermath they care about.
    fn capture_once(g: &mut Game, by: u8, from: u8) {
        g.damage_vehicle(from, VehicleSlot::Miner, 100_000.0, by, 0.0);
        let wreck = g.player(from).unwrap().miner.as_ref().unwrap().mv.pos;
        // Vehicles spawn within capture range of each other, so the owner's tank
        // has to be drawn away before the wreck is actually takeable.
        g.player_mut(from).unwrap().tank.as_mut().unwrap().mv.pos = Vec2::splat(128.0);

        for _ in 0..((sim::CAPTURE_TIME / TICK_DT) as usize + 4) {
            g.player_mut(by).unwrap().tank.as_mut().unwrap().mv.pos = wreck;
            g.step(TICK_DT);
        }
        assert!(g.player(from).unwrap().miner.is_none(), "the miner changed hands");
    }

    /// A capture takes a player off the field, not out of the match.
    #[test]
    fn a_captured_player_sits_out_a_minute_and_comes_back_rebuilt() {
        let mut g = three_player_game();
        g.hills.clear();
        // Something to lose: upgrades bought, ore banked, missiles spent.
        {
            let p = g.player_mut(0).unwrap();
            p.powerups = PowerUp::Turbo.bit() | PowerUp::Radar.bit();
            p.credits = 900;
            p.missiles = 0;
        }

        capture_once(&mut g, 1, 0);
        assert!(g.player(0).unwrap().eliminated, "off the field for now");
        assert!(g.player(0).unwrap().tank.is_none(), "the tank goes with the miner");
        assert_ne!(g.status, GameStatus::Finished, "one capture is not the match");

        // Still gone most of the way through the minute.
        for _ in 0..((sim::CAPTURE_LOCKOUT / TICK_DT) as usize - 30) {
            g.step(TICK_DT);
        }
        assert!(g.player(0).unwrap().eliminated, "back far too early");
        assert!(g.player(0).unwrap().tank.is_none());

        for _ in 0..90 {
            g.step(TICK_DT);
        }
        let p = g.player(0).unwrap();
        assert!(!p.eliminated, "should be back after the lockout");
        assert!(p.tank.is_some() && p.miner.is_some(), "both vehicles come back");
        assert_eq!(p.credits, 0, "the bank is gone");
        assert_eq!(p.missiles, STARTING_MISSILES, "restocked as at the start of a match");
        assert_eq!(
            p.powerups,
            PowerUp::Turbo.bit() | PowerUp::Radar.bit(),
            "upgrades are permanent -- they are the whole reason to keep playing"
        );
        assert_eq!(p.miner.as_ref().unwrap().cargo, 0.0);
        assert!(
            g.events.iter().any(|e| matches!(e, GameEvent::PlayerReturned { player: 0 })),
            "and it should be announced"
        );
    }

    /// The vehicles have to come back where they started, not where they died.
    #[test]
    fn a_returning_player_starts_from_their_own_base() {
        let mut g = three_player_game();
        g.hills.clear();
        capture_once(&mut g, 1, 0);
        // Checked the instant they return: the miner is on autopilot and
        // sets off for the nearest ore straight away, so waiting even a second
        // longer would be measuring where it drove to, not where it started.
        for _ in 0..((sim::CAPTURE_LOCKOUT / TICK_DT) as usize + 60) {
            g.step(TICK_DT);
            if !g.player(0).unwrap().eliminated {
                break;
            }
        }
        let p = g.player(0).unwrap();
        assert!(!p.eliminated, "never came back");
        let base = world::base_position(0);
        assert!(
            p.tank.as_ref().unwrap().mv.pos.distance(base) < 12.0,
            "tank came back {:.1} from base",
            p.tank.as_ref().unwrap().mv.pos.distance(base)
        );
        assert!(
            p.miner.as_ref().unwrap().mv.pos.distance(base) < 12.0,
            "miner came back {:.1} from base",
            p.miner.as_ref().unwrap().mv.pos.distance(base)
        );
    }

    /// The ending the game is about: take the last miner and it is yours.
    #[test]
    fn capturing_the_last_opponent_wins_the_match() {
        let mut g = two_player_game();
        g.hills.clear();
        capture_once(&mut g, 1, 0);

        assert_eq!(g.status, GameStatus::Finished, "nobody else is on the field");
        assert_eq!(g.winner, Some(1));
        assert!(
            g.events.contains(&GameEvent::GameOver { winner: 1 }),
            "the win has to be announced, not just recorded"
        );
        assert_eq!(
            g.player(1).unwrap().captures,
            1,
            "one capture, well short of the three that also win it"
        );
    }

    /// The lockout must not be a way to win a match you have not cleared.
    #[test]
    fn a_capture_is_not_the_match_while_somebody_else_is_standing() {
        let mut g = three_player_game();
        g.hills.clear();
        capture_once(&mut g, 1, 0);
        assert_ne!(g.status, GameStatus::Finished, "player 2 is still out there");
        assert_eq!(g.winner, None);

        // Taking the bystander's miner while the first victim is still in
        // their minute leaves one player on the field, and that ends it.
        capture_once(&mut g, 1, 2);
        assert!(g.player(0).unwrap().eliminated, "the first victim is still down");
        assert_eq!(g.status, GameStatus::Finished);
        assert_eq!(g.winner, Some(1));
    }

    /// Dropping out is not the same as being taken off the field.
    ///
    /// A player who loses their connection leaves their vehicles where they
    /// stand, and their miner can still be taken -- which is how the match
    /// is meant to be won. Handing it over the moment somebody's wifi blinks
    /// would end matches nobody had finished.
    #[test]
    fn a_disconnect_does_not_win_the_match_for_whoever_is_left() {
        let mut g = two_player_game();
        g.disconnect(0);
        for _ in 0..30 {
            g.step(TICK_DT);
        }
        assert_eq!(g.status, GameStatus::Running);
        assert_eq!(g.winner, None, "there is still a miner out there to take");
    }

    /// The other way home: a match nobody can clear the field of is still won
    /// by taking three miners.
    #[test]
    fn taking_three_miners_wins_the_match() {
        let mut g = three_player_game();
        g.hills.clear();
        for round in 1..=sim::CAPTURES_TO_WIN {
            capture_once(&mut g, 1, 0);
            assert_eq!(g.player(1).unwrap().captures, round);
            if round < sim::CAPTURES_TO_WIN {
                assert_ne!(g.status, GameStatus::Finished, "won after only {round}");
                // Let them back on the field so there is a miner to take.
                for _ in 0..((sim::CAPTURE_LOCKOUT / TICK_DT) as usize + 60) {
                    g.step(TICK_DT);
                }
                assert!(!g.player(0).unwrap().eliminated, "never came back for round {round}");
            }
        }
        assert_eq!(g.status, GameStatus::Finished);
        assert_eq!(g.winner, Some(1));
    }

    #[test]
    fn an_owner_can_rescue_their_own_disabled_miner() {
        let mut g = two_player_game();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);
        let wreck = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;

        for _ in 0..600 {
            g.player_mut(0).unwrap().tank.as_mut().unwrap().mv.pos = wreck;
            g.step(TICK_DT);
            if !g.player(0).unwrap().miner.as_ref().unwrap().disabled {
                break;
            }
        }
        assert!(!g.player(0).unwrap().miner.as_ref().unwrap().disabled, "should be repaired");
        assert!(!g.player(0).unwrap().eliminated);
    }

    /// Bringing a miner back has to be slower than taking it.
    ///
    /// Otherwise disabling one achieves nothing: the attacker has to cross the
    /// distance to the wreck *and then* hold it for `CAPTURE_TIME`, while the
    /// defender only has to drive back to it. A tank now spawns well outside
    /// `CAPTURE_RADIUS` of its own miner, so that return trip is real, but
    /// the repair itself still has to be the slower half of the exchange.
    #[test]
    fn a_rescue_takes_longer_than_a_capture() {
        let mut g = two_player_game();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);
        let wreck = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        // Owner alone on it, which is the fastest a rescue can go.
        let mut ticks = 0;
        while ticks < 2000 {
            g.player_mut(0).unwrap().tank.as_mut().unwrap().mv.pos = wreck;
            g.step(TICK_DT);
            ticks += 1;
            if !g.player(0).unwrap().miner.as_ref().unwrap().disabled {
                break;
            }
        }
        let seconds = ticks as f32 * TICK_DT;
        assert!(
            seconds > sim::CAPTURE_TIME,
            "rescue took {seconds:.1}s against a {:.1}s capture, so a wreck can never be taken",
            sim::CAPTURE_TIME
        );
    }

    /// Two hulls cannot end a tick sharing ground, and meeting at speed costs
    /// both of them.
    #[test]
    fn two_tanks_that_run_into_each_other_bounce_apart_and_both_pay() {
        let mut g = two_player_game();
        g.hills.clear();
        let touching = sim::tuning(VehicleKind::Tank).radius * 2.0;
        let top = sim::tuning(VehicleKind::Tank).max_speed;

        // Nose to nose and well inside each other, closing at twice top speed.
        {
            let a = g.player_mut(0).unwrap().tank.as_mut().unwrap();
            a.mv = MoveState { pos: Vec2::new(100.0, 100.0), yaw: 0.0, speed: top, roll: 0.0, alt: 0.0 };
        }
        {
            let b = g.player_mut(1).unwrap().tank.as_mut().unwrap();
            b.mv = MoveState {
                pos: Vec2::new(100.0 + touching * 0.5, 100.0),
                yaw: std::f32::consts::PI,
                speed: top,
                roll: 0.0,
                alt: 0.0,
            };
        }
        let full = g.player(0).unwrap().tank.as_ref().unwrap().shield;

        g.step(TICK_DT);

        let a = g.player(0).unwrap().tank.as_ref().unwrap();
        let b = g.player(1).unwrap().tank.as_ref().unwrap();
        assert!(
            a.mv.pos.distance(b.mv.pos) >= touching - 1e-3,
            "hulls ended {:.2} apart, inside the {touching:.2} they occupy",
            a.mv.pos.distance(b.mv.pos)
        );
        assert!(a.shield < full && b.shield < full, "a head-on meeting has to cost both sides");
        assert!(a.mv.speed < top && b.mv.speed < top, "and take the run out of both of them");
    }

    /// Contact at a crawl is how you park next to somebody, not an attack.
    #[test]
    fn hulls_that_barely_touch_cost_nobody_anything() {
        let mut g = two_player_game();
        g.hills.clear();
        let touching = sim::tuning(VehicleKind::Tank).radius * 2.0;

        {
            let a = g.player_mut(0).unwrap().tank.as_mut().unwrap();
            a.mv = MoveState { pos: Vec2::new(100.0, 100.0), yaw: 0.0, speed: 2.0, roll: 0.0, alt: 0.0 };
        }
        {
            let b = g.player_mut(1).unwrap().tank.as_mut().unwrap();
            b.mv = MoveState {
                pos: Vec2::new(100.0 + touching - 0.4, 100.0),
                yaw: 0.0,
                speed: 0.0,
                roll: 0.0,
                alt: 0.0,
            };
        }
        let full = g.player(0).unwrap().tank.as_ref().unwrap().shield;

        g.step(TICK_DT);

        assert_eq!(g.player(0).unwrap().tank.as_ref().unwrap().shield, full);
        assert_eq!(g.player(1).unwrap().tank.as_ref().unwrap().shield, full);
        let a = g.player(0).unwrap().tank.as_ref().unwrap().mv.pos;
        let b = g.player(1).unwrap().tank.as_ref().unwrap().mv.pos;
        assert!(a.distance(b) >= touching - 1e-3, "they still may not overlap");
    }

    /// A wreck is scenery: solid, but it neither moves nor takes any more
    /// punishment. Without this a captor could shove the miner they came for
    /// out from under themselves, or finish it off by driving at it.
    #[test]
    fn a_wreck_is_solid_but_takes_nothing_and_gives_no_ground() {
        let mut g = two_player_game();
        g.hills.clear();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);
        let wreck = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        let hull = g.player(0).unwrap().miner.as_ref().unwrap().hull;

        // Straight through the middle of it at full speed.
        {
            let t = g.player_mut(1).unwrap().tank.as_mut().unwrap();
            t.mv = MoveState {
                pos: wreck,
                yaw: 0.0,
                speed: sim::tuning(VehicleKind::Tank).max_speed,
                roll: 0.0,
                alt: 0.0,
            };
        }
        g.step(TICK_DT);

        let m = g.player(0).unwrap().miner.as_ref().unwrap();
        assert_eq!(m.mv.pos, wreck, "a wreck has no engine to be shoved with");
        assert_eq!(m.hull, hull, "and nothing left to lose");

        let touching =
            sim::tuning(VehicleKind::Tank).radius + sim::tuning(VehicleKind::Miner).radius;
        let tank = g.player(1).unwrap().tank.as_ref().unwrap().mv.pos;
        assert!(
            tank.distance(wreck) >= touching - 1e-3,
            "the tank ended {:.2} from the wreck, inside it",
            tank.distance(wreck)
        );
    }

    /// The reach has to survive hulls no longer overlapping.
    ///
    /// [`sim::CAPTURE_RADIUS`] used to be wide enough that a tank covered its
    /// own miner from where it spawned. Now that it is close to touching, it
    /// has to clear the distance two hulls are held apart at -- otherwise a tank
    /// pressed right up against a wreck would still be out of range and no
    /// capture could ever complete.
    #[test]
    fn a_tank_pressed_against_a_wreck_is_in_range_to_take_it() {
        let touching =
            sim::tuning(VehicleKind::Tank).radius + sim::tuning(VehicleKind::Miner).radius;
        assert!(
            sim::CAPTURE_RADIUS > touching,
            "hulls are held {touching} apart, so a {} reach can never be met",
            sim::CAPTURE_RADIUS
        );

        // And in play: an enemy tank driven up against a wreck, never placed on
        // top of it, still takes it.
        let mut g = two_player_game();
        g.hills.clear();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);
        let wreck = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        {
            let t = g.player_mut(1).unwrap().tank.as_mut().unwrap();
            t.mv = MoveState { pos: wreck + Vec2::new(touching, 0.0), yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
        }
        for _ in 0..((sim::CAPTURE_TIME / TICK_DT) as usize + 10) {
            g.step(TICK_DT);
        }
        assert!(g.player(0).unwrap().eliminated, "an undefended wreck at arm's length must fall");
    }

    /// Driving into a hillside is a crash, and it is charged to the driver
    /// rather than counting as anybody's kill.
    #[test]
    fn a_tank_driven_into_a_hill_at_speed_loses_hull() {
        let mut g = two_player_game();
        let start = g.player(0).unwrap().tank.as_ref().unwrap().mv.pos;
        g.hills = vec![Hill { pos: start + Vec2::new(40.0, 0.0), radius: 8.0 }];
        {
            let t = g.player_mut(0).unwrap().tank.as_mut().unwrap();
            t.mv.yaw = 0.0;
        }
        let full = g.player(0).unwrap().tank.as_ref().unwrap().shield;

        for tick in 0..60u32 {
            g.set_input(
                0,
                InputFrame {
                    tick: tick + 1,
                    controlling: VehicleSlot::Tank,
                    throttle: 1.0,
                    aim: 0.0,
                    ..Default::default()
                },
            );
            g.step(TICK_DT);
        }
        assert!(
            g.player(0).unwrap().tank.as_ref().unwrap().shield < full,
            "a full-speed run into a hillside has to cost something"
        );

        // Nosing up to the same hill at a crawl does not.
        let mut gentle = two_player_game();
        gentle.hills = vec![Hill { pos: start + Vec2::new(12.0, 0.0), radius: 8.0 }];
        {
            let t = gentle.player_mut(0).unwrap().tank.as_mut().unwrap();
            t.mv.yaw = 0.0;
        }
        for tick in 0..60u32 {
            gentle.set_input(
                0,
                InputFrame {
                    tick: tick + 1,
                    controlling: VehicleSlot::Tank,
                    throttle: 0.2,
                    aim: 0.0,
                    ..Default::default()
                },
            );
            gentle.step(TICK_DT);
        }
        assert_eq!(
            gentle.player(0).unwrap().tank.as_ref().unwrap().shield,
            full,
            "parking against a slope is not a crash"
        );
    }

    /// Losing your tank must not leave you driving nothing.
    ///
    /// The client keeps naming the vehicle it last chose, so a destroyed tank
    /// used to mean every input was applied to a hull that no longer existed --
    /// the miner sat there unattended and the player was frozen out until
    /// the respawn.
    #[test]
    fn a_destroyed_tank_hands_control_to_the_miner() {
        let mut g = two_player_game();
        g.hills.clear();
        g.damage_vehicle(0, VehicleSlot::Tank, 100_000.0, 1, 0.0);
        assert!(
            g.player(0).unwrap().tank.is_none(),
            "the tank has to be gone for this to test anything"
        );

        let start = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        for tick in 0..30u32 {
            // Still asking to drive the tank, exactly as a client would.
            g.set_input(
                0,
                InputFrame {
                    tick: tick + 1,
                    controlling: VehicleSlot::Tank,
                    throttle: 1.0,
                    ..Default::default()
                },
            );
            g.step(TICK_DT);
        }
        let moved = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos.distance(start);
        assert!(moved > 3.0, "the miner only moved {moved:.2}; the player is still frozen out");
    }

    /// Helper: give a player the aircraft and put a sortie in the air.
    fn launch_for(g: &mut Game, id: u8) {
        g.player_mut(id).unwrap().powerups |= PowerUp::Bomber.bit();
        g.launch_plane(id);
    }

    /// The aircraft is bought once and then flown on a clock.
    ///
    /// Everything about the sortie being an event rather than a vehicle you
    /// keep lives here: it cannot be called without the upgrade, only one can
    /// be up at a time, and the next one waits out the cooldown from the moment
    /// the last one ended.
    #[test]
    fn a_sortie_needs_the_upgrade_and_stands_down_between_runs() {
        let mut g = two_player_game();

        // Without the upgrade the key does nothing at all. Taken away
        // explicitly rather than assumed absent, so this keeps testing the gate
        // whatever `STARTING_POWERUPS` happens to hand out.
        g.player_mut(0).unwrap().powerups &= !PowerUp::Bomber.bit();
        g.launch_plane(0);
        assert!(g.player(0).unwrap().plane.is_none(), "a sortie flew without being bought");

        launch_for(&mut g, 0);
        assert!(g.player(0).unwrap().plane.is_some(), "the sortie never took off");

        // Asking again while one is up does not stack a second.
        g.launch_plane(0);
        assert!(g.player(0).unwrap().plane.is_some());

        // Fly it dry.
        for _ in 0..((sim::PLANE_FUEL / TICK_DT) as usize + 5) {
            g.step(TICK_DT);
        }
        let p = g.player(0).unwrap();
        assert!(p.plane.is_none(), "the sortie outlived its fuel");
        assert!(p.sortie_cooldown > 0.0, "nothing is standing between this and the next one");

        // And the next one has to wait it out.
        g.launch_plane(0);
        assert!(g.player(0).unwrap().plane.is_none(), "a second sortie skipped the cooldown");
        for _ in 0..((sim::SORTIE_COOLDOWN / TICK_DT) as usize + 5) {
            g.step(TICK_DT);
        }
        g.launch_plane(0);
        assert!(g.player(0).unwrap().plane.is_some(), "the cooldown never let go");
    }

    /// Running out of fuel puts the player back in something they still have.
    ///
    /// The slot simply stops existing underneath them, which no message
    /// announces -- so if the fallback did not run every tick, a player would
    /// be left holding controls attached to nothing until they pressed a key.
    #[test]
    fn an_aircraft_out_of_fuel_hands_the_controls_back() {
        let mut g = two_player_game();
        launch_for(&mut g, 0);
        g.set_input(
            0,
            InputFrame { tick: 1, controlling: VehicleSlot::Plane, ..Default::default() },
        );
        g.step(TICK_DT);
        assert_eq!(g.player(0).unwrap().effective_input().controlling, VehicleSlot::Plane);

        for _ in 0..((sim::PLANE_FUEL / TICK_DT) as usize + 5) {
            // Kept asking for the plane the whole way down, the way a client
            // that has not seen the snapshot yet would.
            g.set_input(
                0,
                InputFrame { tick: 2, controlling: VehicleSlot::Plane, ..Default::default() },
            );
            g.step(TICK_DT);
        }

        let p = g.player(0).unwrap();
        assert!(p.plane.is_none());
        assert_eq!(
            p.effective_input().controlling,
            VehicleSlot::Tank,
            "the player was left driving an aircraft that is not there"
        );
    }

    /// A hill is cover from a shell and nothing to a bomb.
    ///
    /// This is the whole reason the aircraft is worth a thousand ore: it
    /// reaches what terrain protects. A bomb is at altitude for its entire
    /// flight, so the swept test that stops a shell at the near face of a hill
    /// has to leave it alone.
    #[test]
    fn a_bomb_falls_past_the_hill_that_would_have_stopped_a_shell() {
        let target = Vec2::new(200.0, 200.0);
        let yaw = std::f32::consts::PI;
        // Where the aircraft has to let go for the bomb to land on the target,
        // which is the inverse of what the bombsight computes.
        let release = target - Vec2::from_angle(yaw) * (sim::PLANE_CRUISE * sim::BOMB_FALL_TIME);

        // One hill squarely between the two, so the run is over cover.
        let hill = Hill { pos: target.lerp(release, 0.5), radius: 10.0 };

        let fire = |kind: ProjectileKind, speed: f32, life: f32| {
            let mut g = two_player_game();
            g.hills = vec![hill];
            {
                let t = g.player_mut(1).unwrap().tank.as_mut().unwrap();
                t.mv = MoveState { pos: target, yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
            }
            let before = g.player(1).unwrap().tank.as_ref().unwrap().shield;
            g.projectiles.push(Projectile {
                id: 900,
                kind,
                owner: 0,
                pos: release,
                yaw,
                speed,
                life,
            });
            for _ in 0..((life / TICK_DT) as usize + 3) {
                g.step(TICK_DT);
            }
            before - g.player(1).unwrap().tank.as_ref().unwrap().shield
        };

        // A missile on exactly that line is stopped by the hill.
        let shell = fire(ProjectileKind::Missile, sim::MISSILE_MAX_SPEED, sim::MISSILE_LIFETIME);
        assert_eq!(shell, 0.0, "the hill was supposed to be cover and let {shell} through");

        // The bomb flies over it and lands on the tank regardless.
        let bomb = fire(ProjectileKind::Bomb, sim::PLANE_CRUISE, sim::BOMB_FALL_TIME);
        assert!(bomb > 0.0, "the hill ate a bomb that was 26 units above it");
    }

    /// Cheat mode opens the shop and fills the tank, for everybody at once.
    ///
    /// Match-wide is the point rather than a shortcut: a cheat that applied to
    /// whoever pressed the key would be an advantage, and the reason this
    /// exists is to be able to look at something quickly.
    #[test]
    fn cheat_mode_makes_the_list_free_and_the_fuel_endless() {
        let mut g = two_player_game();
        assert!(!g.cheats, "a match does not start in cheat mode");

        // Ordinarily a thousand ore is out of reach on the opening credits.
        g.purchase(0, PowerUp::Bomber);
        assert!(!PowerUp::Bomber.held(g.player(0).unwrap().powerups));

        g.toggle_cheats();

        // Now anyone can have anything, and it costs nothing.
        for id in [0, 1] {
            let before = g.player(id).unwrap().credits;
            g.purchase(id, PowerUp::Bomber);
            assert!(PowerUp::Bomber.held(g.player(id).unwrap().powerups), "player {id}");
            assert_eq!(g.player(id).unwrap().credits, before, "player {id} was charged");
        }

        // And a sortie flies on past the point it would have run dry.
        g.launch_plane(0);
        assert!(g.player(0).unwrap().plane.is_some());
        for _ in 0..((sim::PLANE_FUEL * 2.0 / TICK_DT) as usize) {
            g.step(TICK_DT);
            // Kept over the middle, so this measures the fuel and not the map.
            if let Some(plane) = g.player_mut(0).unwrap().plane.as_mut() {
                plane.mv.pos = Vec2::splat(world::WORLD_SIZE * 0.5);
            }
        }
        assert!(
            g.player(0).unwrap().plane.is_some(),
            "the sortie ran out of fuel with cheats on"
        );

        // Off again, and the ordinary rules come straight back.
        g.toggle_cheats();
        for _ in 0..((sim::PLANE_FUEL / TICK_DT) as usize + 5) {
            g.step(TICK_DT);
            if let Some(plane) = g.player_mut(0).unwrap().plane.as_mut() {
                plane.mv.pos = Vec2::splat(world::WORLD_SIZE * 0.5);
            }
        }
        assert!(g.player(0).unwrap().plane.is_none(), "the fuel never started burning again");
    }

    /// The throttle has to reach the aircraft through the whole input path.
    ///
    /// The flight model is tested in `sim`, which proves only that the numbers
    /// work when something hands them over. This is the hand-over: a frame that
    /// names the plane and asks for full throttle has to end up trimming the
    /// airspeed, and every step between the two is somewhere it can be dropped.
    #[test]
    fn the_throttle_reaches_the_aircraft() {
        let mut g = two_player_game();
        launch_for(&mut g, 0);

        // Ticks have to keep climbing across both runs: the server drops a
        // frame whose tick it has already acknowledged, so restarting the count
        // would quietly test nothing the second time round.
        let mut tick = 0u32;
        let mut fly = |g: &mut Game, throttle: f32| {
            for _ in 0..60 {
                tick += 1;
                g.set_input(
                    0,
                    InputFrame {
                        tick,
                        controlling: VehicleSlot::Plane,
                        throttle,
                        ..Default::default()
                    },
                );
                g.step(TICK_DT);
                // Held over the middle so it cannot fly out of the match.
                if let Some(plane) = g.player_mut(0).unwrap().plane.as_mut() {
                    plane.mv.pos = Vec2::splat(world::WORLD_SIZE * 0.5);
                }
            }
            g.player(0).unwrap().plane.as_ref().unwrap().mv.speed
        };

        let fast = fly(&mut g, 1.0);
        assert!(
            fast > sim::PLANE_CRUISE + sim::PLANE_SPEED_TRIM - 0.5,
            "full throttle only reached {fast}, against a cruise of {}",
            sim::PLANE_CRUISE
        );
        let slow = fly(&mut g, -1.0);
        assert!(
            slow < sim::PLANE_CRUISE - sim::PLANE_SPEED_TRIM + 0.5,
            "held back it still did {slow}"
        );
    }

    /// The fuel is the only thing that ends a sortie.
    ///
    /// Flying out of the match used to be the other way, and is not any more:
    /// the edge banks the aircraft back inside. At sixty seconds of fuel against
    /// a field about nineteen seconds across at cruise, the wall would otherwise
    /// have ended nearly every sortie before the fuel did -- which makes a
    /// dogfight something you lose by drifting rather than by being outflown.
    #[test]
    fn the_wall_sends_a_sortie_home_instead_of_ending_it() {
        let mut g = two_player_game();
        launch_for(&mut g, 0);
        g.toggle_cheats(); // fuel off, so only the wall could end this

        // Pointed at the nearest wall and left to fly, with no stick input.
        {
            let plane = g.player_mut(0).unwrap().plane.as_mut().unwrap();
            plane.mv = MoveState {
                pos: Vec2::new(world::WORLD_SIZE - 10.0, world::WORLD_SIZE * 0.5),
                yaw: 0.0,
                speed: sim::PLANE_CRUISE,
                roll: 0.0,
                alt: sim::PLANE_ALTITUDE,
            };
        }
        // Watched across the whole run rather than at one instant. Hands off,
        // the aircraft comes off the wall still banked -- the turn-back hands it
        // over the moment it is pointed inward, and the bank is held like any
        // other -- so it circles rather than flying away in a straight line. A
        // pilot rolls level; nobody is flying this one. What has to be true is
        // that the wall never ends the sortie and never holds on to it.
        let mut got_clear = false;
        for _ in 0..600 {
            g.step(TICK_DT);
            let p = g.player(0).unwrap();
            let plane = p.plane.as_ref().expect("the wall ended the sortie");
            if plane.mv.pos.x < world::WORLD_SIZE - sim::PLANE_EDGE_BAND - 10.0 {
                got_clear = true;
            }
        }
        assert!(got_clear, "it never got out of the band it started in");
    }

    /// A bomb takes a deposit out entirely, and only the one it landed on.
    ///
    /// This is the reason to spend a sortie on empty ground: ore is the one
    /// thing on the field that cannot be driven out of the way, and denying a
    /// deposit is worth more than the hull that was standing next to it.
    #[test]
    fn a_bomb_destroys_the_ore_it_lands_on() {
        let mut g = two_player_game();
        g.hills.clear();
        let at = Vec2::new(200.0, 200.0);
        g.ore = vec![
            OreDeposit { pos: at, amount: 900.0, capacity: 900.0 },
            OreDeposit {
                pos: at + Vec2::new(sim::BOMB_BLAST_RADIUS + 6.0, 0.0),
                amount: 900.0,
                capacity: 900.0,
            },
        ];
        g.clear_dirty_ore();

        g.projectiles.push(Projectile {
            id: 910,
            kind: ProjectileKind::Bomb,
            owner: 0,
            pos: at,
            yaw: 0.0,
            speed: 0.0,
            life: TICK_DT * 0.5,
        });
        g.step(TICK_DT);

        assert_eq!(g.ore[0].amount, 0.0, "one bomb should have emptied it outright");
        assert_eq!(g.ore[1].amount, 900.0, "a deposit clear of the blast was taken with it");
    }

    /// A blast is aimed at a place: near misses hurt, far ones do not.
    #[test]
    fn a_bomb_hurts_what_is_near_where_it_lands() {
        // Measured on a miner rather than a tank, and on shield *and* hull
        // rather than shield alone. A bomb now takes a base tank apart in one
        // hit, so a tank would stop existing part way through the experiment
        // and take the reading with it; a miner is disabled rather than
        // destroyed and stays there to be measured.
        let damage_at = |offset: f32| {
            let mut g = two_player_game();
            g.hills.clear();
            let at = Vec2::new(200.0, 200.0);
            {
                let m = g.player_mut(1).unwrap().miner.as_mut().unwrap();
                m.mv = MoveState {
                    pos: at + Vec2::new(offset, 0.0),
                    yaw: 0.0,
                    speed: 0.0,
                    roll: 0.0,
                alt: 0.0,
            };
            }
            let taken = |g: &Game| {
                let m = g.player(1).unwrap().miner.as_ref().unwrap();
                m.shield + m.hull
            };
            let before = taken(&g);
            // Dropped straight down: no travel, so it lands exactly here.
            g.projectiles.push(Projectile {
                id: 901,
                kind: ProjectileKind::Bomb,
                owner: 0,
                pos: at,
                yaw: 0.0,
                speed: 0.0,
                life: TICK_DT * 0.5,
            });
            g.step(TICK_DT);
            before - taken(&g)
        };

        let direct = damage_at(0.0);
        let near = damage_at(sim::BOMB_BLAST_RADIUS * 0.6);
        let clear = damage_at(sim::BOMB_BLAST_RADIUS + 5.0);
        assert!(direct > 0.0, "a bomb landing on a miner did nothing");
        assert!(near > 0.0 && near < direct, "the falloff is not a slope: {direct} then {near}");
        assert_eq!(clear, 0.0, "a bomb outside its own radius still did {clear}");
        // The whole point of the change: a hit is worth the sortie it took.
        assert!(
            direct >= sim::BOMB_DAMAGE - 0.5,
            "a square hit only took {direct} of {}",
            sim::BOMB_DAMAGE
        );
    }

    /// Nothing on the ground can bring the aircraft down.
    ///
    /// The fuel clock is the only limit on a sortie, and that is a deliberate
    /// choice rather than an oversight: every gun in this game fires along the
    /// ground, so a shell that took the plane down would be one the player
    /// watched pass visibly underneath it. The aircraft is kept out of the
    /// target list to make that true, and this is what says so.
    #[test]
    fn the_aircraft_is_not_something_that_can_be_shot_at() {
        let mut g = two_player_game();
        g.hills.clear();
        launch_for(&mut g, 0);
        let over = Vec2::new(200.0, 200.0);
        {
            let plane = g.player_mut(0).unwrap().plane.as_mut().unwrap();
            plane.mv = MoveState { pos: over, yaw: 0.0, speed: sim::PLANE_CRUISE, roll: 0.0, alt: sim::PLANE_ALTITUDE };
        }
        assert!(
            !g.collect_targets().iter().any(|t| t.what == Hittable::Vehicle(VehicleSlot::Plane)),
            "the aircraft is in the target list and can be shot at"
        );

        // Walk a shell straight through where it is.
        g.projectiles.push(Projectile {
            id: 902,
            kind: ProjectileKind::Missile,
            owner: 1,
            pos: over - Vec2::new(30.0, 0.0),
            yaw: 0.0,
            speed: sim::MISSILE_MAX_SPEED,
            life: sim::MISSILE_LIFETIME,
        });
        for _ in 0..20 {
            g.step(TICK_DT);
        }
        assert!(g.player(0).unwrap().plane.is_some(), "something shot the aircraft down");
    }

    /// A wreck keeps its load, and the load goes to whoever takes it.
    #[test]
    fn capturing_a_loaded_miner_seizes_its_ore() {
        let mut g = two_player_game();
        g.hills.clear();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);

        // Disabling must not empty it -- that is where the ore used to vanish.
        {
            let m = g.player_mut(0).unwrap().miner.as_mut().unwrap();
            m.cargo = 42.7;
        }
        let before = g.player(1).unwrap().credits;

        let wreck = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        let touching =
            sim::tuning(VehicleKind::Tank).radius + sim::tuning(VehicleKind::Miner).radius;
        {
            let t = g.player_mut(1).unwrap().tank.as_mut().unwrap();
            t.mv = MoveState { pos: wreck + Vec2::new(touching, 0.0), yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
        }
        for _ in 0..((sim::CAPTURE_TIME / TICK_DT) as usize + 10) {
            g.step(TICK_DT);
        }

        let captor = g.player(1).unwrap();
        assert_eq!(
            captor.credits,
            before + 42,
            "whole units only, and the fraction goes down with the hull"
        );
        assert!(
            g.events
                .iter()
                .any(|e| matches!(e, GameEvent::OreSeized { by: 1, from: 0, amount: 42 })),
            "the haul has to be announced: {:?}",
            g.events
        );
    }

    /// An empty miner should not claim a haul that was never there.
    #[test]
    fn capturing_an_empty_miner_announces_nothing() {
        let mut g = two_player_game();
        g.hills.clear();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);
        let wreck = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        let touching =
            sim::tuning(VehicleKind::Tank).radius + sim::tuning(VehicleKind::Miner).radius;
        {
            let t = g.player_mut(1).unwrap().tank.as_mut().unwrap();
            t.mv = MoveState { pos: wreck + Vec2::new(touching, 0.0), yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
        }
        for _ in 0..((sim::CAPTURE_TIME / TICK_DT) as usize + 10) {
            g.step(TICK_DT);
        }
        assert!(
            !g.events.iter().any(|e| matches!(e, GameEvent::OreSeized { .. })),
            "nothing was aboard, so nothing was seized"
        );
    }

    /// An emplacement has to work through everyone in range, not fixate.
    ///
    /// Always engaging the nearest would let a pair of attackers park one tank
    /// in front to soak every shell while the other worked untouched.
    #[test]
    fn a_sentinel_shares_its_fire_between_two_attackers() {
        let mut g = two_player_game();
        g.hills.clear();
        // Player 0's emplacement, with two of player 1's hulls sitting in front
        // of it side by side so neither is meaningfully nearer.
        let post = world::sentinel_position(0);
        let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - post).to_angle();
        let ahead = Vec2::from_angle(inward) * 30.0;
        let across = Vec2::from_angle(inward).perp() * 10.0;
        let (a, b) = (post + ahead + across, post + ahead - across);

        let mut hit_tank = 0u32;
        let mut hit_miner = 0u32;
        for _ in 0..600 {
            // Held in place; this test is about who gets shot at.
            {
                let p = g.player_mut(1).unwrap();
                p.tank.as_mut().unwrap().mv = MoveState { pos: a, yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
                p.miner.as_mut().unwrap().mv = MoveState { pos: b, yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
                p.tank.as_mut().unwrap().shield = 100.0;
                p.miner.as_mut().unwrap().shield = 100.0;
            }
            g.step(TICK_DT);
            let p = g.player(1).unwrap();
            hit_tank += (p.tank.as_ref().unwrap().shield < 100.0) as u32;
            hit_miner += (p.miner.as_ref().unwrap().shield < 100.0) as u32;
        }
        assert!(
            hit_tank > 0 && hit_miner > 0,
            "both hulls should have been engaged, got {hit_tank}/{hit_miner}"
        );
    }

    /// In range has to mean in reach, out at the edge and not just in its face.
    ///
    /// Sized from the emplacement's own range rather than from the tank's gun,
    /// which is tuned for play and has been shortened twice: sharing that
    /// number left this barely a unit clear of firing at what it could not hit.
    #[test]
    fn a_sentinel_reaches_the_far_edge_of_its_range() {
        let mut g = two_player_game();
        g.hills.clear();
        let post = world::sentinel_position(0);
        let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - post).to_angle();
        // Just inside the range it engages at, which is the shot most likely to
        // fall short.
        let edge = post + Vec2::from_angle(inward) * (sim::SENTINEL_RANGE - 2.0);

        let full = g.player(1).unwrap().tank.as_ref().unwrap().shield;
        for _ in 0..300 {
            // Held there; this test is about whether the shell arrives.
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv =
                MoveState { pos: edge, yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
            g.step(TICK_DT);
        }
        assert!(
            g.player(1).unwrap().tank.as_ref().unwrap().shield < full,
            "nothing landed at {:.0} units, inside a range of {:.0}",
            edge.distance(post),
            sim::SENTINEL_RANGE
        );
    }

    /// The same at the miner's own turret, which is where sharing the
    /// tank's shell actually bit: it engaged out to `AUTO_TURRET_RANGE` while
    /// its shell died about two units short of it.
    #[test]
    fn an_auto_turret_reaches_the_far_edge_of_its_range() {
        let mut g = two_player_game();
        g.hills.clear();
        g.player_mut(0).unwrap().powerups = PowerUp::AutoTurret.bit();
        // Out in the open, far from either corner's emplacement, so the only
        // gun that can be firing is the one under test.
        let station = Vec2::splat(world::WORLD_SIZE * 0.5);
        let victim = station + Vec2::new(sim::AUTO_TURRET_RANGE - 1.0, 0.0);

        let full = g.player(1).unwrap().tank.as_ref().unwrap().shield;
        for _ in 0..300 {
            // Both held: this is about whether the shell arrives, not about
            // where an autopilot would rather be.
            g.player_mut(0).unwrap().miner.as_mut().unwrap().mv =
                MoveState { pos: station, yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv =
                MoveState { pos: victim, yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
            g.step(TICK_DT);
        }
        assert!(
            g.player(1).unwrap().tank.as_ref().unwrap().shield < full,
            "nothing landed at {:.0} units, inside a range of {:.0}",
            victim.distance(station),
            sim::AUTO_TURRET_RANGE
        );
    }

    /// Out of range is out of the fight.
    #[test]
    fn a_sentinel_ignores_what_it_cannot_reach() {
        let mut g = two_player_game();
        g.hills.clear();
        let post = world::sentinel_position(0);
        let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - post).to_angle();
        let far = post + Vec2::from_angle(inward) * (sim::SENTINEL_RANGE + 25.0);
        {
            let p = g.player_mut(1).unwrap();
            p.tank.as_mut().unwrap().mv = MoveState { pos: far, yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
        }
        let full = g.player(1).unwrap().tank.as_ref().unwrap().shield;
        for _ in 0..300 {
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = far;
            g.step(TICK_DT);
        }
        assert_eq!(g.player(1).unwrap().tank.as_ref().unwrap().shield, full);
    }

    /// Shooting one down buys a window, and only a window.
    #[test]
    fn a_downed_sentinel_goes_quiet_and_comes_back() {
        let mut g = two_player_game();
        g.hills.clear();
        assert!(g.player(0).unwrap().sentinel.standing());

        g.damage_sentinel(0, sim::SENTINEL_HULL + 1.0);
        assert!(!g.player(0).unwrap().sentinel.standing(), "it should be rubble");
        assert!(
            g.snapshot_for(1, false).players.iter().find(|p| p.id == 0).unwrap().sentinel.is_none(),
            "rubble must not be sent as a standing gun"
        );

        // Silent while it rebuilds, even with a target sitting right there.
        let post = world::sentinel_position(0);
        let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - post).to_angle();
        let close = post + Vec2::from_angle(inward) * 20.0;
        let full = g.player(1).unwrap().tank.as_ref().unwrap().shield;
        for _ in 0..((sim::SENTINEL_REBUILD / TICK_DT) as usize - 30) {
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = close;
            g.step(TICK_DT);
        }
        assert_eq!(
            g.player(1).unwrap().tank.as_ref().unwrap().shield,
            full,
            "rubble does not shoot"
        );

        for _ in 0..90 {
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = close;
            g.step(TICK_DT);
        }
        let s = &g.player(0).unwrap().sentinel;
        assert!(s.standing() && s.hull == sim::SENTINEL_HULL, "it should be back at full hull");
        assert!(
            g.events.iter().any(|e| matches!(e, GameEvent::SentinelRebuilt { player: 0 })),
            "and say so"
        );
    }

    /// It must not shoot its own side, and its own side must not shoot it.
    #[test]
    fn a_sentinel_is_friendly_to_its_owner() {
        let mut g = two_player_game();
        g.hills.clear();
        let post = world::sentinel_position(0);
        let inward = (Vec2::splat(world::WORLD_SIZE * 0.5) - post).to_angle();
        let close = post + Vec2::from_angle(inward) * 20.0;
        let full = g.player(0).unwrap().tank.as_ref().unwrap().shield;
        for _ in 0..300 {
            g.player_mut(0).unwrap().tank.as_mut().unwrap().mv.pos = close;
            g.step(TICK_DT);
        }
        assert_eq!(
            g.player(0).unwrap().tank.as_ref().unwrap().shield,
            full,
            "an emplacement does not shoot the player it belongs to"
        );
    }

    /// The whole point of the autopilot: income without being driven.
    ///
    /// Runs the full loop -- find ore, fill up, come home, unload -- with the
    /// player notionally in their tank the entire time.
    #[test]
    fn an_auto_miner_mines_and_banks_without_being_driven() {
        let mut g = two_player_game();
        let start = g.player(0).unwrap().credits;
        assert_eq!(g.player(0).unwrap().miner_mode, MinerMode::Auto, "the default");

        // Two minutes of nobody touching it. The player sits in their tank, so
        // the miner is on its own the whole way.
        for tick in 0..3600u32 {
            g.set_input(
                0,
                InputFrame {
                    tick: tick + 1,
                    controlling: VehicleSlot::Tank,
                    ..Default::default()
                },
            );
            g.step(TICK_DT);
        }

        let p = g.player(0).unwrap();
        assert!(
            p.ore_mined > 0,
            "it never banked anything: cargo {:.1}, credits {}",
            p.miner.as_ref().unwrap().cargo,
            p.credits
        );
        assert!(p.credits > start, "credits went {start} -> {}", p.credits);
    }

    /// Stop means stop, so a player can park it somewhere deliberately.
    #[test]
    fn a_stopped_miner_stays_where_it_is() {
        let mut g = two_player_game();
        g.set_miner_mode(0, MinerMode::Stop);
        let start = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        for _ in 0..600 {
            g.step(TICK_DT);
        }
        let moved = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos.distance(start);
        assert!(moved < 1.0, "it wandered {moved:.2} with the brakes on");
    }

    /// Home means home, and arriving there is what banks the load.
    #[test]
    fn a_miner_sent_home_goes_home() {
        let mut g = two_player_game();
        g.set_miner_mode(0, MinerMode::Home);
        // Start it well out in the field so the trip is real.
        {
            let m = g.player_mut(0).unwrap().miner.as_mut().unwrap();
            m.mv = MoveState { pos: Vec2::splat(world::WORLD_SIZE * 0.4), yaw: 0.0, speed: 0.0, roll: 0.0, alt: 0.0 };
            m.cargo = 20.0;
        }
        for _ in 0..5400 {
            g.step(TICK_DT);
            if g.player(0).unwrap().credits > STARTING_CREDITS {
                break;
            }
        }
        let pos = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        assert!(
            sim::is_at_base(pos, 0),
            "ended at {pos:?}, not on its pad at {:?}",
            world::base_position(0)
        );
        assert!(g.player(0).unwrap().credits > STARTING_CREDITS, "and should have unloaded");
    }

    #[test]
    fn an_owner_tank_contests_an_enemy_capture() {
        let mut g = two_player_game();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);
        let wreck = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        for _ in 0..((sim::CAPTURE_TIME / TICK_DT) as usize * 2) {
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = wreck;
            g.player_mut(0).unwrap().tank.as_mut().unwrap().mv.pos = wreck;
            g.step(TICK_DT);
        }
        assert!(!g.player(0).unwrap().eliminated, "a defended miner must not be taken");
    }

    /// The other half of contesting: standing over your own wreck denies the
    /// capture, but it does not repair it while an enemy is there too. The
    /// defender has to clear them off first.
    #[test]
    fn a_contested_wreck_is_neither_taken_nor_repaired() {
        let mut g = two_player_game();
        g.damage_vehicle(0, VehicleSlot::Miner, 100_000.0, 1, 0.0);
        let wreck = g.player(0).unwrap().miner.as_ref().unwrap().mv.pos;
        let hull = g.player(0).unwrap().miner.as_ref().unwrap().hull;

        for _ in 0..((sim::CAPTURE_TIME / TICK_DT) as usize * 2) {
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = wreck;
            g.player_mut(0).unwrap().tank.as_mut().unwrap().mv.pos = wreck;
            g.step(TICK_DT);
        }

        let m = g.player(0).unwrap().miner.as_ref().unwrap();
        assert!(m.disabled, "a contested wreck stays a wreck");
        assert_eq!(m.hull, hull, "no free repairs with an enemy tank on top of it");
        assert_eq!(g.player(1).unwrap().captures, 0, "and it is not taken either");
    }

    #[test]
    fn bullets_damage_enemies_and_never_their_owner() {
        let mut g = two_player_game();
        // Put player 1's tank directly in front of player 0's gun.
        let shooter = g.player(0).unwrap().tank.as_ref().unwrap().mv.pos;
        g.player_mut(0).unwrap().tank.as_mut().unwrap().turret_yaw = 0.0;
        let firing = |tick: u32| InputFrame {
            tick,
            controlling: VehicleSlot::Tank,
            fire_primary: true,
            aim: 0.0,
            ..Default::default()
        };
        let victim_pos = shooter + Vec2::new(20.0, 0.0);
        g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = victim_pos;
        let own_miner_shield = g.player(0).unwrap().miner.as_ref().unwrap().shield;

        let start = g.player(1).unwrap().tank.as_ref().unwrap().shield;
        for tick in 0..30u32 {
            g.set_input(0, firing(tick + 1));
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = victim_pos;
            g.step(TICK_DT);
        }
        assert!(
            g.player(1).unwrap().tank.as_ref().unwrap().shield < start,
            "the target should have taken fire"
        );
        assert_eq!(
            g.player(0).unwrap().miner.as_ref().unwrap().shield,
            own_miner_shield,
            "a player's own vehicles must be immune to their fire"
        );
    }

    /// A duplicate of the shooting setup above with a hill dropped in between:
    /// the same shots that connect over open ground must not connect through
    /// terrain.
    /// The upgrade has to reach the gun it is bought for, and nobody else's.
    #[test]
    fn a_long_barrel_lengthens_the_shell_a_tank_fires() {
        let mut g = two_player_game();
        g.player_mut(1).unwrap().powerups = PowerUp::LongBarrel.bit();

        for id in 0..2u8 {
            g.set_input(id, InputFrame {
                tick: 1,
                controlling: VehicleSlot::Tank,
                fire_primary: true,
                ..Default::default()
            });
        }
        g.step(TICK_DT);

        let life = |owner: u8| {
            g.projectiles
                .iter()
                .find(|p| p.owner == owner && p.kind == ProjectileKind::Bullet)
                .unwrap_or_else(|| panic!("player {owner} should have a shell in the air"))
                .life
        };
        // Both have flown one tick, so the difference between them is the whole
        // of what the upgrade added.
        assert!(
            (life(1) - life(0) - sim::BULLET_LIFETIME).abs() < 0.001,
            "{} against {}",
            life(1),
            life(0)
        );
    }

    /// What the upgrade is worth in units of field, which is the thing a player
    /// actually feels.
    #[test]
    fn a_shell_falls_short_of_a_quadrant_until_the_barrel_is_bought() {
        let reach = |powerups: u16| {
            let mut g = two_player_game();
            // Nothing in the way: this is about how far a shell carries, not
            // about what it might run into on the way.
            g.hills.clear();
            let start = Vec2::splat(world::WORLD_SIZE * 0.5);
            g.projectiles.push(Projectile {
                id: 1,
                kind: ProjectileKind::Bullet,
                owner: 0,
                pos: start,
                yaw: 0.0,
                speed: sim::BULLET_SPEED,
                life: sim::bullet_lifetime(powerups),
            });
            let mut travelled = 0.0;
            for _ in 0..400 {
                let Some(shell) = g.projectiles.first() else { break };
                travelled = shell.pos.distance(start);
                g.step(TICK_DT);
            }
            assert!(g.projectiles.is_empty(), "the shell should have fallen short by now");
            travelled
        };

        let plain = reach(0);
        let upgraded = reach(PowerUp::LongBarrel.bit());
        let per_tick = sim::BULLET_SPEED * TICK_DT;

        assert!(plain < world::WORLD_SIZE * 0.25, "a plain shell carried {plain}");
        assert!(
            (upgraded - plain * 2.0).abs() < per_tick * 2.0,
            "{upgraded} should be twice {plain}"
        );
    }

    #[test]
    fn a_hill_between_two_tanks_is_cover() {
        let fire_for_a_while = |g: &mut Game| {
            let shooter = g.player(0).unwrap().tank.as_ref().unwrap().mv.pos;
            g.player_mut(0).unwrap().tank.as_mut().unwrap().turret_yaw = 0.0;
            let victim_pos = shooter + Vec2::new(30.0, 0.0);
            g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = victim_pos;
            let start = g.player(1).unwrap().tank.as_ref().unwrap().shield;
            for tick in 0..40u32 {
                g.set_input(0, InputFrame {
                    tick: tick + 1,
                    controlling: VehicleSlot::Tank,
                    fire_primary: true,
                    aim: 0.0,
                    ..Default::default()
                });
                // Held in place; this test is about the shots, not the driving.
                g.player_mut(1).unwrap().tank.as_mut().unwrap().mv.pos = victim_pos;
                g.step(TICK_DT);
            }
            start - g.player(1).unwrap().tank.as_ref().unwrap().shield
        };

        let mut open = two_player_game();
        open.hills.clear();
        assert!(fire_for_a_while(&mut open) > 0.0, "the shot must land over open ground");

        let mut blocked = two_player_game();
        let shooter = blocked.player(0).unwrap().tank.as_ref().unwrap().mv.pos;
        blocked.hills = vec![Hill { pos: shooter + Vec2::new(15.0, 0.0), radius: 6.0 }];
        assert_eq!(fire_for_a_while(&mut blocked), 0.0, "the hill should have eaten every shot");
    }

    #[test]
    fn a_shield_that_soaks_a_hit_reports_a_flash_and_a_hull_hit_does_not() {
        let mut g = two_player_game();
        let at = g.player(1).unwrap().tank.as_ref().unwrap().mv.pos;

        g.damage_vehicle(1, VehicleSlot::Tank, 10.0, 0, 1.5);
        let fx = g.snapshot_for(0, false).hits;
        assert_eq!(fx.len(), 1, "a soaked hit should flash");
        assert_eq!(fx[0].kind, HitKind::Shield);
        assert_eq!(fx[0].target(), Some((1, VehicleSlot::Tank)));
        assert!(fx[0].pos.distance(at) < 0.02, "the flash belongs on the vehicle");

        // Strip the shield, then hit the hull: nothing left to absorb anything.
        g.clear_dirty_ore();
        g.player_mut(1).unwrap().tank.as_mut().unwrap().shield = 0.0;
        g.damage_vehicle(1, VehicleSlot::Tank, 10.0, 0, 1.5);
        assert!(g.snapshot_for(0, false).hits.is_empty(), "a bare hull has no shield to flash");
    }

    #[test]
    fn a_missile_into_a_hill_reports_a_blast() {
        let mut g = two_player_game();
        g.hills = vec![Hill { pos: Vec2::new(120.0, 100.0), radius: 8.0 }];
        g.projectiles.push(Projectile {
            id: 1,
            kind: ProjectileKind::Missile,
            owner: 0,
            pos: Vec2::new(100.0, 100.0),
            yaw: 0.0,
            speed: sim::MISSILE_MAX_SPEED,
            life: sim::MISSILE_LIFETIME,
        });
        // Long enough to cross the 12 units to the near face, short enough that
        // it could not have crossed the whole hill.
        for _ in 0..8 {
            g.step(TICK_DT);
        }

        assert!(g.projectiles.is_empty(), "the hill should have stopped the missile");
        let fx = g.snapshot_for(0, false).hits;
        assert_eq!(fx.len(), 1);
        assert_eq!(fx[0].kind, HitKind::Blast);
        assert_eq!(fx[0].target(), None, "terrain is not a vehicle");
        assert!(fx[0].pos.x < 120.0, "the blast belongs at the near face, not the middle");
    }

    #[test]
    fn shields_regenerate_only_after_a_lull() {
        let mut g = two_player_game();
        g.damage_vehicle(0, VehicleSlot::Tank, 30.0, 1, 0.0);
        let hurt = g.player(0).unwrap().tank.as_ref().unwrap().shield;

        g.step(TICK_DT);
        assert_eq!(g.player(0).unwrap().tank.as_ref().unwrap().shield, hurt, "no instant regen");

        for _ in 0..((sim::SHIELD_REGEN_DELAY / TICK_DT) as usize + 30) {
            g.step(TICK_DT);
        }
        assert!(g.player(0).unwrap().tank.as_ref().unwrap().shield > hurt, "shields should recover");
    }

    /// A player who stops sending must coast to a halt, not drive on forever.
    #[test]
    fn input_expires_when_a_client_goes_quiet() {
        let mut g = two_player_game();
        g.set_input(0, InputFrame { tick: 1, throttle: 1.0, ..Default::default() });
        for _ in 0..15 {
            g.step(TICK_DT);
        }
        assert!(g.player(0).unwrap().tank.as_ref().unwrap().mv.speed > 5.0, "should be underway");

        // Now go silent. Nothing refreshes the input frame.
        for _ in 0..((INPUT_TIMEOUT / TICK_DT) as usize + 60) {
            g.step(TICK_DT);
        }
        assert!(
            g.player(0).unwrap().tank.as_ref().unwrap().mv.speed.abs() < 0.5,
            "a silent client's tank should stop, not keep driving: speed {}",
            g.player(0).unwrap().tank.as_ref().unwrap().mv.speed
        );
    }

    #[test]
    fn stale_input_frames_are_ignored() {
        let mut g = two_player_game();
        g.set_input(0, InputFrame { tick: 100, throttle: 1.0, ..Default::default() });
        g.set_input(0, InputFrame { tick: 50, throttle: -1.0, ..Default::default() });
        assert_eq!(g.player(0).unwrap().input.tick, 100);
        assert!(g.player(0).unwrap().input.throttle > 0.0, "the older frame must not win");
    }

    #[test]
    fn snapshots_stay_within_the_packet_budget_under_load() {
        use orewar_shared::bytes::Encode;
        use orewar_shared::net::MAX_UNRELIABLE;
        use orewar_shared::protocol::ServerMessage;

        let mut g = Game::new(9);
        for i in 0..MAX_PLAYERS as u64 {
            g.join(i + 1, &format!("player{i}")).unwrap();
        }
        // Everyone firing everything, for many ticks.
        for i in 0..MAX_PLAYERS as u8 {
            let p = g.player_mut(i).unwrap();
            p.credits = 100_000;
            p.missiles = 200;
        }
        for i in 0..MAX_PLAYERS as u8 {
            g.purchase(i, PowerUp::AutoTurret);
        }
        let firing = |tick: u32| InputFrame {
            tick,
            controlling: VehicleSlot::Tank,
            fire_primary: true,
            fire_secondary: true,
            ..Default::default()
        };
        for tick in 0..400u32 {
            // Input expires, so a live client keeps sending it every tick.
            for id in 0..MAX_PLAYERS as u8 {
                g.set_input(id, firing(tick + 1));
            }
            g.step(TICK_DT);
            for viewer in 0..MAX_PLAYERS as u8 {
                let snap = g.snapshot_for(viewer, tick % 30 == 0);
                let bytes = ServerMessage::Snapshot(snap).to_vec();
                assert!(
                    bytes.len() <= MAX_UNRELIABLE,
                    "snapshot grew to {} bytes at tick {tick}",
                    bytes.len()
                );
            }
        }
        assert!(!g.projectiles.is_empty(), "the test should actually have produced traffic");
    }

    #[test]
    fn the_simulation_is_reproducible() {
        let run = || {
            let mut g = Game::new(777);
            g.join(1, "a").unwrap();
            g.join(2, "b").unwrap();
            for i in 0..300 {
                for id in 0..2u8 {
                    g.set_input(
                        id,
                        InputFrame {
                            tick: i,
                            controlling: VehicleSlot::Tank,
                            throttle: 1.0,
                            steer: ((i % 11) as f32 - 5.0) / 5.0,
                            climb: 0.0,
                            aim: i as f32 * 0.05,
                            fire_primary: i % 5 == 0,
                            fire_secondary: false,
                        },
                    );
                }
                g.step(TICK_DT);
            }
            let p = g.player(0).unwrap();
            (p.tank.as_ref().unwrap().mv.pos, p.credits, g.projectiles.len())
        };
        assert_eq!(run(), run());
    }
}
