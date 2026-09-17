//! Client-side world state: snapshot buffering, interpolation, and prediction.
//!
//! Two different jobs, deliberately kept apart:
//!
//! * **Other players and projectiles** are *interpolated*. The client renders
//!   them slightly in the past ([`INTERP_DELAY`]) and slides between the two
//!   snapshots that bracket that moment, so movement stays smooth even though
//!   snapshots arrive 33 ms apart and sometimes go missing.
//! * **The vehicle you are driving** is *predicted*. Waiting a round trip to see
//!   your own steering is the difference between a game that feels responsive
//!   and one that feels broken, so the client runs the shared step function on
//!   its own input immediately, and reconciles against authority as it arrives.
//!
//! Both are then drawn on the *render* clock rather than the simulation one.
//! Prediction advances in `FixedUpdate` at [`world::TICK_HZ`], so reading its
//! state straight out would move your vehicle in 30 Hz steps however fast the
//! screen refreshes -- which looks exactly like network stutter but has nothing
//! to do with the network. [`Prediction::render_pos`] slides between the last
//! two simulation steps instead.

use std::collections::VecDeque;

use bevy::prelude::*;
use orewar_shared::math::{self, Vec2 as SimVec2};
use orewar_shared::protocol::{
    GameStatus, MinerMode, HitFx, InputFrame, PlaneSnapshot, PlayerInfo, ProjectileKind,
    Snapshot, VehicleSlot, VehicleSnapshot,
};
use orewar_shared::sim::{self, MoveState};
use orewar_shared::world::{self, Hill, MAX_PLAYERS, OreDeposit, PowerUp, TICK_DT};

/// How far behind the newest snapshot the world is drawn.
///
/// This is the buffer that absorbs jitter and the occasional dropped packet.
/// Three ticks is enough to ride out one loss without stalling, and small enough
/// that the delay is not felt.
pub const INTERP_DELAY: f64 = 0.10;

const MAX_SNAPSHOTS: usize = 32;
const MAX_INPUT_HISTORY: usize = 128;

/// Prediction error above this is corrected instantly rather than smoothed:
/// something happened that local simulation could not have known about, such as
/// a respawn or a shove, and easing into it would look worse than a cut.
const SNAP_THRESHOLD: f32 = 7.0;
/// Time for smoothed prediction error to halve.
const ERROR_HALF_LIFE: f32 = 0.08;

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderVehicle {
    pub pos: SimVec2,
    pub yaw: f32,
    /// Bank angle. Always zero on the ground; on the aircraft it is both what
    /// makes the turn and what the model is visibly rolled by.
    pub roll: f32,
    /// Height above the ground. Zero on everything but the aircraft, where it
    /// is what the model is lifted by, what the camera follows, and -- through
    /// `sim::bomb_fall_time` -- how far ahead the bombsight sits.
    pub alt: f32,
    /// Signed speed along `yaw`. Carried through for the bombsight, which is
    /// this times the fall time and is wrong the moment it uses anything else.
    pub speed: f32,
    pub turret_yaw: f32,
    pub shield: f32,
    pub hull: f32,
    pub cargo: f32,
    pub disabled: bool,
    pub capture_progress: f32,
}

impl RenderVehicle {
    fn from_snapshot(v: &VehicleSnapshot) -> Self {
        RenderVehicle {
            pos: v.pos,
            yaw: v.yaw,
            roll: 0.0,
            alt: 0.0,
            speed: v.speed,
            turret_yaw: v.turret_yaw,
            shield: v.shield,
            hull: v.hull,
            cargo: v.cargo,
            disabled: v.disabled,
            capture_progress: v.capture_progress,
        }
    }

    fn lerp(a: &VehicleSnapshot, b: &VehicleSnapshot, t: f32) -> Self {
        RenderVehicle {
            pos: a.pos.lerp(b.pos, t),
            roll: 0.0,
            alt: 0.0,
            speed: a.speed + (b.speed - a.speed) * t,
            // Angles must take the short way around, or a vehicle crossing the
            // -pi/+pi boundary spins the long way for a frame.
            yaw: math::angle_lerp(a.yaw, b.yaw, t),
            turret_yaw: math::angle_lerp(a.turret_yaw, b.turret_yaw, t),
            shield: a.shield + (b.shield - a.shield) * t,
            hull: a.hull + (b.hull - a.hull) * t,
            cargo: a.cargo + (b.cargo - a.cargo) * t,
            disabled: b.disabled,
            capture_progress: a.capture_progress + (b.capture_progress - a.capture_progress) * t,
        }
    }
}

/// The aircraft as the rest of the client wants it: a vehicle like any other.
///
/// A [`PlaneSnapshot`] carries only what changes, because four of them have to
/// fit in every packet. Everything downstream of here -- prediction, the
/// camera, the HUD, the model -- is written against a vehicle, so the cheapest
/// place to pay for that thrift is once, here. The speed is the constant it is
/// always flying at, and the turret bearing is the airframe's own: it bombs
/// what it is pointed at.
fn plane_as_vehicle(p: &PlaneSnapshot) -> VehicleSnapshot {
    VehicleSnapshot {
        pos: p.pos,
        yaw: p.yaw,
        turret_yaw: p.yaw,
        speed: p.speed,
        // Fuel rides in on `cargo`, which is the one field on a vehicle that
        // means nothing to an aircraft and is already a "how full is it".
        cargo: p.fuel,
        // Real numbers now that the aircraft can be shot down. These were a
        // token zero and one for as long as nothing could reach it, and leaving
        // them that way meant the panel and the bars read an aeroplane that was
        // always on its last point of hull.
        shield: p.shield,
        hull: p.hull,
        disabled: false,
        capture_progress: 0.0,
    }
}

/// A base emplacement as drawn. Its position is fixed by the player id, so
/// only what moves travels.
#[derive(Clone, Copy, Debug)]
pub struct RenderSentinel {
    pub hull: f32,
    pub turret_yaw: f32,
}

#[derive(Clone, Debug, Default)]
pub struct RenderPlayer {
    pub id: u8,
    pub connected: bool,
    pub eliminated: bool,
    pub credits: u32,
    pub ore_mined: u32,
    pub powerups: u16,
    pub missiles: u8,
    pub captures: u8,
    /// What the miner does when nobody is driving it.
    pub miner_mode: MinerMode,
    /// Seconds until a captured player is back on the field; zero when they
    /// are on it.
    pub respawn_in: u8,
    pub tank: Option<RenderVehicle>,
    pub miner: Option<RenderVehicle>,
    /// Absent while the emplacement is rubble.
    pub sentinel: Option<RenderSentinel>,
    /// Present only while a sortie is in the air. Its `cargo` is the fuel left,
    /// as a fraction.
    pub plane: Option<RenderVehicle>,
    /// Seconds until another sortie can be called; zero when one is ready.
    pub plane_ready_in: u8,
}

impl RenderPlayer {
    pub fn vehicle(&self, slot: VehicleSlot) -> Option<&RenderVehicle> {
        match slot {
            VehicleSlot::Tank => self.tank.as_ref(),
            VehicleSlot::Miner => self.miner.as_ref(),
            VehicleSlot::Plane => self.plane.as_ref(),
        }
    }

    /// Whether a sortie can be called right now, which is what greys the key
    /// out rather than letting the player press something that does nothing.
    pub fn sortie_ready(&self) -> bool {
        PowerUp::Bomber.held(self.powerups) && self.plane.is_none() && self.plane_ready_in == 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RenderProjectile {
    pub id: u16,
    pub kind: ProjectileKind,
    pub owner: u8,
    pub pos: SimVec2,
    pub yaw: f32,
    /// Height, for the cannon round. Everything else is on the ground.
    pub alt: f32,
}

/// Everything the render systems read. Rebuilt every frame from the snapshot
/// buffer so drawing code never has to think about the network.
#[derive(Default)]
pub struct RenderWorld {
    pub players: Vec<Option<RenderPlayer>>,
    pub projectiles: Vec<RenderProjectile>,
}

/// How far one simulation step moved the vehicle, so that a correction landing
/// part way through one can leave the interpolation where it found it.
#[derive(Clone, Copy, Default)]
struct Step {
    pos: SimVec2,
    yaw: f32,
    roll: f32,
    alt: f32,
}

/// The two pieces of aircraft state that do not fit on a `VehicleSnapshot`.
///
/// Named rather than passed as a pair of bare floats: both are angles-or-heights
/// of the same type going into the same call, and the one way to get this wrong
/// -- swapping them -- is the kind of thing a compiler cannot see.
#[derive(Clone, Copy, Debug, Default)]
pub struct Air {
    pub roll: f32,
    pub alt: f32,
}

/// Local prediction of the vehicle this client is driving.
#[derive(Clone, Default)]
pub struct Prediction {
    pub active: bool,
    pub slot: VehicleSlot,
    pub state: MoveState,
    /// Where the vehicle was one simulation step ago, so a frame landing
    /// between two steps can be drawn between the two positions rather than
    /// snapped to the newer one.
    previous: MoveState,
    /// Inputs sent but not yet confirmed, replayed after every correction.
    history: VecDeque<InputFrame>,
    /// Residual error, held separately and decayed toward zero so corrections
    /// appear as a gentle drift rather than a jump.
    offset: SimVec2,
    yaw_offset: f32,
}

impl Prediction {
    /// Position to actually draw, including the un-decayed correction.
    ///
    /// `alpha` is how far the current frame sits between the last simulation
    /// step and the next one, from `Time<Fixed>::overstep_fraction`. Without it
    /// the vehicle would hold still for a whole 33 ms and then jump, which at
    /// any refresh rate above the tick rate is visible as a constant judder.
    pub fn render_pos(&self, alpha: f32) -> SimVec2 {
        self.previous.pos.lerp(self.state.pos, alpha) + self.offset
    }

    pub fn render_yaw(&self, alpha: f32) -> f32 {
        let yaw = math::angle_lerp(self.previous.yaw, self.state.yaw, alpha);
        math::wrap_angle(yaw + self.yaw_offset)
    }

    /// Bank to draw, interpolated for the same reason the heading is: the roll
    /// changes by a fixed amount per simulation step, so drawing the current
    /// step's value at every frame steps it at 30 Hz inside otherwise smooth
    /// motion, and a wing that moves in visible increments is worse than one
    /// that does not move at all.
    pub fn render_roll(&self, alpha: f32) -> f32 {
        math::angle_lerp(self.previous.roll, self.state.roll, alpha)
    }

    /// Height to draw at, interpolated for exactly the reason the bank is.
    ///
    /// A plain lerp rather than `angle_lerp`: this is a distance, and taking the
    /// short way round a circle would be meaningless.
    pub fn render_alt(&self, alpha: f32) -> f32 {
        self.previous.alt + (self.state.alt - self.previous.alt) * alpha
    }

    /// Steps prediction forward with the input the client just sent.
    pub fn apply(&mut self, frame: InputFrame, powerups: u16, hills: &[Hill]) {
        if !self.active {
            return;
        }
        if frame.controlling != self.slot {
            // The player switched vehicles; prediction restarts from whatever
            // the next snapshot says about the new one.
            self.active = false;
            self.history.clear();
            return;
        }
        self.previous = self.state;
        // Impacts are the server's to charge for, and it does not tell the
        // client about the other vehicles at all, so the outcome is nothing this
        // side can act on.
        let _ = sim::step_vehicle(
            &mut self.state,
            frame.throttle,
            frame.steer,
            frame.climb,
            self.slot.kind(),
            powerups,
            hills,
            TICK_DT,
        );
        self.history.push_back(frame);
        while self.history.len() > MAX_INPUT_HISTORY {
            self.history.pop_front();
        }
    }

    /// Re-anchors prediction on an authoritative state.
    ///
    /// The server's snapshot reflects input up to `acked`, so everything sent
    /// since then is replayed on top of it. Without the replay the vehicle would
    /// visibly rubber-band backwards by one round trip every snapshot.
    pub fn reconcile(
        &mut self,
        authoritative: &VehicleSnapshot,
        slot: VehicleSlot,
        air: Air,
        acked: u32,
        powerups: u16,
        hills: &[Hill],
    ) {
        let previous = self.state;
        // How far the step now in progress had already carried the vehicle.
        // Needed to keep the interpolation in phase across the correction.
        let step = Step {
            pos: previous.pos - self.previous.pos,
            yaw: math::angle_delta(self.previous.yaw, previous.yaw),
            roll: previous.roll - self.previous.roll,
            alt: previous.alt - self.previous.alt,
        };
        let was_active = self.active && self.slot == slot;

        self.slot = slot;
        self.state = MoveState {
            pos: authoritative.pos,
            yaw: authoritative.yaw,
            speed: authoritative.speed,
            roll: air.roll,
            alt: air.alt,
        };

        while self.history.front().is_some_and(|f| f.tick <= acked) {
            self.history.pop_front();
        }
        let replay: Vec<InputFrame> =
            self.history.iter().copied().filter(|f| f.controlling == slot).collect();
        // The state one step short of the end, which is what the frames between
        // now and the next simulation step have to be drawn sliding out of.
        let mut behind = None;
        for frame in replay {
            behind = Some(self.state);
            let _ = sim::step_vehicle(
                &mut self.state,
                frame.throttle,
                frame.steer,
                frame.climb,
                slot.kind(),
                powerups,
                hills,
                TICK_DT,
            );
        }

        // A correction is not a simulation step, and it used to set both ends of
        // the interpolation to the corrected state on the grounds that there was
        // nothing to slide between. There is: the *step* the correction landed
        // inside is still in progress, and collapsing it draws the vehicle at
        // the end of that step instead of part way through it. Snapshots arrive
        // on their own clock, unaligned with the simulation's, so that happened
        // about thirty times a second and read as a few pixels of jitter on
        // everything the local player drives.
        //
        // Replaying gives the honest answer -- the last replayed step is a real
        // pair of positions in the corrected frame. With nothing to replay there
        // is no such pair, so the phase of the step already under way is carried
        // over instead.
        self.previous = match behind {
            Some(state) => state,
            None => MoveState {
                pos: self.state.pos - step.pos,
                yaw: math::wrap_angle(self.state.yaw - step.yaw),
                speed: self.state.speed,
                roll: self.state.roll - step.roll,
                alt: self.state.alt - step.alt,
            },
        };

        if was_active {
            // Fold the difference into the visual offset instead of moving the
            // vehicle, then let it decay away.
            let error = previous.pos - self.state.pos;
            let yaw_error = math::angle_delta(self.state.yaw, previous.yaw);
            if error.length() > SNAP_THRESHOLD {
                self.offset = SimVec2::ZERO;
                self.yaw_offset = 0.0;
            } else {
                self.offset = error;
                self.yaw_offset = yaw_error;
            }
        } else {
            self.offset = SimVec2::ZERO;
            self.yaw_offset = 0.0;
        }
        self.active = true;
    }

    fn decay(&mut self, dt: f32) {
        self.offset = math::smooth_damp_vec2(self.offset, SimVec2::ZERO, ERROR_HALF_LIFE, dt);
        self.yaw_offset = math::smooth_damp(self.yaw_offset, 0.0, ERROR_HALF_LIFE, dt);
    }
}

/// The client's whole view of the match.
#[derive(Resource)]
pub struct GameState {
    pub local_player: Option<u8>,
    pub world_seed: Option<u64>,
    pub ore: Vec<OreDeposit>,
    /// Impassable ground, regenerated from the seed exactly as the server
    /// does. Prediction needs it or every hill becomes a rubber-band.
    pub hills: Vec<Hill>,
    pub roster: Vec<PlayerInfo>,
    pub status: GameStatus,
    pub winner: Option<u8>,
    /// Cheat mode, as the server reports it. Any player can toggle it and it
    /// lands on everybody, so this is read from the snapshot rather than
    /// remembered from having asked.
    cheats: bool,
    /// Newest last, paired with the client time each arrived.
    snapshots: VecDeque<(f64, Snapshot)>,
    pub render: RenderWorld,
    pub prediction: Prediction,
    /// Impacts waiting to be drawn, each paired with the render time it
    /// belongs to. They are held rather than drawn on arrival because the
    /// world itself is drawn [`INTERP_DELAY`] behind: showing a hit the
    /// moment its snapshot lands puts the flash ahead of the shot that
    /// caused it, which reads as the bullet exploding before it arrives.
    pending_fx: Vec<(f64, HitFx)>,
    /// Recent events, newest last, for the on-screen log.
    pub log: VecDeque<String>,
    /// Server tick of the newest snapshot, for the HUD.
    pub server_tick: u32,
}

impl Default for GameState {
    fn default() -> Self {
        GameState {
            local_player: None,
            world_seed: None,
            ore: Vec::new(),
            hills: Vec::new(),
            roster: Vec::new(),
            status: GameStatus::Waiting,
            winner: None,
            cheats: false,
            snapshots: VecDeque::new(),
            render: RenderWorld { players: vec![None; MAX_PLAYERS], projectiles: Vec::new() },
            prediction: Prediction::default(),
            pending_fx: Vec::new(),
            log: VecDeque::new(),
            server_tick: 0,
        }
    }
}

impl GameState {
    /// Builds the ore field from the seed the server sent.
    pub fn adopt_world(&mut self, seed: u64) {
        if self.world_seed == Some(seed) {
            return;
        }
        self.world_seed = Some(seed);
        // Both sides run the same generator, so the map never travels.
        self.ore = world::generate_ore(seed);
        self.hills = world::generate_hills(seed);
    }

    /// Switches to the map of a match that has just been restarted.
    ///
    /// The buffered snapshots describe the old match, where the vehicles were
    /// somewhere out on the field. Interpolating from those into the new ones
    /// would slide every vehicle across the map to its pad, so the buffer is
    /// dropped and rendering resumes from the first snapshot of the new match.
    pub fn begin_new_match(&mut self, seed: u64) {
        self.adopt_world(seed);
        self.snapshots.clear();
        self.pending_fx.clear();
        self.prediction.active = false;
        self.winner = None;
    }

    /// Hands over the impacts whose moment has come, keeping the rest.
    ///
    /// Effects wait out the same delay the world is drawn behind, so a
    /// shield flash lands on the frame the shot is seen to arrive rather
    /// than a tenth of a second early.
    pub fn take_due_fx(&mut self, now: f64) -> Vec<HitFx> {
        let (due, waiting): (Vec<_>, Vec<_>) =
            self.pending_fx.drain(..).partition(|(at, _)| *at <= now);
        self.pending_fx = waiting;
        due.into_iter().map(|(_, fx)| fx).collect()
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.log.push_back(message.into());
        while self.log.len() > 6 {
            self.log.pop_front();
        }
    }

    pub fn local(&self) -> Option<&RenderPlayer> {
        let id = self.local_player?;
        self.render.players.get(id as usize)?.as_ref()
    }

    pub fn name_of(&self, id: u8) -> String {
        self.roster
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| world::PLAYER_COLOR_NAMES[id as usize % MAX_PLAYERS].to_string())
    }

    /// Accepts a snapshot and updates everything derived from it.
    /// Whether the match is in cheat mode. Taken from the server rather than
    /// from whether this client asked for it, since any player can toggle it.
    pub fn cheats(&self) -> bool {
        self.cheats
    }

    pub fn push_snapshot(&mut self, now: f64, snapshot: Snapshot) {
        // Snapshots ride an unreliable channel and can arrive out of order.
        // An older one has nothing to add.
        if let Some((_, newest)) = self.snapshots.back() {
            if snapshot.tick <= newest.tick {
                return;
            }
        }

        self.status = snapshot.status;
        self.winner = snapshot.winner;
        if snapshot.cheats != self.cheats {
            self.cheats = snapshot.cheats;
            // Worth saying out loud: it is match-wide, so somebody else may
            // have been the one who turned it on.
            self.note(if self.cheats { "Cheat mode ON" } else { "Cheat mode off" });
        }
        self.server_tick = snapshot.tick;
        // Stale snapshots were rejected above, so each tick's impacts are
        // picked up exactly once. `now` is when this snapshot arrived, and
        // `interpolate` reaches it `INTERP_DELAY` later.
        self.pending_fx.extend(snapshot.hits.iter().map(|fx| (now + INTERP_DELAY, *fx)));

        // Apply ore amounts. A full sync replaces everything; otherwise only the
        // deposits the server says changed.
        for update in &snapshot.ore {
            if let Some(deposit) = self.ore.get_mut(update.id as usize) {
                deposit.amount = update.amount as f32;
            }
        }

        // Re-anchor prediction on this snapshot before it is buffered.
        if let Some(local) = self.local_player {
            if let Some(me) = snapshot.players.iter().find(|p| p.id == local) {
                let slot = self.prediction.slot;
                // The aircraft is widened into a full vehicle here so that
                // prediction has one shape to reconcile against.
                let as_plane = me.plane.as_ref().map(plane_as_vehicle);
                let vehicle = match slot {
                    VehicleSlot::Tank => me.tank.as_ref(),
                    VehicleSlot::Miner => me.miner.as_ref(),
                    VehicleSlot::Plane => as_plane.as_ref(),
                };
                // The bank and the height are not on a `VehicleSnapshot` --
                // only an aircraft has either -- so they are handed over beside
                // it rather than by widening a struct that four of ride in
                // every packet.
                let air = me
                    .plane
                    .map_or(Air::default(), |q| Air { roll: q.roll, alt: q.alt });
                match vehicle {
                    Some(v) => self.prediction.reconcile(
                        v,
                        slot,
                        air,
                        snapshot.acked_input,
                        me.powerups,
                        &self.hills,
                    ),
                    // The vehicle is gone (destroyed, or captured); nothing to
                    // predict until it comes back.
                    None => self.prediction.active = false,
                }
            }
        }

        self.snapshots.push_back((now, snapshot));
        while self.snapshots.len() > MAX_SNAPSHOTS {
            self.snapshots.pop_front();
        }
    }

    /// Tells prediction which vehicle to follow when the player switches.
    pub fn set_predicted_slot(&mut self, slot: VehicleSlot) {
        if self.prediction.slot != slot {
            self.prediction.slot = slot;
            self.prediction.active = false;
        }
    }

    /// Rebuilds [`RenderWorld`] for the current frame.
    ///
    /// `alpha` is how far this frame sits between the last simulation step and
    /// the next; see [`Prediction::render_pos`].
    pub fn interpolate(&mut self, now: f64, dt: f32, alpha: f32) {
        self.prediction.decay(dt);

        let target = now - INTERP_DELAY;
        let mut players: Vec<Option<RenderPlayer>> = vec![None; MAX_PLAYERS];
        let mut projectiles = Vec::new();

        // Find the pair of snapshots that straddle the render time.
        let mut pair = None;
        for i in 0..self.snapshots.len().saturating_sub(1) {
            let (t0, _) = &self.snapshots[i];
            let (t1, _) = &self.snapshots[i + 1];
            if *t0 <= target && target <= *t1 {
                pair = Some(i);
                break;
            }
        }

        match pair {
            Some(i) => {
                let (t0, a) = &self.snapshots[i];
                let (t1, b) = &self.snapshots[i + 1];
                let span = (t1 - t0).max(1e-6);
                let t = ((target - t0) / span).clamp(0.0, 1.0) as f32;

                for pb in &b.players {
                    let pa = a.players.iter().find(|p| p.id == pb.id);
                    let tank = match (pa.and_then(|p| p.tank), pb.tank) {
                        (Some(va), Some(vb)) => Some(RenderVehicle::lerp(&va, &vb, t)),
                        (_, Some(vb)) => Some(RenderVehicle::from_snapshot(&vb)),
                        _ => None,
                    };
                    let miner = match (pa.and_then(|p| p.miner), pb.miner) {
                        (Some(va), Some(vb)) => Some(RenderVehicle::lerp(&va, &vb, t)),
                        (_, Some(vb)) => Some(RenderVehicle::from_snapshot(&vb)),
                        _ => None,
                    };
                    // The gun slews, so its heading is worth interpolating for
                    // the same reason a hull's is.
                    let sentinel = match (pa.and_then(|p| p.sentinel), pb.sentinel) {
                        (Some(sa), Some(sb)) => Some(RenderSentinel {
                            hull: sb.hull,
                            turret_yaw: math::angle_lerp(sa.turret_yaw, sb.turret_yaw, t),
                        }),
                        (_, Some(sb)) => {
                            Some(RenderSentinel { hull: sb.hull, turret_yaw: sb.turret_yaw })
                        }
                        _ => None,
                    };
                    // Interpolated like the rest. A sortie that has just taken
                    // off has no earlier frame to come from, so it starts where
                    // the newer snapshot puts it.
                    let plane = match (pa.and_then(|p| p.plane), pb.plane) {
                        (Some(qa), Some(qb)) => Some(RenderVehicle {
                            roll: math::angle_lerp(qa.roll, qb.roll, t),
                            // A height, so a plain lerp. Left out, an enemy
                            // aircraft would be drawn on the ground.
                            alt: qa.alt + (qb.alt - qa.alt) * t,
                            ..RenderVehicle::lerp(
                                &plane_as_vehicle(&qa),
                                &plane_as_vehicle(&qb),
                                t,
                            )
                        }),
                        (_, Some(qb)) => Some(RenderVehicle {
                            roll: qb.roll,
                            alt: qb.alt,
                            ..RenderVehicle::from_snapshot(&plane_as_vehicle(&qb))
                        }),
                        _ => None,
                    };
                    if let Some(slot) = players.get_mut(pb.id as usize) {
                        *slot = Some(RenderPlayer {
                            id: pb.id,
                            connected: pb.connected,
                            eliminated: pb.eliminated,
                            credits: pb.credits,
                            ore_mined: pb.ore_mined,
                            powerups: pb.powerups,
                            missiles: pb.missiles,
                            captures: pb.captures,
                            miner_mode: pb.miner_mode,
                            respawn_in: pb.respawn_in,
                            tank,
                            miner,
                            sentinel,
                            plane,
                            plane_ready_in: pb.plane_ready_in,
                        });
                    }
                }

                for pb in &b.projectiles {
                    // Heading as well as position: a missile turns as it flies,
                    // and taking the newer yaw straight made it snap between
                    // snapshots while its body slid smoothly.
                    let (pos, yaw) = match a.projectiles.iter().find(|p| p.id == pb.id) {
                        Some(pa) => (pa.pos.lerp(pb.pos, t), math::angle_lerp(pa.yaw, pb.yaw, t)),
                        // Newly fired: no earlier state to come from.
                        None => (pb.pos, pb.yaw),
                    };
                    projectiles.push(RenderProjectile {
                        id: pb.id,
                        kind: pb.kind,
                        owner: pb.owner,
                        pos,
                        yaw,
                        // Held rather than interpolated: a cannon round flies
                        // level for its whole life, so the two snapshots either
                        // side of this frame carry the same height.
                        alt: pb.alt,
                    });
                }
            }
            // Not enough history yet, or we have fallen behind the buffer:
            // show the newest thing we have rather than nothing.
            None => {
                if let Some((_, latest)) = self.snapshots.back() {
                    for p in &latest.players {
                        if let Some(slot) = players.get_mut(p.id as usize) {
                            *slot = Some(RenderPlayer {
                                id: p.id,
                                connected: p.connected,
                                eliminated: p.eliminated,
                                credits: p.credits,
                                ore_mined: p.ore_mined,
                                powerups: p.powerups,
                                missiles: p.missiles,
                                captures: p.captures,
                                miner_mode: p.miner_mode,
                                respawn_in: p.respawn_in,
                                tank: p.tank.as_ref().map(RenderVehicle::from_snapshot),
                                miner: p.miner.as_ref().map(RenderVehicle::from_snapshot),
                                sentinel: p.sentinel.map(|s| RenderSentinel {
                                    hull: s.hull,
                                    turret_yaw: s.turret_yaw,
                                }),
                                plane: p.plane.as_ref().map(|q| RenderVehicle {
                                    roll: q.roll,
                                    ..RenderVehicle::from_snapshot(&plane_as_vehicle(q))
                                }),
                                plane_ready_in: p.plane_ready_in,
                            });
                        }
                    }
                    projectiles.extend(latest.projectiles.iter().map(|p| RenderProjectile {
                        id: p.id,
                        kind: p.kind,
                        owner: p.owner,
                        pos: p.pos,
                        yaw: p.yaw,
                        alt: p.alt,
                    }));
                }
            }
        }

        // The vehicle under local control comes from prediction, not from the
        // interpolated (and therefore stale) snapshot stream.
        if let (Some(local), true) = (self.local_player, self.prediction.active) {
            let slot = self.prediction.slot;
            let pos = self.prediction.render_pos(alpha);
            let yaw = self.prediction.render_yaw(alpha);
            let roll = self.prediction.render_roll(alpha);
            let alt = self.prediction.render_alt(alpha);
            let speed = self.prediction.state.speed;
            if let Some(Some(player)) = players.get_mut(local as usize) {
                let target = match slot {
                    VehicleSlot::Tank => player.tank.as_mut(),
                    VehicleSlot::Miner => player.miner.as_mut(),
                    VehicleSlot::Plane => player.plane.as_mut(),
                };
                if let Some(v) = target {
                    v.pos = pos;
                    v.yaw = yaw;
                    if slot == VehicleSlot::Plane {
                        v.roll = roll;
                        v.alt = alt;
                        // The sight is drawn from these two, so they have to be
                        // what prediction is flying rather than the last
                        // snapshot: at the ceiling the bomb is thrown twice as
                        // far as it is off the floor, and a sight lagging a
                        // snapshot behind through a climb would visibly chase
                        // the aircraft.
                        v.speed = speed;
                    }
                }
            }
        }

        self.render = RenderWorld { players, projectiles };
    }
}

/// Rebuilds the render view each frame.
pub fn interpolate_system(
    mut state: ResMut<GameState>,
    time: Res<Time>,
    fixed: Res<Time<Fixed>>,
) {
    let now = time.elapsed_secs_f64();
    let dt = time.delta_secs();
    // How far this frame has run past the last fixed step. Prediction only
    // advances on those steps, so this is what turns 30 Hz motion into motion at
    // the refresh rate.
    state.interpolate(now, dt, fixed.overstep_fraction());
}

#[cfg(test)]
mod tests {
    use super::*;
    use orewar_shared::protocol::PlayerSnapshot;

    fn vehicle_at(x: f32, yaw: f32) -> VehicleSnapshot {
        VehicleSnapshot { pos: SimVec2::new(x, 0.0), yaw, ..Default::default() }
    }

    fn snapshot(tick: u32, x: f32) -> Snapshot {
        Snapshot {
            tick,
            status: GameStatus::Running,
            players: vec![PlayerSnapshot {
                id: 0,
                connected: true,
                tank: Some(vehicle_at(x, 0.0)),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn positions_are_interpolated_between_snapshots() {
        let mut s = GameState::default();
        s.push_snapshot(0.0, snapshot(1, 0.0));
        s.push_snapshot(1.0, snapshot(2, 10.0));
        // Render time lands exactly halfway between the two.
        s.interpolate(0.5 + INTERP_DELAY, 0.016, 1.0);
        let x = s.render.players[0].as_ref().unwrap().tank.unwrap().pos.x;
        assert!((x - 5.0).abs() < 1e-3, "expected the midpoint, got {x}");
    }

    #[test]
    fn out_of_order_snapshots_are_discarded() {
        let mut s = GameState::default();
        s.push_snapshot(0.0, snapshot(10, 100.0));
        s.push_snapshot(0.1, snapshot(4, 0.0));
        assert_eq!(s.server_tick, 10, "an older snapshot must not overwrite a newer one");
    }

    #[test]
    fn the_newest_snapshot_is_shown_before_the_buffer_fills() {
        let mut s = GameState::default();
        s.push_snapshot(0.0, snapshot(1, 42.0));
        s.interpolate(0.0, 0.016, 1.0);
        assert_eq!(s.render.players[0].as_ref().unwrap().tank.unwrap().pos.x, 42.0);
    }

    /// A frame between two simulation steps has to be drawn between the two
    /// positions.
    ///
    /// Prediction advances 30 times a second; the screen draws far more often
    /// than that. Reading the stepped state straight held the vehicle still and
    /// then jumped it, which reads as network stutter even in a local game.
    #[test]
    fn a_frame_between_two_steps_is_drawn_between_two_positions() {
        let mut p = Prediction { active: true, ..Default::default() };
        let frame = InputFrame {
            tick: 1,
            controlling: VehicleSlot::Tank,
            throttle: 1.0,
            ..Default::default()
        };
        // Two steps, so there is a real gap to slide across.
        p.apply(frame, 0, &[]);
        p.apply(InputFrame { tick: 2, ..frame }, 0, &[]);

        let (start, end) = (p.previous.pos, p.state.pos);
        assert!(end.x > start.x, "the tank has to have moved for this to mean anything");

        assert!((p.render_pos(0.0) - start).length() < 1e-4, "alpha 0 draws the older step");
        assert!((p.render_pos(1.0) - end).length() < 1e-4, "alpha 1 draws the newer step");

        let mid = p.render_pos(0.5);
        let expected = start.lerp(end, 0.5);
        assert!(
            (mid - expected).length() < 1e-4,
            "a half-step frame drew {mid:?}, not {expected:?}"
        );
    }

    /// Prediction must land where authority does once inputs are replayed.
    ///
    /// Run against a hill on purpose. Terrain is the case where the two could
    /// disagree -- if either side resolved it differently, or one of them did
    /// not have it at all, the replay would land somewhere else and the vehicle
    /// would rubber-band every time it touched a slope.
    /// A snapshot landing between two simulation steps must not move anything.
    ///
    /// Prediction advances at 30 Hz and the picture is drawn at the refresh
    /// rate, so what is on screen between steps is an interpolation of the last
    /// two. Snapshots arrive on the server's clock, which is not aligned with
    /// either, so a correction routinely lands part way through a step -- and if
    /// it collapses that interpolation, the vehicle is redrawn at the *end* of
    /// the step it is only half way through and then waits there. That is a jump
    /// forward of most of a tick's travel, about thirty times a second, on
    /// everything the local player drives. It reads as jitter.
    ///
    /// Here the server agrees exactly with the client, so there is no error to
    /// correct and nothing on screen has any business moving.
    #[test]
    fn a_correction_between_two_steps_does_not_jump_the_picture() {
        let hills: [Hill; 0] = [];
        let mut p = Prediction { active: true, slot: VehicleSlot::Tank, ..Default::default() };

        let drive = |tick| InputFrame {
            tick,
            controlling: VehicleSlot::Tank,
            throttle: 1.0,
            steer: 0.4,
            ..Default::default()
        };
        for tick in 1..=10u32 {
            p.apply(drive(tick), 0, &hills);
        }

        // Where the server had it at tick 4, arrived at the same way.
        let mut authority = MoveState::default();
        for tick in 1..=4u32 {
            let _ = sim::step_vehicle(
                &mut authority,
                drive(tick).throttle,
                drive(tick).steer,
                0.0,
                VehicleSlot::Tank.kind(),
                0,
                &hills,
                TICK_DT,
            );
        }

        // Sampled part way through the step that is currently in progress.
        for alpha in [0.0, 0.25, 0.5, 0.75] {
            let mut p = p.clone();
            let before = p.render_pos(alpha);
            let before_yaw = p.render_yaw(alpha);
            p.reconcile(
                &VehicleSnapshot {
                    pos: authority.pos,
                    yaw: authority.yaw,
                    speed: authority.speed,
                    ..Default::default()
                },
                VehicleSlot::Tank,
                Air::default(),
                4,
                0,
                &hills,
            );
            let after = p.render_pos(alpha);
            assert!(
                before.distance(after) < 1e-3,
                "a correction at alpha {alpha} moved the vehicle {:.4} units, \
                 from {before:?} to {after:?}",
                before.distance(after)
            );
            assert!(
                math::angle_delta(before_yaw, p.render_yaw(alpha)).abs() < 1e-3,
                "a correction at alpha {alpha} turned the vehicle"
            );
        }
    }

    #[test]
    fn replayed_inputs_reproduce_the_server_state() {
        let hills = [Hill { pos: SimVec2::new(9.0, 0.0), radius: 4.0 }];
        let mut p = Prediction { active: true, slot: VehicleSlot::Tank, ..Default::default() };

        // Client drives forward for 10 ticks, none of it acknowledged yet.
        for tick in 1..=10u32 {
            p.apply(
                InputFrame {
                    tick,
                    controlling: VehicleSlot::Tank,
                    throttle: 1.0,
                    ..Default::default()
                },
                0,
                &hills,
            );
        }
        let predicted = p.state.pos;
        assert!(predicted.x < 9.0, "the hill should have stopped it short: {predicted:?}");

        // The server confirms tick 4 and reports where it had the tank then.
        let mut authority = MoveState::default();
        for _ in 1..=4 {
            let _ = sim::step_vehicle(
                &mut authority,
                1.0,
                0.0,
                0.0,
                VehicleSlot::Tank.kind(),
                0,
                &hills,
                TICK_DT,
            );
        }
        p.reconcile(
            &VehicleSnapshot {
                pos: authority.pos,
                yaw: authority.yaw,
                speed: authority.speed,
                ..Default::default()
            },
            VehicleSlot::Tank,
            Air::default(),
            4,
            0,
            &hills,
        );

        // Ticks 5..10 replay on top, so we end up exactly where we were.
        assert!(
            p.state.pos.distance(predicted) < 1e-4,
            "replay landed at {:?}, expected {:?}",
            p.state.pos,
            predicted
        );
    }

    /// An impact must not be drawn before the world reaches the tick it
    /// happened in.
    ///
    /// The world is rendered `INTERP_DELAY` behind the newest snapshot, so an
    /// effect drawn the moment its snapshot lands is that much early -- the hit
    /// flash appears while the shell still has visible distance to cover.
    #[test]
    fn an_impact_waits_for_the_world_to_catch_up_to_it() {
        use orewar_shared::protocol::{HitFx, HitKind};

        let mut s = GameState::default();
        let arrived = 4.0;
        s.push_snapshot(arrived, Snapshot {
            tick: 1,
            hits: vec![HitFx::on_terrain(HitKind::Blast, SimVec2::new(10.0, 10.0))],
            ..Default::default()
        });

        assert!(s.take_due_fx(arrived).is_empty(), "drawn on arrival, before its tick is shown");
        assert!(
            s.take_due_fx(arrived + INTERP_DELAY - 0.01).is_empty(),
            "still early"
        );
        assert_eq!(s.take_due_fx(arrived + INTERP_DELAY).len(), 1, "due once the world arrives");
        assert!(s.take_due_fx(arrived + 10.0).is_empty(), "and handed over only once");
    }

    #[test]
    fn a_large_correction_snaps_instead_of_sliding() {
        let mut p = Prediction { active: true, slot: VehicleSlot::Tank, ..Default::default() };
        p.apply(
            InputFrame { tick: 1, controlling: VehicleSlot::Tank, throttle: 1.0, ..Default::default() },
            0,
            &[],
        );
        // Authority reports somewhere far away, e.g. after a respawn.
        p.reconcile(
            &VehicleSnapshot { pos: SimVec2::new(200.0, 200.0), ..Default::default() },
            VehicleSlot::Tank,
            Air::default(),
            1,
            0,
            &[],
        );
        assert!(
            p.render_pos(1.0).distance(SimVec2::new(200.0, 200.0)) < 1e-3,
            "a large error should be taken immediately, not eased into"
        );
    }

    #[test]
    fn switching_vehicles_restarts_prediction() {
        let mut p = Prediction { active: true, slot: VehicleSlot::Tank, ..Default::default() };
        p.apply(
            InputFrame {
                tick: 1,
                controlling: VehicleSlot::Miner,
                throttle: 1.0,
                ..Default::default()
            },
            0,
            &[],
        );
        assert!(!p.active, "prediction for the old vehicle must not keep running");
    }

    /// Prediction has to fly the yoke, not just the stick.
    ///
    /// This is the altitude version of the correction jitter, and it was real:
    /// the forward step was handed a constant zero for the climb while the
    /// replay inside `reconcile` was handed the frame's own. So locally the
    /// aircraft never changed height, every snapshot snapped it to the altitude
    /// the server had actually reached, and the replay put it back -- thirty
    /// times a second, visible as jitter that appeared only while climbing or
    /// diving and never in level flight.
    ///
    /// The yoke is the only control with this shape. Throttle and steering were
    /// wired from the start, so nothing else in the suite would have noticed.
    #[test]
    fn prediction_climbs_with_the_yoke() {
        let mut p = Prediction {
            active: true,
            slot: VehicleSlot::Plane,
            state: MoveState {
                pos: SimVec2::new(240.0, 240.0),
                yaw: 0.0,
                speed: sim::PLANE_CRUISE,
                roll: 0.0,
                alt: sim::PLANE_ALTITUDE,
            },
            ..Default::default()
        };

        let yoke = |climb: f32, tick: u32| InputFrame {
            tick,
            controlling: VehicleSlot::Plane,
            climb,
            ..Default::default()
        };

        for tick in 1..=10 {
            p.apply(yoke(1.0, tick), 0, &[]);
        }
        let climbed = p.state.alt;
        assert!(
            climbed > sim::PLANE_ALTITUDE + 1.0,
            "pulling back did not climb prediction: {climbed}"
        );

        for tick in 11..=30 {
            p.apply(yoke(-1.0, tick), 0, &[]);
        }
        assert!(p.state.alt < climbed, "pushing did not descend prediction");

        // And the pair the interpolation is drawn between has to differ while
        // the height is changing, or the aircraft is drawn at one step's value
        // for the whole of that step -- which is the jitter wearing a different
        // hat.
        assert_ne!(
            p.previous.alt, p.state.alt,
            "there is nothing to interpolate between during a dive"
        );
    }
}
