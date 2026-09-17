//! Impact effects: missile blasts, and the flash a shield makes soaking a hit.
//!
//! Entirely cosmetic and entirely driven by the server's [`HitFx`] list -- the
//! client never decides that something was hit, only how it looks when it was.
//!
//! Effects fade by swapping down a ladder of pre-built materials rather than by
//! editing one. A `StandardMaterial` handle is shared by every entity using it,
//! so animating the asset would animate every effect on screen at once; making
//! one per blast would instead churn the asset store on every shot.

use bevy::light::NotShadowCaster;
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;
use bevy::asset::RenderAssetUsages;
use orewar_shared::protocol::{HitKind, VehicleSlot};
use orewar_shared::sim;

use crate::coords;
use crate::state::GameState;

/// How long a missile blast lasts, and how wide it gets.
const BLAST_LIFE: f32 = 0.34;
const BLAST_RADIUS: f32 = 4.0;
/// Height the centre of a blast sits at. The simulation is flat, so an impact
/// carries no height of its own; without a lift most of the fireball would be
/// under the ground plane and only its cap would show.
const BLAST_CENTRE: f32 = 1.7;
/// A shield flash is a flinch, not a status light, so it is over quickly.
///
/// Deliberately shorter than [`sim::BULLET_COOLDOWN`]: at 0.28 a flash outlived
/// the gap between shots, so sustained fire kept one arc permanently on the hull
/// with a second overlapping it at a different rung of the fade. That reads as a
/// ghost stuck to the tank rather than as a shield taking hits.
const SHIELD_LIFE: f32 = 0.18;
/// Rungs on the fade ladder. Ten is past the point where the step shows.
const FADE_STEPS: usize = 10;

/// Radius of the flash as a multiple of the hull radius, at rest and at the
/// top of its swell. The floor clears 1.38, which is as wide as the shield
/// bubble in [`crate::vehicles`] gets, so the two never fight for depth.
const SHIELD_INNER: f32 = 1.44;
const SHIELD_SWELL: f32 = 0.20;

const BLAST_COLOR: [f32; 3] = [1.0, 0.62, 0.24];
const SHIELD_COLOR: [f32; 3] = [0.40, 0.74, 1.0];

#[derive(Resource)]
pub struct EffectAssets {
    blast_mesh: Handle<Mesh>,
    arc_mesh: Handle<Mesh>,
    blast_fade: Vec<Handle<StandardMaterial>>,
    shield_fade: Vec<Handle<StandardMaterial>>,
}

#[derive(Component)]
pub struct Effect {
    kind: HitKind,
    age: f32,
    life: f32,
    /// The vehicle a shield flash rides, so the arc stays on the hull as it
    /// drives rather than being left behind where the shot landed.
    follow: Option<(u8, VehicleSlot)>,
    /// Bearing the blow came in on, which is the face the arc sits on.
    angle: f32,
}

/// Builds the ladder for one colour: the same additive material, dimmed to
/// nothing. Additive means the fade is in the colour, not in an alpha value --
/// which also means these never need sorting against each other.
fn fade_ladder(materials: &mut Assets<StandardMaterial>, rgb: [f32; 3]) -> Vec<Handle<StandardMaterial>> {
    (0..FADE_STEPS)
        .map(|i| {
            let k = 1.0 - i as f32 / FADE_STEPS as f32;
            materials.add(StandardMaterial {
                base_color: Color::srgb(rgb[0] * k, rgb[1] * k, rgb[2] * k),
                unlit: true,
                alpha_mode: AlphaMode::Add,
                // The arc is a single surface seen from either side, and a blast
                // is hollow; both want their back faces.
                double_sided: true,
                cull_mode: None,
                ..default()
            })
        })
        .collect()
}

pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(EffectAssets {
        // Coarse on purpose: a faceted ball of light reads as debris, where a
        // smooth one reads as a bubble.
        blast_mesh: meshes.add(Sphere::new(1.0).mesh().ico(1).expect("1 subdivision is in range")),
        arc_mesh: meshes.add(shield_arc_mesh()),
        blast_fade: fade_ladder(&mut materials, BLAST_COLOR),
        shield_fade: fade_ladder(&mut materials, SHIELD_COLOR),
    });
}

/// The patch of a shield bubble facing the blow: a cap cut from a unit sphere
/// around `+X`.
///
/// A cap rather than the upright band this started as. A band stands off the
/// hull on a circle of its own, so from most angles it cuts straight through
/// the vehicle it is meant to be protecting, which reads as a slab stuck
/// through the tank rather than as a shield. Every point of a cap lies on the
/// sphere the shield bubble already occupies, so it can only sit on that
/// surface.
fn shield_arc_mesh() -> Mesh {
    /// Angular radius of the cap. Wide enough to read as a shield, narrow
    /// enough to say which side took the hit.
    const CAP: f32 = 0.85;
    const RINGS: usize = 5;
    const SEGMENTS: usize = 20;

    // A point at polar angle `theta` from `+X`, swept `phi` about that axis.
    // Simulation `+Y` is world `+Z`, and `yaw_to_quat` turns model `+X` into the
    // bearing the server sent, so the cap is built facing `+X`.
    let point = |theta: f32, phi: f32| {
        let (st, ct) = theta.sin_cos();
        let (sp, cp) = phi.sin_cos();
        Vec3::new(ct, st * sp, st * cp)
    };

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut push = |v: Vec3| {
        positions.push(v.to_array());
        // On a unit sphere about the origin, the position is the normal.
        normals.push(v.to_array());
        uvs.push([0.0, 0.0]);
    };

    for ring in 0..RINGS {
        let inner = ring as f32 / RINGS as f32 * CAP;
        let outer = (ring + 1) as f32 / RINGS as f32 * CAP;
        for seg in 0..SEGMENTS {
            let a = seg as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            let b = (seg + 1) as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            let (ia, ib) = (point(inner, a), point(inner, b));
            let (oa, ob) = (point(outer, a), point(outer, b));
            if ring == 0 {
                // The innermost ring degenerates to the pole.
                push(ia);
                push(oa);
                push(ob);
            } else {
                push(ia);
                push(oa);
                push(ob);
                push(ia);
                push(ob);
                push(ib);
            }
        }
    }

    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
}

/// Turns the impacts that arrived with the last snapshot into entities.
pub fn spawn(
    mut commands: Commands,
    assets: Res<EffectAssets>,
    time: Res<Time>,
    mut state: ResMut<GameState>,
    mut live: Query<&mut Effect>,
) {
    // The same clock `interpolate` runs on, so an impact appears on the
    // frame the world reaches the tick it happened in.
    for fx in state.take_due_fx(time.elapsed_secs_f64()) {
        // One shield, one flash. A hull under fire is hit faster than a flash
        // fades, so a second entity would not read as a second hit -- it would
        // just sit on top of the first. Restarting the arc already on that hull
        // shows the shield working harder instead.
        if fx.kind == HitKind::Shield {
            if let Some(mut arc) =
                live.iter_mut().find(|e| e.kind == HitKind::Shield && e.follow == fx.target())
            {
                arc.age = 0.0;
                arc.angle = fx.angle;
                continue;
            }
        }

        let (mesh, material, life) = match fx.kind {
            HitKind::Blast => (&assets.blast_mesh, &assets.blast_fade[0], BLAST_LIFE),
            HitKind::Shield => (&assets.arc_mesh, &assets.shield_fade[0], SHIELD_LIFE),
        };
        // Looked up here rather than carried on the wire: every client already
        // has every aircraft's altitude, and the alternative is a byte on a
        // struct that six of ride in every snapshot to describe something that
        // is on the ground almost every time.
        let lift = match fx.target() {
            Some((player, VehicleSlot::Plane)) => state
                .render
                .players
                .get(player as usize)
                .and_then(|p| p.as_ref())
                .and_then(|p| p.plane.as_ref())
                .map_or(0.0, |v| v.alt),
            _ => 0.0,
        };
        commands.spawn((
            Effect { kind: fx.kind, age: 0.0, life, follow: fx.target(), angle: fx.angle },
            Mesh3d(mesh.clone()),
            MeshMaterial3d(material.clone()),
            // A shield flash is re-placed on its hull every frame; only the
            // blast keeps the position it is spawned at -- so this is the one
            // chance to put it at the right height. `HitFx` carries a ground
            // position and nothing else, which was the whole truth while every
            // impact happened on the ground; two aircraft meeting is one that
            // does not, and a blast for it drawn on the grass would be the most
            // visible thing in the dogfight and in the wrong place.
            Transform::from_translation(coords::sim_to_world_at(fx.pos, BLAST_CENTRE + lift))
                .with_scale(Vec3::splat(0.01)),
            NotShadowCaster,
        ));
    }
}

/// Advances every effect and retires the ones that are done.
pub fn animate(
    mut commands: Commands,
    time: Res<Time>,
    state: Res<GameState>,
    assets: Res<EffectAssets>,
    mut effects: Query<(
        Entity,
        &mut Effect,
        &mut Transform,
        &mut MeshMaterial3d<StandardMaterial>,
    )>,
) {
    let dt = time.delta_secs();
    for (entity, mut effect, mut transform, mut material) in &mut effects {
        effect.age += dt;
        let t = effect.age / effect.life;
        if t >= 1.0 {
            commands.entity(entity).despawn();
            continue;
        }

        let ladder = match effect.kind {
            HitKind::Blast => &assets.blast_fade,
            HitKind::Shield => &assets.shield_fade,
        };
        let rung = ((t * FADE_STEPS as f32) as usize).min(FADE_STEPS - 1);
        if material.0.id() != ladder[rung].id() {
            material.0 = ladder[rung].clone();
        }

        match effect.kind {
            HitKind::Blast => {
                // Fast at first and slowing, the way a pressure wave goes: a
                // linear expansion reads as a balloon inflating.
                transform.scale = Vec3::splat(BLAST_RADIUS * t.sqrt());
            }
            HitKind::Shield => {
                let Some((player, slot)) = effect.follow else {
                    commands.entity(entity).despawn();
                    continue;
                };
                // The arc belongs to a hull, so it goes when the hull does.
                let Some(vehicle) = state
                    .render
                    .players
                    .get(player as usize)
                    .and_then(|p| p.as_ref())
                    .and_then(|p| p.vehicle(slot))
                else {
                    commands.entity(entity).despawn();
                    continue;
                };

                // Out and back within one flash: the shield taking the energy
                // and giving it up again. Held outside the widest the bubble in
                // `crate::vehicles` ever gets, so the flash lights up on the
                // shield rather than inside it.
                let swell = SHIELD_INNER + SHIELD_SWELL * (std::f32::consts::PI * t).sin();
                let radius = sim::tuning(slot.kind()).radius * swell;
                // Concentric with that bubble, which rides 1.6 above the hull --
                // and for an aircraft the hull is not on the ground, so the
                // flash has to climb with it. `alt` is zero for the two on the
                // ground, which is what it used to be for all three.
                // Uniform, because a cap of a sphere is only a cap of a sphere
                // while all three axes agree.
                *transform =
                    Transform::from_translation(coords::sim_to_world_at(vehicle.pos, vehicle.alt + 1.6))
                    .with_rotation(coords::yaw_to_quat(effect.angle))
                    .with_scale(Vec3::splat(radius));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::mesh::VertexAttributeValues;

    fn arc_positions() -> Vec<[f32; 3]> {
        let mesh = shield_arc_mesh();
        match mesh.attribute(Mesh::ATTRIBUTE_POSITION).expect("the arc has positions") {
            VertexAttributeValues::Float32x3(v) => v.clone(),
            other => panic!("unexpected position format: {other:?}"),
        }
    }

    /// Every vertex has to sit on the unit sphere, inside a cap around `+X`.
    ///
    /// This is the property that stops the flash cutting through the vehicle:
    /// `animate` scales it uniformly and centres it on the hull, so anything not
    /// on the sphere lands inside the hull instead of on the shield. `+X` is the
    /// other half of it -- `yaw_to_quat` turns model `+X` into the bearing the
    /// server reported, so a cap built facing anywhere else lights up the wrong
    /// side of the vehicle.
    #[test]
    fn the_shield_flash_is_a_cap_of_the_unit_sphere_around_plus_x() {
        let positions = arc_positions();
        assert!(!positions.is_empty());
        assert_eq!(positions.len() % 3, 0, "triangle list");

        let mut widest: f32 = 0.0;
        for p in &positions {
            let v = Vec3::from_array(*p);
            assert!((v.length() - 1.0).abs() < 1e-4, "vertex off the sphere at {v:?}");
            widest = widest.max(v.dot(Vec3::X).clamp(-1.0, 1.0).acos());
        }
        assert!(widest <= 0.851, "cap reaches {widest} rad, wider than it was built");
        assert!(widest > 0.8, "cap only reaches {widest} rad, narrower than intended");
    }

    /// The flash has to clear the shield bubble `crate::vehicles` draws, or the
    /// two surfaces land on each other and z-fight.
    #[test]
    fn the_shield_flash_stays_outside_the_shield_bubble() {
        // The widest that bubble gets, from `sync_vehicles`.
        const BUBBLE: f32 = 1.12 + 0.26;
        assert!(SHIELD_INNER > BUBBLE, "{SHIELD_INNER} would start inside the bubble");
    }

    /// A flash has to be over before the next shot can land, or arcs pile up
    /// on the same hull and the newest one never gets to be the only thing
    /// showing. This is the invariant that stopped the flash reading as a ghost
    /// stuck to the tank.
    #[test]
    fn a_shield_flash_is_shorter_than_the_gap_between_shots() {
        assert!(
            SHIELD_LIFE < sim::BULLET_COOLDOWN,
            "a {SHIELD_LIFE}s flash outlives the {}s reload",
            sim::BULLET_COOLDOWN
        );
    }

    /// The pulse has to start and finish small and peak in between, or the
    /// flash would pop into existence at full size and clip off.
    #[test]
    fn the_shield_pulse_swells_and_settles() {
        let pulse = |t: f32| 0.86 + 0.30 * (std::f32::consts::PI * t).sin();
        assert!((pulse(0.0) - 0.86).abs() < 1e-5);
        assert!((pulse(1.0) - 0.86).abs() < 1e-5);
        assert!(pulse(0.5) > pulse(0.0) * 1.2, "the middle should visibly swell");
    }
}
