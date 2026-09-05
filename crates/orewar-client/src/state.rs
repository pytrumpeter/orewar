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
    GameStatus, HitFx, InputFrame, PlayerInfo, ProjectileKind, Snapshot, VehicleSlot,
    VehicleSnapshot,
};
use orewar_shared::sim::{self, MoveState};
use orewar_shared::world::{self, Hill, MAX_PLAYERS, OreDeposit, TICK_DT};

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
    pub tank: Option<RenderVehicle>,
    pub harvester: Option<RenderVehicle>,
    /// Absent while the emplacement is rubble.
    pub sentinel: Option<RenderSentinel>,
}

impl RenderPlayer {
    pub fn vehicle(&self, slot: VehicleSlot) -> Option<&RenderVehicle> {
        match slot {
            VehicleSlot::Tank => self.tank.as_ref(),
            VehicleSlot::Harvester => self.harvester.as_ref(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RenderProjectile {
    pub id: u16,
    pub kind: ProjectileKind,
    pub owner: u8,
    pub pos: SimVec2,
    pub yaw: f32,
}

/// Everything the render systems read. Rebuilt every frame from the snapshot
/// buffer so drawing code never has to think about the network.
#[derive(Default)]
pub struct RenderWorld {
    pub players: Vec<Option<RenderPlayer>>,
    pub projectiles: Vec<RenderProjectile>,
}

/// Local prediction of the vehicle this client is driving.
#[derive(Default)]
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
        acked: u32,
        powerups: u16,
        hills: &[Hill],
    ) {
        let previous = self.state;
        let was_active = self.active && self.slot == slot;

        self.slot = slot;
        self.state =
            MoveState { pos: authoritative.pos, yaw: authoritative.yaw, speed: authoritative.speed };

        while self.history.front().is_some_and(|f| f.tick <= acked) {
            self.history.pop_front();
        }
        let replay: Vec<InputFrame> =
            self.history.iter().copied().filter(|f| f.controlling == slot).collect();
        for frame in replay {
            let _ = sim::step_vehicle(
                &mut self.state,
                frame.throttle,
                frame.steer,
                slot.kind(),
                powerups,
                hills,
                TICK_DT,
            );
        }

        // A correction is not a simulation step, so there is nothing to slide
        // between: both ends of the interpolation become the corrected state and
        // the residual is carried by `offset`, which decays on the render clock.
        self.previous = self.state;

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
                let vehicle = match slot {
                    VehicleSlot::Tank => me.tank.as_ref(),
                    VehicleSlot::Harvester => me.harvester.as_ref(),
                };
                match vehicle {
                    Some(v) => self.prediction.reconcile(
                        v,
                        slot,
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
                    let harvester = match (pa.and_then(|p| p.harvester), pb.harvester) {
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
                            tank,
                            harvester,
                            sentinel,
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
                                tank: p.tank.as_ref().map(RenderVehicle::from_snapshot),
                                harvester: p.harvester.as_ref().map(RenderVehicle::from_snapshot),
                                sentinel: p.sentinel.map(|s| RenderSentinel {
                                    hull: s.hull,
                                    turret_yaw: s.turret_yaw,
                                }),
                            });
                        }
                    }
                    projectiles.extend(latest.projectiles.iter().map(|p| RenderProjectile {
                        id: p.id,
                        kind: p.kind,
                        owner: p.owner,
                        pos: p.pos,
                        yaw: p.yaw,
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
            if let Some(Some(player)) = players.get_mut(local as usize) {
                let target = match slot {
                    VehicleSlot::Tank => player.tank.as_mut(),
                    VehicleSlot::Harvester => player.harvester.as_mut(),
                };
                if let Some(v) = target {
                    v.pos = pos;
                    v.yaw = yaw;
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
                controlling: VehicleSlot::Harvester,
                throttle: 1.0,
                ..Default::default()
            },
            0,
            &[],
        );
        assert!(!p.active, "prediction for the old vehicle must not keep running");
    }
}
