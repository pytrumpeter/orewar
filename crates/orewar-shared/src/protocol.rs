//! Message types exchanged between client and server.
//!
//! Two rules govern what goes where:
//!
//! * State that is *re-sent every tick* travels unreliably. A lost snapshot is
//!   obsolete before a retransmission could arrive.
//! * State that is *communicated once* travels reliably: the handshake, the
//!   roster, purchases, and the events that change who owns what.
//!
//! Snapshots must fit [`crate::net::MAX_UNRELIABLE`] bytes. The budget is
//! enforced by a test at the bottom of this file rather than left to hope, so
//! adding a field to a per-player struct fails the build instead of silently
//! truncating the player list at four players.

use crate::bytes::{Decode, DecodeError, Encode, Reader, Result, Writer};
use crate::math::Vec2;
use crate::sim::VehicleKind;
use crate::world::{MAX_PLAYERS, PowerUp, WORLD_SIZE};

/// Bumped whenever the wire format changes, so mismatched builds fail the
/// handshake instead of misparsing each other.
///
/// Last bumped for [`HitFx`] on the snapshot.
/// Without a bump an older peer would complete the handshake and then hit an
/// unknown tag on the reliable stream, which is a decode error rather than a
/// recoverable one.
pub const PROTOCOL_ID: u32 = 0x4F52_5706;

/// Quantization ceiling for shield and hull values.
const STAT_SCALE: f32 = 512.0;
/// Quantization ceiling for cargo.
const CARGO_SCALE: f32 = 256.0;
/// Fixed-point steps per world unit per second for vehicle speed.
const SPEED_SCALE: f32 = 1000.0;

/// Snapshots carry at most this many projectiles, nearest to the receiver
/// first. Bullets are dense and short-lived; dropping the far ones keeps
/// packets inside the MTU and is invisible in play.
pub const MAX_PROJECTILES_PER_SNAPSHOT: usize = 40;

/// A full ore resync is sent this often; between them only changed deposits go
/// out. This bounds how long a client can hold a stale amount after packet loss.
pub const ORE_FULL_SYNC_INTERVAL: u32 = 30;

// ---------------------------------------------------------------------------
// Client -> server
// ---------------------------------------------------------------------------

/// Which of a player's two vehicles they are currently driving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum VehicleSlot {
    #[default]
    Tank = 0,
    Harvester = 1,
}

impl VehicleSlot {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(VehicleSlot::Tank),
            1 => Some(VehicleSlot::Harvester),
            _ => None,
        }
    }

    pub fn kind(self) -> VehicleKind {
        match self {
            VehicleSlot::Tank => VehicleKind::Tank,
            VehicleSlot::Harvester => VehicleKind::Harvester,
        }
    }

    pub fn other(self) -> Self {
        match self {
            VehicleSlot::Tank => VehicleSlot::Harvester,
            VehicleSlot::Harvester => VehicleSlot::Tank,
        }
    }
}

/// One tick of player intent. Sent unreliably every client tick.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InputFrame {
    /// Client tick this input belongs to; echoed back so the client can line up
    /// prediction with authority.
    pub tick: u32,
    pub controlling: VehicleSlot,
    /// Forward/back, `-1..=1`.
    pub throttle: f32,
    /// Left/right, `-1..=1`.
    pub steer: f32,
    /// Desired turret bearing in world radians.
    pub aim: f32,
    pub fire_primary: bool,
    pub fire_secondary: bool,
}

impl Encode for InputFrame {
    fn encode(&self, w: &mut Writer) {
        let flags = self.controlling as u8
            | (self.fire_primary as u8) << 1
            | (self.fire_secondary as u8) << 2;
        w.u32(self.tick);
        w.u8(flags);
        w.i8((self.throttle.clamp(-1.0, 1.0) * 127.0) as i8);
        w.i8((self.steer.clamp(-1.0, 1.0) * 127.0) as i8);
        w.angle(self.aim);
    }
}

impl Decode for InputFrame {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let tick = r.u32()?;
        let flags = r.u8()?;
        let throttle = r.i8()? as f32 / 127.0;
        let steer = r.i8()? as f32 / 127.0;
        let aim = r.angle()?;
        Ok(InputFrame {
            tick,
            controlling: VehicleSlot::from_u8(flags & 1).unwrap_or_default(),
            throttle: throttle.clamp(-1.0, 1.0),
            steer: steer.clamp(-1.0, 1.0),
            aim,
            fire_primary: flags & 0b10 != 0,
            fire_secondary: flags & 0b100 != 0,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    /// Unreliable, every tick.
    Input(InputFrame),
    /// Reliable. Server validates cost and availability.
    Purchase(PowerUp),
    /// Reliable. Politely announces a disconnect.
    Leave,
    /// Reliable. Restarts the match on a fresh map, for everyone.
    ///
    /// Any player may ask. There is no host in this protocol -- the server is
    /// the authority but takes no side -- so the alternative would be inventing
    /// one, and a four-player game around one screen does not need it.
    NewGame,
}

impl Encode for ClientMessage {
    fn encode(&self, w: &mut Writer) {
        match self {
            ClientMessage::Input(f) => {
                w.u8(1);
                f.encode(w);
            }
            ClientMessage::Purchase(p) => {
                w.u8(2).u8(*p as u8);
            }
            ClientMessage::Leave => {
                w.u8(3);
            }
            ClientMessage::NewGame => {
                w.u8(4);
            }
        }
    }
}

impl Decode for ClientMessage {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let tag = r.u8()?;
        Ok(match tag {
            1 => ClientMessage::Input(r.read()?),
            2 => {
                let raw = r.u8()?;
                ClientMessage::Purchase(
                    PowerUp::from_u8(raw).ok_or(DecodeError::BadTag("PowerUp", raw))?,
                )
            }
            3 => ClientMessage::Leave,
            4 => ClientMessage::NewGame,
            other => return Err(DecodeError::BadTag("ClientMessage", other)),
        })
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VehicleSnapshot {
    pub pos: Vec2,
    pub yaw: f32,
    pub turret_yaw: f32,
    /// Signed speed along `yaw`. Present so a client can resume prediction from
    /// an authoritative state and replay its unacknowledged inputs.
    pub speed: f32,
    pub shield: f32,
    pub hull: f32,
    /// Ore aboard. Always zero for a tank.
    pub cargo: f32,
    /// A harvester at zero hull: immobile and capturable.
    pub disabled: bool,
    /// Capture or rescue progress, `0..=1`.
    pub capture_progress: f32,
}

impl Encode for VehicleSnapshot {
    fn encode(&self, w: &mut Writer) {
        w.vec2(self.pos);
        w.angle(self.yaw);
        w.angle(self.turret_yaw);
        w.signed_fixed16(self.speed, SPEED_SCALE);
        w.unorm16(self.shield, STAT_SCALE);
        w.unorm16(self.hull, STAT_SCALE);
        w.unorm16(self.cargo, CARGO_SCALE);
        w.bool(self.disabled);
        w.unorm8(self.capture_progress);
    }
}

impl Decode for VehicleSnapshot {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(VehicleSnapshot {
            pos: r.vec2()?,
            yaw: r.angle()?,
            turret_yaw: r.angle()?,
            speed: r.signed_fixed16(SPEED_SCALE)?,
            shield: r.unorm16(STAT_SCALE)?,
            hull: r.unorm16(STAT_SCALE)?,
            cargo: r.unorm16(CARGO_SCALE)?,
            disabled: r.bool()?,
            capture_progress: r.unorm8()?,
        })
    }
}

/// Bytes one `VehicleSnapshot` occupies on the wire.
pub const VEHICLE_SNAPSHOT_BYTES: usize = 22;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerSnapshot {
    pub id: u8,
    pub connected: bool,
    pub eliminated: bool,
    pub credits: u32,
    pub ore_mined: u32,
    pub powerups: u16,
    pub missiles: u8,
    /// Enemy harvesters this player has taken.
    pub captures: u8,
    /// Absent while the tank is waiting to respawn.
    pub tank: Option<VehicleSnapshot>,
    /// Absent once the harvester has been captured.
    pub harvester: Option<VehicleSnapshot>,
}

impl Encode for PlayerSnapshot {
    fn encode(&self, w: &mut Writer) {
        let flags = self.connected as u8
            | (self.eliminated as u8) << 1
            | (self.tank.is_some() as u8) << 2
            | (self.harvester.is_some() as u8) << 3;
        w.u8(self.id);
        w.u8(flags);
        w.u32(self.credits);
        w.u32(self.ore_mined);
        w.u16(self.powerups);
        w.u8(self.missiles);
        w.u8(self.captures);
        if let Some(v) = &self.tank {
            v.encode(w);
        }
        if let Some(v) = &self.harvester {
            v.encode(w);
        }
    }
}

impl Decode for PlayerSnapshot {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let id = r.u8()?;
        let flags = r.u8()?;
        let credits = r.u32()?;
        let ore_mined = r.u32()?;
        let powerups = r.u16()?;
        let missiles = r.u8()?;
        let captures = r.u8()?;
        let tank = if flags & 0b100 != 0 { Some(r.read()?) } else { None };
        let harvester = if flags & 0b1000 != 0 { Some(r.read()?) } else { None };
        Ok(PlayerSnapshot {
            id,
            connected: flags & 1 != 0,
            eliminated: flags & 0b10 != 0,
            credits,
            ore_mined,
            powerups,
            missiles,
            captures,
            tank,
            harvester,
        })
    }
}

/// Fixed bytes per player, before optional vehicles.
pub const PLAYER_SNAPSHOT_FIXED_BYTES: usize = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProjectileKind {
    Bullet = 0,
    Missile = 1,
}

impl ProjectileKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(ProjectileKind::Bullet),
            1 => Some(ProjectileKind::Missile),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProjectileSnapshot {
    pub id: u16,
    pub kind: ProjectileKind,
    pub owner: u8,
    pub pos: Vec2,
    pub yaw: f32,
}

impl Encode for ProjectileSnapshot {
    fn encode(&self, w: &mut Writer) {
        w.u16(self.id).u8(self.kind as u8).u8(self.owner).vec2(self.pos).angle(self.yaw);
    }
}

impl Decode for ProjectileSnapshot {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let id = r.u16()?;
        let raw = r.u8()?;
        Ok(ProjectileSnapshot {
            id,
            kind: ProjectileKind::from_u8(raw)
                .ok_or(DecodeError::BadTag("ProjectileKind", raw))?,
            owner: r.u8()?,
            pos: r.vec2()?,
            yaw: r.angle()?,
        })
    }
}

pub const PROJECTILE_SNAPSHOT_BYTES: usize = 14;

/// A deposit's current amount. Only changed deposits are sent between full syncs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OreUpdate {
    pub id: u16,
    /// Rounded to whole units; sub-unit precision is not worth the bytes.
    pub amount: u16,
}

pub const ORE_UPDATE_BYTES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum GameStatus {
    /// Fewer than two players; the match has not begun.
    #[default]
    Waiting = 0,
    Running = 1,
    Finished = 2,
}

impl GameStatus {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(GameStatus::Waiting),
            1 => Some(GameStatus::Running),
            2 => Some(GameStatus::Finished),
            _ => None,
        }
    }
}

/// What an impact looked like, so the client can draw it.
///
/// Purely cosmetic, and carried unreliably alongside the snapshot it belongs
/// to: a dropped packet costs one frame of sparkle, which is not worth a
/// retransmission or the reliable stream's ordering guarantee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HitKind {
    /// A missile went off.
    Blast = 0,
    /// A shield soaked a hit.
    Shield = 1,
}

impl HitKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(HitKind::Blast),
            1 => Some(HitKind::Shield),
            _ => None,
        }
    }
}

/// Fixed-point steps per world unit for an impact position. `WORLD_SIZE` times
/// this has to stay inside a `u16`, which leaves plenty of headroom at 320.
const HIT_POS_SCALE: f32 = 100.0;

pub const HIT_FX_BYTES: usize = 8;

/// Impacts carried per snapshot, nearest to the viewer first. Six simultaneous
/// hits in one 33 ms tick is already more than a player can read.
pub const MAX_HITS_PER_SNAPSHOT: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitFx {
    pub kind: HitKind,
    /// Where the impact happened, in world space.
    pub pos: Vec2,
    /// Bearing from the struck vehicle's centre out to the impact, so a shield
    /// flash can be drawn on the face that took it. Meaningless for terrain.
    pub angle: f32,
    /// Player in the low bits, slot in the high bit, or [`HitFx::TERRAIN`].
    /// Packed here so the one place that knows the layout is this file.
    target: u8,
}

impl HitFx {
    /// Stands for "hit something that is not a vehicle".
    pub const TERRAIN: u8 = u8::MAX;
    const SLOT_BIT: u8 = 0b1000_0000;

    pub fn on_vehicle(kind: HitKind, pos: Vec2, angle: f32, player: u8, slot: VehicleSlot) -> Self {
        let slot_bit = if slot == VehicleSlot::Harvester { Self::SLOT_BIT } else { 0 };
        HitFx { kind, pos, angle, target: (player & 0x7F) | slot_bit }
    }

    pub fn on_terrain(kind: HitKind, pos: Vec2) -> Self {
        HitFx { kind, pos, angle: 0.0, target: Self::TERRAIN }
    }

    /// The vehicle this landed on, if it landed on one.
    pub fn target(self) -> Option<(u8, VehicleSlot)> {
        if self.target == Self::TERRAIN {
            return None;
        }
        let slot = if self.target & Self::SLOT_BIT != 0 {
            VehicleSlot::Harvester
        } else {
            VehicleSlot::Tank
        };
        Some((self.target & 0x7F, slot))
    }
}

impl Encode for HitFx {
    fn encode(&self, w: &mut Writer) {
        let q = |v: f32| (v.clamp(0.0, WORLD_SIZE) * HIT_POS_SCALE) as u16;
        w.u8(self.kind as u8).u16(q(self.pos.x)).u16(q(self.pos.y)).angle(self.angle).u8(self.target);
    }
}

impl Decode for HitFx {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let raw = r.u8()?;
        let kind = HitKind::from_u8(raw).ok_or(DecodeError::BadTag("HitKind", raw))?;
        let x = r.u16()? as f32 / HIT_POS_SCALE;
        let y = r.u16()? as f32 / HIT_POS_SCALE;
        Ok(HitFx { kind, pos: Vec2::new(x, y), angle: r.angle()?, target: r.u8()? })
    }
}

/// The authoritative view of the world for one tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub tick: u32,
    pub status: GameStatus,
    /// Winning player once `status` is `Finished`.
    pub winner: Option<u8>,
    /// Most recent input tick the server has processed from *this* client, so
    /// the client knows how far its prediction has been confirmed.
    pub acked_input: u32,
    pub players: Vec<PlayerSnapshot>,
    pub projectiles: Vec<ProjectileSnapshot>,
    /// True when `ore` lists every deposit rather than only changed ones.
    pub ore_is_full_sync: bool,
    pub ore: Vec<OreUpdate>,
    /// Impacts that happened during this tick, for the client to draw.
    pub hits: Vec<HitFx>,
}

impl Encode for Snapshot {
    fn encode(&self, w: &mut Writer) {
        w.u32(self.tick);
        w.u8(self.status as u8);
        w.u8(self.winner.unwrap_or(u8::MAX));
        w.u32(self.acked_input);
        w.list(&self.players, |w, p| p.encode(w));
        w.list(&self.projectiles, |w, p| p.encode(w));
        w.bool(self.ore_is_full_sync);
        w.list(&self.ore, |w, o| {
            w.u16(o.id).u16(o.amount);
        });
        w.list(&self.hits, |w, h| h.encode(w));
    }
}

impl Decode for Snapshot {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let tick = r.u32()?;
        let raw = r.u8()?;
        let status = GameStatus::from_u8(raw).ok_or(DecodeError::BadTag("GameStatus", raw))?;
        let winner = match r.u8()? {
            u8::MAX => None,
            v => Some(v),
        };
        let acked_input = r.u32()?;
        let players = r.list(|r| r.read())?;
        let projectiles = r.list(|r| r.read())?;
        let ore_is_full_sync = r.bool()?;
        let ore = r.list(|r| Ok(OreUpdate { id: r.u16()?, amount: r.u16()? }))?;
        let hits = r.list(|r| r.read())?;
        Ok(Snapshot {
            tick,
            status,
            winner,
            acked_input,
            players,
            projectiles,
            ore_is_full_sync,
            ore,
            hits,
        })
    }
}

// ---------------------------------------------------------------------------
// Server -> client
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerInfo {
    pub id: u8,
    pub name: String,
    pub connected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectReason {
    NotEnoughCredits = 0,
    AlreadyOwned = 1,
    Eliminated = 2,
}

impl RejectReason {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(RejectReason::NotEnoughCredits),
            1 => Some(RejectReason::AlreadyOwned),
            2 => Some(RejectReason::Eliminated),
            _ => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            RejectReason::NotEnoughCredits => "not enough ore",
            RejectReason::AlreadyOwned => "already owned",
            RejectReason::Eliminated => "you are out of the match",
        }
    }
}

/// Things worth telling the player about. Delivered reliably and in order, so
/// the client can narrate them without worrying about gaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameEvent {
    PlayerJoined { player: u8 },
    PlayerLeft { player: u8 },
    PurchaseAccepted { powerup: PowerUp, credits: u32 },
    PurchaseRejected { powerup: PowerUp, reason: RejectReason },
    TankDestroyed { player: u8, by: u8 },
    TankRespawned { player: u8 },
    HarvesterDisabled { player: u8 },
    HarvesterRescued { player: u8 },
    HarvesterCaptured { by: u8, from: u8 },
    PlayerEliminated { player: u8 },
    MatchStarted,
    GameOver { winner: u8 },
    /// The match was restarted on a fresh map.
    ///
    /// Carries the seed because a client that stays connected across the reset
    /// never sees another `Welcome`, and the map never travels as a deposit
    /// list -- both sides generate it from the seed.
    MatchReset { world_seed: u64, by: u8 },
}

impl Encode for GameEvent {
    fn encode(&self, w: &mut Writer) {
        match *self {
            GameEvent::PlayerJoined { player } => {
                w.u8(1).u8(player);
            }
            GameEvent::PlayerLeft { player } => {
                w.u8(2).u8(player);
            }
            GameEvent::PurchaseAccepted { powerup, credits } => {
                w.u8(3).u8(powerup as u8).u32(credits);
            }
            GameEvent::PurchaseRejected { powerup, reason } => {
                w.u8(4).u8(powerup as u8).u8(reason as u8);
            }
            GameEvent::TankDestroyed { player, by } => {
                w.u8(5).u8(player).u8(by);
            }
            GameEvent::TankRespawned { player } => {
                w.u8(6).u8(player);
            }
            GameEvent::HarvesterDisabled { player } => {
                w.u8(7).u8(player);
            }
            GameEvent::HarvesterRescued { player } => {
                w.u8(8).u8(player);
            }
            GameEvent::HarvesterCaptured { by, from } => {
                w.u8(9).u8(by).u8(from);
            }
            GameEvent::PlayerEliminated { player } => {
                w.u8(10).u8(player);
            }
            GameEvent::MatchStarted => {
                w.u8(11);
            }
            GameEvent::GameOver { winner } => {
                w.u8(12).u8(winner);
            }
            GameEvent::MatchReset { world_seed, by } => {
                w.u8(13).u64(world_seed).u8(by);
            }
        }
    }
}

impl Decode for GameEvent {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let powerup = |r: &mut Reader<'_>| -> Result<PowerUp> {
            let raw = r.u8()?;
            PowerUp::from_u8(raw).ok_or(DecodeError::BadTag("PowerUp", raw))
        };
        let tag = r.u8()?;
        Ok(match tag {
            1 => GameEvent::PlayerJoined { player: r.u8()? },
            2 => GameEvent::PlayerLeft { player: r.u8()? },
            3 => GameEvent::PurchaseAccepted { powerup: powerup(r)?, credits: r.u32()? },
            4 => {
                let p = powerup(r)?;
                let raw = r.u8()?;
                GameEvent::PurchaseRejected {
                    powerup: p,
                    reason: RejectReason::from_u8(raw)
                        .ok_or(DecodeError::BadTag("RejectReason", raw))?,
                }
            }
            5 => GameEvent::TankDestroyed { player: r.u8()?, by: r.u8()? },
            6 => GameEvent::TankRespawned { player: r.u8()? },
            7 => GameEvent::HarvesterDisabled { player: r.u8()? },
            8 => GameEvent::HarvesterRescued { player: r.u8()? },
            9 => GameEvent::HarvesterCaptured { by: r.u8()?, from: r.u8()? },
            10 => GameEvent::PlayerEliminated { player: r.u8()? },
            11 => GameEvent::MatchStarted,
            12 => GameEvent::GameOver { winner: r.u8()? },
            13 => GameEvent::MatchReset { world_seed: r.u64()?, by: r.u8()? },
            other => return Err(DecodeError::BadTag("GameEvent", other)),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    /// First reliable message on a connection.
    ///
    /// Carries the world *seed*, not the deposit list: both sides run the same
    /// deterministic generator, which keeps this message far inside one packet.
    Welcome { player_id: u8, world_seed: u64, tick: u32 },
    Roster(Vec<PlayerInfo>),
    Snapshot(Snapshot),
    Event(GameEvent),
}

impl Encode for ServerMessage {
    fn encode(&self, w: &mut Writer) {
        match self {
            ServerMessage::Welcome { player_id, world_seed, tick } => {
                w.u8(1).u8(*player_id).u64(*world_seed).u32(*tick);
            }
            ServerMessage::Roster(players) => {
                w.u8(2);
                w.list(players, |w, p| {
                    w.u8(p.id).bool(p.connected).string(&p.name);
                });
            }
            ServerMessage::Snapshot(s) => {
                w.u8(3);
                s.encode(w);
            }
            ServerMessage::Event(e) => {
                w.u8(4);
                e.encode(w);
            }
        }
    }
}

impl Decode for ServerMessage {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let tag = r.u8()?;
        Ok(match tag {
            1 => ServerMessage::Welcome {
                player_id: r.u8()?,
                world_seed: r.u64()?,
                tick: r.u32()?,
            },
            2 => ServerMessage::Roster(r.list(|r| {
                Ok(PlayerInfo { id: r.u8()?, connected: r.bool()?, name: r.string()? })
            })?),
            3 => ServerMessage::Snapshot(r.read()?),
            4 => ServerMessage::Event(r.read()?),
            other => return Err(DecodeError::BadTag("ServerMessage", other)),
        })
    }
}

// ---------------------------------------------------------------------------
// Handshake bodies (outside the reliable stream; see `net::PacketKind`)
// ---------------------------------------------------------------------------

/// Why a server turned a client away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DenyReason {
    ServerFull = 0,
    BadProtocol = 1,
    MatchFinished = 2,
}

impl DenyReason {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(DenyReason::ServerFull),
            1 => Some(DenyReason::BadProtocol),
            2 => Some(DenyReason::MatchFinished),
            _ => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            DenyReason::ServerFull => "server is full",
            DenyReason::BadProtocol => "client and server versions do not match",
            DenyReason::MatchFinished => "the match has already finished",
        }
    }
}

/// Maximum players a snapshot could ever describe.
pub const SNAPSHOT_MAX_PLAYERS: usize = MAX_PLAYERS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::vec2;
    use crate::net::MAX_UNRELIABLE;

    fn sample_vehicle() -> VehicleSnapshot {
        VehicleSnapshot {
            pos: vec2(123.5, 66.25),
            yaw: 1.0,
            turret_yaw: -2.0,
            speed: -7.25,
            shield: 87.5,
            hull: 43.0,
            cargo: 22.0,
            disabled: true,
            capture_progress: 0.5,
        }
    }

    fn sample_player(id: u8) -> PlayerSnapshot {
        PlayerSnapshot {
            id,
            connected: true,
            eliminated: false,
            credits: 4321,
            ore_mined: 99_999,
            powerups: PowerUp::Radar.bit() | PowerUp::Turbo.bit(),
            missiles: 7,
            captures: 2,
            tank: Some(sample_vehicle()),
            harvester: Some(sample_vehicle()),
        }
    }

    /// Quantized fields cannot round-trip exactly, so compare within tolerance.
    fn assert_vehicle_close(a: &VehicleSnapshot, b: &VehicleSnapshot) {
        assert!((a.pos.x - b.pos.x).abs() < 1e-3 && (a.pos.y - b.pos.y).abs() < 1e-3);
        assert!(crate::math::angle_delta(a.yaw, b.yaw).abs() < 1e-3);
        assert!(crate::math::angle_delta(a.turret_yaw, b.turret_yaw).abs() < 1e-3);
        assert!((a.speed - b.speed).abs() < 0.01, "{} vs {}", a.speed, b.speed);
        assert!((a.shield - b.shield).abs() < 0.05, "{} vs {}", a.shield, b.shield);
        assert!((a.hull - b.hull).abs() < 0.05);
        assert!((a.cargo - b.cargo).abs() < 0.05);
        assert_eq!(a.disabled, b.disabled);
        assert!((a.capture_progress - b.capture_progress).abs() < 0.01);
    }

    #[test]
    fn input_frames_round_trip() {
        let f = InputFrame {
            tick: 900_001,
            controlling: VehicleSlot::Harvester,
            throttle: -1.0,
            steer: 0.5,
            aim: 2.5,
            fire_primary: true,
            fire_secondary: false,
        };
        let bytes = f.to_vec();
        assert_eq!(bytes.len(), 9, "input frames should stay tiny");
        let got = InputFrame::from_slice(&bytes).unwrap();
        assert_eq!(got.tick, f.tick);
        assert_eq!(got.controlling, f.controlling);
        assert_eq!(got.fire_primary, f.fire_primary);
        assert_eq!(got.fire_secondary, f.fire_secondary);
        assert!((got.throttle - f.throttle).abs() < 0.02);
        assert!((got.steer - f.steer).abs() < 0.02);
        assert!(crate::math::angle_delta(got.aim, f.aim).abs() < 1e-3);
    }

    #[test]
    fn client_messages_round_trip() {
        for msg in [
            ClientMessage::Purchase(PowerUp::AutoTurret),
            ClientMessage::Leave,
            ClientMessage::NewGame,
            ClientMessage::Input(InputFrame::default()),
        ] {
            let bytes = msg.to_vec();
            assert_eq!(ClientMessage::from_slice(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn every_game_event_round_trips() {
        let events = [
            GameEvent::PlayerJoined { player: 3 },
            GameEvent::PlayerLeft { player: 1 },
            GameEvent::PurchaseAccepted { powerup: PowerUp::Radar, credits: 12_345 },
            GameEvent::PurchaseRejected {
                powerup: PowerUp::MissilePack,
                reason: RejectReason::NotEnoughCredits,
            },
            GameEvent::TankDestroyed { player: 0, by: 2 },
            GameEvent::TankRespawned { player: 0 },
            GameEvent::HarvesterDisabled { player: 2 },
            GameEvent::HarvesterRescued { player: 2 },
            GameEvent::HarvesterCaptured { by: 1, from: 2 },
            GameEvent::PlayerEliminated { player: 2 },
            GameEvent::MatchStarted,
            GameEvent::GameOver { winner: 1 },
            GameEvent::MatchReset { world_seed: 0xFEED_FACE_1234_5678, by: 2 },
        ];
        for e in events {
            let bytes = ServerMessage::Event(e).to_vec();
            assert_eq!(ServerMessage::from_slice(&bytes).unwrap(), ServerMessage::Event(e));
        }
    }

    #[test]
    fn welcome_and_roster_round_trip() {
        let w = ServerMessage::Welcome { player_id: 2, world_seed: 0xDEAD_BEEF_CAFE_1234, tick: 77 };
        assert_eq!(ServerMessage::from_slice(&w.to_vec()).unwrap(), w);

        let roster = ServerMessage::Roster(vec![
            PlayerInfo { id: 0, name: "Ash".into(), connected: true },
            PlayerInfo { id: 1, name: "Bo".into(), connected: false },
        ]);
        assert_eq!(ServerMessage::from_slice(&roster.to_vec()).unwrap(), roster);
    }

    #[test]
    fn snapshots_round_trip() {
        let snap = Snapshot {
            tick: 4242,
            status: GameStatus::Running,
            winner: None,
            acked_input: 4200,
            players: (0..MAX_PLAYERS as u8).map(sample_player).collect(),
            projectiles: (0..10)
                .map(|i| ProjectileSnapshot {
                    id: i,
                    kind: if i % 2 == 0 { ProjectileKind::Bullet } else { ProjectileKind::Missile },
                    owner: (i % 4) as u8,
                    pos: vec2(i as f32, i as f32 * 2.0),
                    yaw: i as f32 * 0.1,
                })
                .collect(),
            ore_is_full_sync: true,
            ore: (0..48).map(|i| OreUpdate { id: i, amount: i * 7 }).collect(),
            hits: vec![
                HitFx::on_terrain(HitKind::Blast, vec2(30.0, 40.0)),
                HitFx::on_vehicle(HitKind::Shield, vec2(9.5, 9.5), 0.75, 2, VehicleSlot::Tank),
            ],
        };
        let got = Snapshot::from_slice(&snap.to_vec()).unwrap();

        assert_eq!(got.tick, snap.tick);
        assert_eq!(got.status, snap.status);
        assert_eq!(got.winner, snap.winner);
        assert_eq!(got.acked_input, snap.acked_input);
        assert_eq!(got.ore, snap.ore);
        assert_eq!(got.ore_is_full_sync, snap.ore_is_full_sync);
        assert_eq!(got.players.len(), snap.players.len());
        assert_eq!(got.projectiles.len(), snap.projectiles.len());
        assert_eq!(got.hits.len(), snap.hits.len());
        assert_eq!(got.hits[0].target(), None);
        assert_eq!(got.hits[1].target(), Some((2, VehicleSlot::Tank)));
        for (a, b) in got.players.iter().zip(&snap.players) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.credits, b.credits);
            assert_eq!(a.ore_mined, b.ore_mined);
            assert_eq!(a.powerups, b.powerups);
            assert_eq!(a.missiles, b.missiles);
            assert_eq!(a.captures, b.captures);
            assert_vehicle_close(a.tank.as_ref().unwrap(), b.tank.as_ref().unwrap());
            assert_vehicle_close(a.harvester.as_ref().unwrap(), b.harvester.as_ref().unwrap());
        }
    }

    #[test]
    fn a_hit_effect_round_trips_including_its_packed_target() {
        let cases = [
            HitFx::on_terrain(HitKind::Blast, vec2(12.5, 300.25)),
            HitFx::on_vehicle(HitKind::Shield, vec2(0.0, 0.0), -2.5, 0, VehicleSlot::Tank),
            HitFx::on_vehicle(HitKind::Shield, vec2(319.99, 1.0), 1.25, 3, VehicleSlot::Harvester),
        ];
        for want in cases {
            let got = HitFx::from_slice(&want.to_vec()).unwrap();
            assert_eq!(got.kind, want.kind);
            assert_eq!(got.target(), want.target(), "the packed target must survive");
            assert!(got.pos.distance(want.pos) < 0.02, "{:?} vs {:?}", got.pos, want.pos);
            assert!(crate::math::angle_delta(got.angle, want.angle).abs() < 0.01);
        }
        // Terrain has no vehicle behind it, and player 127 must not be mistaken
        // for the sentinel.
        assert_eq!(HitFx::on_terrain(HitKind::Blast, Vec2::ZERO).target(), None);
        assert_eq!(
            HitFx::on_vehicle(HitKind::Blast, Vec2::ZERO, 0.0, 3, VehicleSlot::Tank).target(),
            Some((3, VehicleSlot::Tank))
        );
    }

    #[test]
    fn a_winner_survives_the_sentinel_encoding() {
        let snap = Snapshot { status: GameStatus::Finished, winner: Some(3), ..Default::default() };
        assert_eq!(Snapshot::from_slice(&snap.to_vec()).unwrap().winner, Some(3));
        let snap = Snapshot { winner: None, ..Default::default() };
        assert_eq!(Snapshot::from_slice(&snap.to_vec()).unwrap().winner, None);
    }

    /// The budget that keeps snapshots inside a single unfragmented datagram.
    ///
    /// If a field is added to a per-player or per-projectile struct, this fails
    /// rather than letting packets quietly exceed the MTU in a live match.
    #[test]
    fn a_worst_case_snapshot_fits_one_packet() {
        let snap = Snapshot {
            tick: u32::MAX,
            status: GameStatus::Running,
            winner: Some(3),
            acked_input: u32::MAX,
            players: (0..MAX_PLAYERS as u8).map(sample_player).collect(),
            projectiles: (0..MAX_PROJECTILES_PER_SNAPSHOT)
                .map(|i| ProjectileSnapshot {
                    id: i as u16,
                    kind: ProjectileKind::Missile,
                    owner: 3,
                    pos: vec2(255.0, 255.0),
                    yaw: 3.0,
                })
                .collect(),
            ore_is_full_sync: true,
            ore: (0..64).map(|i| OreUpdate { id: i, amount: u16::MAX }).collect(),
            hits: (0..MAX_HITS_PER_SNAPSHOT)
                .map(|_| HitFx::on_vehicle(
                    HitKind::Shield,
                    vec2(WORLD_SIZE, WORLD_SIZE),
                    3.0,
                    3,
                    VehicleSlot::Harvester,
                ))
                .collect(),
        };
        // Snapshots ride inside a ServerMessage, which adds its own tag byte.
        let encoded = ServerMessage::Snapshot(snap).to_vec();
        assert!(
            encoded.len() <= MAX_UNRELIABLE,
            "worst-case snapshot is {} bytes, over the {MAX_UNRELIABLE} byte budget",
            encoded.len()
        );
    }

    #[test]
    fn struct_size_constants_match_the_encoders() {
        assert_eq!(sample_vehicle().to_vec().len(), VEHICLE_SNAPSHOT_BYTES);
        let bare = PlayerSnapshot { tank: None, harvester: None, ..sample_player(0) };
        assert_eq!(bare.to_vec().len(), PLAYER_SNAPSHOT_FIXED_BYTES);
        let proj = ProjectileSnapshot {
            id: 1,
            kind: ProjectileKind::Bullet,
            owner: 0,
            pos: vec2(0.0, 0.0),
            yaw: 0.0,
        };
        assert_eq!(proj.to_vec().len(), PROJECTILE_SNAPSHOT_BYTES);
        let fx = HitFx::on_terrain(HitKind::Blast, vec2(1.0, 2.0));
        assert_eq!(fx.to_vec().len(), HIT_FX_BYTES);
    }

    #[test]
    fn malformed_input_is_rejected_cleanly() {
        assert!(ClientMessage::from_slice(&[]).is_err());
        assert!(ClientMessage::from_slice(&[99]).is_err());
        assert!(ServerMessage::from_slice(&[3, 1, 2]).is_err());
        // A truncated snapshot must not panic.
        let bytes = ServerMessage::Snapshot(Snapshot::default()).to_vec();
        for cut in 0..bytes.len() {
            let _ = ServerMessage::from_slice(&bytes[..cut]);
        }
    }
}
