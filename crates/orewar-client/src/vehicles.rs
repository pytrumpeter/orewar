//! Spawns and updates the 3D views of vehicles and projectiles.
//!
//! Nothing here simulates anything. Every frame it reconciles the set of
//! entities on screen against [`GameState::render`], creating what is new,
//! removing what is gone, and moving the rest.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::light::NotShadowCaster;
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;
use orewar_shared::protocol::{ProjectileKind, VehicleSlot};
use orewar_shared::sim::{self, VehicleKind};
use orewar_shared::world::MAX_PLAYERS;

use crate::coords;
use crate::input::LocalInput;
use crate::state::{GameState, RenderVehicle};

#[derive(Component)]
pub struct VehicleView {
    pub player: u8,
    pub slot: VehicleSlot,
}

/// The rotating part of a tank, and the auto-turret on a harvester.
#[derive(Component)]
pub struct TurretView;

#[derive(Component)]
pub struct ShieldBubble;

/// Floating bar over a disabled harvester showing capture progress.
#[derive(Component)]
pub struct CaptureBar;

/// Marks a projectile's view entity. The id lives in [`ViewRegistry`], which
/// is what maps snapshots onto entities.
#[derive(Component)]
pub struct ProjectileView;

/// A bomb on its way down, and when this client first saw it.
///
/// Height is the one thing about a bomb the wire cannot afford to carry, so it
/// is reconstructed here from how long the bomb has been on screen against
/// [`sim::BOMB_FALL_TIME`]. Where it will land is not guesswork -- that comes
/// from the server like everything else -- only how far down it has got.
#[derive(Component)]
pub struct FallingBomb {
    released: f32,
}

/// The ring on the ground showing where a bomb released now would land.
#[derive(Component)]
pub struct BombSight;

#[derive(Resource)]
pub struct VehicleAssets {
    tank_chassis: Handle<Mesh>,
    tank_deck: Handle<Mesh>,
    tank_tread: Handle<Mesh>,
    tank_wheel: Handle<Mesh>,
    tank_turret: Handle<Mesh>,
    tank_cupola: Handle<Mesh>,
    tank_mantlet: Handle<Mesh>,
    tank_barrel: Handle<Mesh>,
    tank_muzzle: Handle<Mesh>,
    harvester_hull: Handle<Mesh>,
    harvester_bin: Handle<Mesh>,
    harvester_drum: Handle<Mesh>,
    harvester_tread: Handle<Mesh>,
    harvester_wheel: Handle<Mesh>,
    harvester_turret: Handle<Mesh>,
    harvester_barrel: Handle<Mesh>,
    plane_fuselage: Handle<Mesh>,
    plane_nose: Handle<Mesh>,
    plane_wing: Handle<Mesh>,
    plane_tailplane: Handle<Mesh>,
    plane_fin: Handle<Mesh>,
    plane_nacelle: Handle<Mesh>,
    plane_propeller: Handle<Mesh>,
    bomb: Handle<Mesh>,
    bombsight: Handle<Mesh>,
    shield: Handle<Mesh>,
    capture_bar: Handle<Mesh>,
    bullet: Handle<Mesh>,
    missile: Handle<Mesh>,
    /// Per-player body, trim, shield, and projectile materials.
    body: Vec<Handle<StandardMaterial>>,
    trim: Vec<Handle<StandardMaterial>>,
    shield_material: Vec<Handle<StandardMaterial>>,
    projectile: Vec<Handle<StandardMaterial>>,
    dark: Handle<StandardMaterial>,
    metal: Handle<StandardMaterial>,
    propeller: Handle<StandardMaterial>,
    /// Per-player bombsight rings.
    sight: Vec<Handle<StandardMaterial>>,
    capture_material: Handle<StandardMaterial>,
}

#[derive(Resource, Default)]
pub struct ViewRegistry {
    vehicles: HashMap<(u8, VehicleSlot), Entity>,
    projectiles: HashMap<u16, Entity>,
}

pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut body = Vec::new();
    let mut trim = Vec::new();
    let mut shield_material = Vec::new();
    let mut projectile = Vec::new();
    let mut sight = Vec::new();

    for id in 0..MAX_PLAYERS as u8 {
        let color = coords::player_color(id);
        body.push(materials.add(StandardMaterial {
            base_color: color,
            // Smoother than the scenery on purpose. A rounded hull only reads
            // as rounded when the sun draws a highlight that travels across
            // it, and at 0.55 the sheen was too diffuse to show the curve.
            perceptual_roughness: 0.42,
            metallic: 0.35,
            ..default()
        }));
        trim.push(materials.add(StandardMaterial {
            base_color: coords::player_color_dim(id, 0.55),
            perceptual_roughness: 0.7,
            metallic: 0.4,
            ..default()
        }));
        shield_material.push(materials.add(StandardMaterial {
            // The shields are the vehicles' defining feature, so they get a
            // visible bubble rather than only a HUD number.
            base_color: color.with_alpha(0.10),
            emissive: LinearRgba::from(color) * 0.22,
            alpha_mode: AlphaMode::Blend,
            double_sided: true,
            cull_mode: None,
            unlit: true,
            ..default()
        }));
        projectile.push(materials.add(StandardMaterial {
            base_color: color,
            emissive: LinearRgba::from(color) * 6.0,
            unlit: true,
            ..default()
        }));
        // The bombsight has to be legible against grass, ore and a base pad
        // alike, so it is unlit and glows rather than being lit like scenery.
        sight.push(materials.add(StandardMaterial {
            base_color: color.with_alpha(0.55),
            emissive: LinearRgba::from(color) * 2.2,
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            double_sided: true,
            cull_mode: None,
            ..default()
        }));
    }

    commands.insert_resource(VehicleAssets {
        // Every model is built facing +X, which is what `coords::yaw_to_quat`
        // assumes. The plated parts are [`rounded_box`]es rather than cuboids
        // and the turned parts are cylinders, but both hulls keep the footprint
        // and the ride height they had as crates, so nothing that was placed
        // against them has had to move.
        //
        // Every radius here is well short of the half extent it rounds. Taken
        // to the limit a rounded box is a pill, and a hull built out of pills
        // reads as inflated rather than moulded: the flat face that catches
        // the sun is what says "plate", and the bevel is only there to stop
        // it ending in a hard line.
        tank_chassis: meshes.add(rounded_box(Vec3::new(5.4, 1.1, 3.3), 0.26, 3)),
        tank_deck: meshes.add(rounded_box(Vec3::new(4.3, 1.0, 2.6), 0.24, 3)),
        tank_tread: meshes.add(rounded_box(Vec3::new(5.6, 0.85, 0.72), 0.3, 3)),
        tank_wheel: meshes.add(Cylinder::new(0.34, 0.22)),
        tank_turret: meshes.add(rounded_box(Vec3::new(2.6, 1.05, 2.2), 0.26, 3)),
        tank_cupola: meshes.add(Cylinder::new(0.4, 0.34)),
        tank_mantlet: meshes.add(Cylinder::new(0.4, 0.95)),
        tank_barrel: meshes.add(Cylinder::new(0.17, 3.4)),
        tank_muzzle: meshes.add(Cylinder::new(0.26, 0.5)),
        harvester_hull: meshes.add(rounded_box(Vec3::new(6.2, 1.4, 4.2), 0.3, 3)),
        harvester_bin: meshes.add(rounded_box(Vec3::new(4.0, 1.3, 3.4), 0.28, 3)),
        harvester_drum: meshes.add(Cylinder::new(0.8, 3.6)),
        harvester_tread: meshes.add(rounded_box(Vec3::new(6.0, 1.0, 0.85), 0.3, 3)),
        harvester_wheel: meshes.add(Cylinder::new(0.38, 0.24)),
        harvester_turret: meshes.add(rounded_box(Vec3::new(1.8, 0.7, 1.0), 0.2, 3)),
        harvester_barrel: meshes.add(Cylinder::new(0.12, 1.5)),
        // The bomber. Longer and far thinner than either ground vehicle, which
        // is most of what makes it read as an aircraft from above -- at chase
        // range the wing is the silhouette and everything else hangs off it.
        plane_fuselage: meshes.add(rounded_box(Vec3::new(7.4, 1.5, 1.5), 0.45, 3)),
        plane_nose: meshes.add(Cylinder::new(0.55, 1.2)),
        plane_wing: meshes.add(rounded_box(Vec3::new(2.0, 0.36, 10.5), 0.17, 2)),
        plane_tailplane: meshes.add(rounded_box(Vec3::new(1.2, 0.3, 4.0), 0.14, 2)),
        plane_fin: meshes.add(rounded_box(Vec3::new(1.3, 1.7, 0.3), 0.14, 2)),
        plane_nacelle: meshes.add(Cylinder::new(0.42, 2.2)),
        // A disc, for a propeller turning too fast to have blades.
        plane_propeller: meshes.add(Cylinder::new(0.95, 0.06)),
        bomb: meshes.add(rounded_box(Vec3::new(1.1, 0.5, 0.5), 0.25, 3)),
        // The sight: a ring on the ground the size of a blast.
        bombsight: meshes.add(Torus::new(sim::BOMB_BLAST_RADIUS - 0.5, sim::BOMB_BLAST_RADIUS)),
        shield: meshes.add(Sphere::new(1.0).mesh().uv(16, 10)),
        capture_bar: meshes.add(Cuboid::new(1.0, 0.45, 0.45)),
        bullet: meshes.add(Sphere::new(0.34).mesh().uv(8, 6)),
        missile: meshes.add(Cuboid::new(1.7, 0.36, 0.36)),
        body,
        trim,
        shield_material,
        projectile,
        sight,
        dark: materials.add(StandardMaterial {
            base_color: Color::srgb(0.14, 0.14, 0.16),
            perceptual_roughness: 0.85,
            ..default()
        }),
        // Running gear and gun fittings. Dark enough to stay undercarriage,
        // light enough to be seen against the tread it is bolted to -- in the
        // player's own colour these read as part of the hull rather than as
        // the machinery underneath it.
        metal: materials.add(StandardMaterial {
            base_color: Color::srgb(0.38, 0.39, 0.43),
            perceptual_roughness: 0.5,
            metallic: 0.65,
            ..default()
        }),
        // A propeller under power is a blur you see the field through, not a
        // disc. Drawn opaque these were three black holes in the middle of the
        // aircraft, which is the one thing a stylised model cannot carry.
        propeller: materials.add(StandardMaterial {
            base_color: Color::srgba(0.72, 0.75, 0.80, 0.3),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            double_sided: true,
            cull_mode: None,
            ..default()
        }),
        capture_material: materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.85, 0.2),
            emissive: LinearRgba::new(2.0, 1.5, 0.2, 1.0),
            unlit: true,
            ..default()
        }),
    });
    commands.insert_resource(ViewRegistry::default());
}

/// Spawns the one bombsight ring, hidden until something is flying.
pub fn setup_bombsight(mut commands: Commands, assets: Res<VehicleAssets>) {
    commands.spawn((
        BombSight,
        Mesh3d(assets.bombsight.clone()),
        MeshMaterial3d(assets.sight[0].clone()),
        Transform::default(),
        Visibility::Hidden,
        NotShadowCaster,
    ));
}

/// Puts the bombsight where a bomb released this instant would land.
///
/// Not a hint and not an estimate: it calls [`sim::bomb_impact`], which is the
/// same function the server's release goes through, on a speed that is a
/// constant both sides agree on. Bombing without it is guesswork -- from
/// [`sim::PLANE_ALTITUDE`] up, with the aircraft throwing each bomb some 47
/// units forward, there is nothing on screen to judge the lead against.
pub fn sync_bombsight(
    state: Res<GameState>,
    input: Res<LocalInput>,
    assets: Res<VehicleAssets>,
    mut sight: Query<
        (&mut Transform, &mut Visibility, &mut MeshMaterial3d<StandardMaterial>),
        With<BombSight>,
    >,
) {
    let Ok((mut transform, mut visibility, mut material)) = sight.single_mut() else { return };

    // Only while you are the one flying: it is an aiming aid, not a warning to
    // everybody else on the field about where the bombs are going.
    let aiming = state
        .local()
        .filter(|_| input.controlling == VehicleSlot::Plane)
        .and_then(|me| me.plane.map(|p| (me.id, p)));

    let Some((player, plane)) = aiming else {
        *visibility = Visibility::Hidden;
        return;
    };

    *visibility = Visibility::Visible;
    let idx = player as usize % MAX_PLAYERS;
    if material.0 != assets.sight[idx] {
        material.0 = assets.sight[idx].clone();
    }
    let at = sim::bomb_impact(plane.pos, plane.yaw, sim::PLANE_CRUISE);
    // Just clear of the ground so the ring does not fight the grass for depth.
    transform.translation = coords::sim_to_world_at(at, 0.12);
}

fn spawn_vehicle(
    commands: &mut Commands,
    assets: &VehicleAssets,
    player: u8,
    slot: VehicleSlot,
) -> Entity {
    let idx = player as usize % MAX_PLAYERS;
    let radius = sim::tuning(slot.kind()).radius;

    let root = commands.spawn((VehicleView { player, slot }, Transform::default(), Visibility::Visible)).id();

    match slot {
        VehicleSlot::Tank => {
            commands.entity(root).with_children(|parent| {
                parent.spawn((
                    Mesh3d(assets.tank_chassis.clone()),
                    MeshMaterial3d(assets.body[idx].clone()),
                    Transform::from_xyz(0.0, 1.05, 0.0),
                ));
                // A second, narrower deck set back over the chassis. One slab
                // of hull reads as a crate however round its edges are; the
                // step gives the silhouette a shoulder and a shadow line.
                //
                // Sunk far enough in that its bevel turns vertical well below
                // the chassis roof it passes through. A bevel that goes
                // vertical *at* another part's flat face meets it tangentially
                // -- the two stay within a hair of each other across a wide
                // band of pixels -- and the seam boils. The same clearance is
                // why the turret is rounded less than it could be.
                parent.spawn((
                    Mesh3d(assets.tank_deck.clone()),
                    MeshMaterial3d(assets.body[idx].clone()),
                    Transform::from_xyz(-0.15, 1.62, 0.0),
                ));
                for z in [-1.76, 1.76] {
                    parent.spawn((
                        Mesh3d(assets.tank_tread.clone()),
                        MeshMaterial3d(assets.dark.clone()),
                        Transform::from_xyz(0.0, 0.55, z),
                    ));
                    // Road wheels, standing just proud of the tread's outer
                    // face. Without them the tread is a dark bar that gives no
                    // sense of the tank rolling over the ground.
                    for x in [-1.85, 0.0, 1.85] {
                        parent.spawn((
                            Mesh3d(assets.tank_wheel.clone()),
                            MeshMaterial3d(assets.metal.clone()),
                            Transform::from_xyz(x, 0.5, z * 1.2).with_rotation(across_z()),
                        ));
                    }
                }
                // Turret is a child so it can be aimed independently of the hull.
                parent
                    .spawn((TurretView, Transform::from_xyz(0.0, 2.15, 0.0), Visibility::Inherited))
                    .with_children(|turret| {
                        turret.spawn((
                            Mesh3d(assets.tank_turret.clone()),
                            MeshMaterial3d(assets.trim[idx].clone()),
                            Transform::default(),
                        ));
                        turret.spawn((
                            Mesh3d(assets.tank_cupola.clone()),
                            MeshMaterial3d(assets.metal.clone()),
                            Transform::from_xyz(-0.5, 0.6, 0.45),
                        ));
                        // The mantlet covers the join, where a bare barrel used
                        // to emerge from the middle of a flat face.
                        turret.spawn((
                            Mesh3d(assets.tank_mantlet.clone()),
                            MeshMaterial3d(assets.metal.clone()),
                            Transform::from_xyz(1.2, 0.0, 0.0).with_rotation(across_z()),
                        ));
                        // The tip stays at x = 4.1, where the square barrel
                        // ended, so the gun reaches exactly as far as it looked
                        // like it did before.
                        turret.spawn((
                            Mesh3d(assets.tank_barrel.clone()),
                            MeshMaterial3d(assets.dark.clone()),
                            Transform::from_xyz(2.4, 0.0, 0.0).with_rotation(along_x()),
                        ));
                        turret.spawn((
                            Mesh3d(assets.tank_muzzle.clone()),
                            MeshMaterial3d(assets.metal.clone()),
                            Transform::from_xyz(3.85, 0.0, 0.0).with_rotation(along_x()),
                        ));
                    });
            });
        }
        VehicleSlot::Plane => {
            commands.entity(root).with_children(|parent| {
                parent.spawn((
                    Mesh3d(assets.plane_fuselage.clone()),
                    MeshMaterial3d(assets.body[idx].clone()),
                    Transform::from_xyz(0.0, 0.0, 0.0),
                ));
                // A rounded nose. No propeller on it: the engines are on the
                // wing, and from directly behind -- which is where the chase
                // camera lives and the only place this is ever seen from -- a
                // third disc sits squarely over the fuselage.
                parent.spawn((
                    Mesh3d(assets.plane_nose.clone()),
                    MeshMaterial3d(assets.metal.clone()),
                    Transform::from_xyz(3.6, 0.0, 0.0).with_rotation(along_x()),
                ));
                // The wing. Set slightly forward of the middle, which is where
                // an eye expects it, and thin enough to read as a wing rather
                // than as a plank.
                parent.spawn((
                    Mesh3d(assets.plane_wing.clone()),
                    MeshMaterial3d(assets.body[idx].clone()),
                    Transform::from_xyz(0.5, 0.35, 0.0),
                ));
                // Engines slung under it, with their own propellers: two of
                // them is what says "bomber" rather than "fighter".
                for z in [-2.6, 2.6] {
                    parent.spawn((
                        Mesh3d(assets.plane_nacelle.clone()),
                        MeshMaterial3d(assets.metal.clone()),
                        Transform::from_xyz(0.7, 0.0, z).with_rotation(along_x()),
                    ));
                    parent.spawn((
                        Mesh3d(assets.plane_propeller.clone()),
                        MeshMaterial3d(assets.propeller.clone()),
                        Transform::from_xyz(1.95, 0.0, z).with_rotation(along_x()),
                        NotShadowCaster,
                    ));
                }
                // Tail: a horizontal plane and a fin, in the trim colour so the
                // back end is legible against the wing from directly behind,
                // which is exactly where the chase camera sits.
                parent.spawn((
                    Mesh3d(assets.plane_tailplane.clone()),
                    MeshMaterial3d(assets.trim[idx].clone()),
                    Transform::from_xyz(-3.1, 0.25, 0.0),
                ));
                parent.spawn((
                    Mesh3d(assets.plane_fin.clone()),
                    MeshMaterial3d(assets.trim[idx].clone()),
                    Transform::from_xyz(-3.1, 1.1, 0.0),
                ));
            });
        }
        VehicleSlot::Harvester => {
            commands.entity(root).with_children(|parent| {
                parent.spawn((
                    Mesh3d(assets.harvester_hull.clone()),
                    MeshMaterial3d(assets.body[idx].clone()),
                    Transform::from_xyz(0.0, 1.35, 0.0),
                ));
                // The ore bin, riding high and to the rear. This is the part
                // that tells a harvester from a tank at chase-camera range, so
                // it takes the trim colour and the tallest line on the hull.
                // Sunk clear of the hull roof, for the reason given above the
                // tank's deck.
                parent.spawn((
                    Mesh3d(assets.harvester_bin.clone()),
                    MeshMaterial3d(assets.trim[idx].clone()),
                    Transform::from_xyz(-0.8, 2.0, 0.0),
                ));
                // A cutting drum across the bow, running down to where the
                // treads meet the ground. The flat blade it replaces read as a
                // plough at best and as a wall at worst; a drum reads as
                // something that digs.
                //
                // Sized so its crown stays below where the hull's front bevel
                // begins, which leaves it emerging through the flat face of
                // the bow -- a clean perpendicular cut. Any taller and the top
                // of the drum runs along the inside of that bevel a hundredth
                // of a unit away before breaking through it, which is the
                // tangential seam described above the tank's deck.
                parent.spawn((
                    Mesh3d(assets.harvester_drum.clone()),
                    MeshMaterial3d(assets.metal.clone()),
                    Transform::from_xyz(2.95, 0.9, 0.0).with_rotation(across_z()),
                ));
                for z in [-2.1, 2.1] {
                    parent.spawn((
                        Mesh3d(assets.harvester_tread.clone()),
                        MeshMaterial3d(assets.dark.clone()),
                        Transform::from_xyz(0.0, 0.6, z),
                    ));
                    for x in [-2.0, 0.0, 2.0] {
                        parent.spawn((
                            Mesh3d(assets.harvester_wheel.clone()),
                            MeshMaterial3d(assets.metal.clone()),
                            Transform::from_xyz(x, 0.55, z * 1.2).with_rotation(across_z()),
                        ));
                    }
                }
                parent
                    .spawn((TurretView, Transform::from_xyz(0.0, 2.9, 0.0), Visibility::Inherited))
                    .with_children(|turret| {
                        turret.spawn((
                            Mesh3d(assets.harvester_turret.clone()),
                            MeshMaterial3d(assets.dark.clone()),
                            Transform::default(),
                        ));
                        // A stub gun, so which way the auto-turret is looking
                        // is legible from the side and not only head on.
                        turret.spawn((
                            Mesh3d(assets.harvester_barrel.clone()),
                            MeshMaterial3d(assets.dark.clone()),
                            Transform::from_xyz(1.5, 0.0, 0.0).with_rotation(along_x()),
                        ));
                    });
                parent.spawn((
                    CaptureBar,
                    Mesh3d(assets.capture_bar.clone()),
                    MeshMaterial3d(assets.capture_material.clone()),
                    Transform::from_xyz(0.0, 6.0, 0.0),
                    Visibility::Hidden,
                    NotShadowCaster,
                ));
            });
        }
    }

    // A shield bubble on everything that has a shield. The aircraft has none
    // -- nothing can shoot it -- so it gets no bubble rather than a bubble that
    // is always empty.
    if slot == VehicleSlot::Plane {
        return root;
    }

    commands.entity(root).with_children(|parent| {
        parent.spawn((
            ShieldBubble,
            Mesh3d(assets.shield.clone()),
            MeshMaterial3d(assets.shield_material[idx].clone()),
            Transform::from_xyz(0.0, 1.6, 0.0).with_scale(Vec3::splat(radius * 1.35)),
            Visibility::Visible,
            // A translucent shell has no business casting a solid shadow,
            // and keeping these double-sided, cull-disabled spheres out of the
            // shadow pass saves a draw per vehicle for nothing lost.
            NotShadowCaster,
        ));
    });

    root
}

/// Creates, moves, and removes vehicle views to match the render world.
pub fn sync_vehicles(
    mut commands: Commands,
    assets: Res<VehicleAssets>,
    mut registry: ResMut<ViewRegistry>,
    state: Res<GameState>,
    mut roots: Query<(Entity, &VehicleView, &mut Transform), Without<TurretView>>,
    mut turrets: Query<(&ChildOf, &mut Transform), (With<TurretView>, Without<VehicleView>)>,
    mut bubbles: Query<
        (&ChildOf, &mut Transform, &mut Visibility),
        (With<ShieldBubble>, Without<TurretView>, Without<VehicleView>),
    >,
    mut bars: Query<
        (&ChildOf, &mut Transform, &mut Visibility),
        (With<CaptureBar>, Without<ShieldBubble>, Without<TurretView>, Without<VehicleView>),
    >,
) {
    // What should exist this frame.
    let mut wanted: HashMap<(u8, VehicleSlot), RenderVehicle> = HashMap::new();
    for player in state.render.players.iter().flatten() {
        if let Some(v) = player.tank {
            wanted.insert((player.id, VehicleSlot::Tank), v);
        }
        if let Some(v) = player.harvester {
            wanted.insert((player.id, VehicleSlot::Harvester), v);
        }
        if let Some(v) = player.plane {
            wanted.insert((player.id, VehicleSlot::Plane), v);
        }
    }

    // Retire views for vehicles that were destroyed or captured.
    registry.vehicles.retain(|key, entity| {
        if wanted.contains_key(key) {
            true
        } else {
            commands.entity(*entity).despawn();
            false
        }
    });

    for key in wanted.keys() {
        registry
            .vehicles
            .entry(*key)
            .or_insert_with(|| spawn_vehicle(&mut commands, &assets, key.0, key.1));
    }

    // Position the roots, recording what each entity turned out to be so the
    // child parts below can find their vehicle without re-querying.
    let mut resolved: HashMap<Entity, (u8, VehicleSlot, RenderVehicle)> = HashMap::new();
    for (entity, view, mut transform) in &mut roots {
        let Some(v) = wanted.get(&(view.player, view.slot)) else { continue };
        transform.translation = coords::sim_to_world(v.pos);
        if view.slot == VehicleSlot::Plane {
            transform.translation.y += sim::PLANE_ALTITUDE;
        }
        transform.rotation = coords::yaw_to_quat(v.yaw);
        if view.slot == VehicleSlot::Plane {
            // Rolled about its own nose-to-tail axis, applied after the
            // heading so it turns in the model's own frame. The bank is the
            // control the player is actually working, and seeing it is how they
            // know how hard the aircraft is coming round -- so the wing that
            // drops has to be the one on the inside of the turn. Banking left
            // puts the left wing down.
            //
            // The sign is the subtle part, for the same reason it is subtle in
            // `coords::yaw_to_quat`: a positive rotation about the model's
            // nose carries its roof toward the wing that is *rising*, so the
            // roll goes in as it is rather than negated.
            transform.rotation *= Quat::from_rotation_x(v.roll);
        }
        // A disabled harvester sits low and canted, so it reads as a wreck.
        if v.disabled {
            transform.translation.y -= 0.45;
            transform.rotation *= Quat::from_rotation_z(0.13);
        }
        resolved.insert(entity, (view.player, view.slot, *v));
    }

    // Turrets rotate relative to the hull, so subtract the hull heading.
    for (parent, mut transform) in &mut turrets {
        let Some((_, _, v)) = resolved.get(&parent.parent()) else { continue };
        transform.rotation = coords::yaw_to_quat(v.turret_yaw - v.yaw);
    }

    for (parent, mut transform, mut visibility) in &mut bubbles {
        let Some((player, slot, v)) = resolved.get(&parent.parent()) else { continue };
        let kind = match slot {
            VehicleSlot::Tank => VehicleKind::Tank,
            VehicleSlot::Harvester => VehicleKind::Harvester,
            // Unreachable: `spawn_vehicle` gives the aircraft no bubble to find.
            VehicleSlot::Plane => VehicleKind::Plane,
        };
        let powerups = state
            .render
            .players
            .get(*player as usize)
            .and_then(|p| p.as_ref())
            .map_or(0, |p| p.powerups);
        let max = sim::max_shield(kind, powerups).max(1.0);
        let fraction = (v.shield / max).clamp(0.0, 1.0);
        *visibility = if fraction > 0.02 { Visibility::Visible } else { Visibility::Hidden };
        let radius = sim::tuning(kind).radius;
        transform.scale = Vec3::splat(radius * (1.12 + fraction * 0.26));
    }

    for (parent, mut transform, mut visibility) in &mut bars {
        let Some((_, _, v)) = resolved.get(&parent.parent()) else { continue };
        *visibility = if v.disabled { Visibility::Visible } else { Visibility::Hidden };
        // Scale along the model's X so it fills up left to right.
        transform.scale = Vec3::new((v.capture_progress.clamp(0.0, 1.0) * 7.0).max(0.05), 1.0, 1.0);
    }
}

/// Creates and moves projectile views.
pub fn sync_projectiles(
    mut commands: Commands,
    assets: Res<VehicleAssets>,
    mut registry: ResMut<ViewRegistry>,
    state: Res<GameState>,
    time: Res<Time>,
    mut views: Query<(&mut Transform, Option<&FallingBomb>), With<ProjectileView>>,
) {
    let live: HashMap<u16, _> = state.render.projectiles.iter().map(|p| (p.id, p)).collect();

    registry.projectiles.retain(|id, entity| {
        if live.contains_key(id) {
            true
        } else {
            commands.entity(*entity).despawn();
            false
        }
    });

    let now = time.elapsed_secs();
    for projectile in &state.render.projectiles {
        let idx = projectile.owner as usize % MAX_PLAYERS;
        let entity = *registry.projectiles.entry(projectile.id).or_insert_with(|| {
            let mesh = match projectile.kind {
                ProjectileKind::Bullet => assets.bullet.clone(),
                ProjectileKind::Missile => assets.missile.clone(),
                ProjectileKind::Bomb => assets.bomb.clone(),
            };
            // A bomb is a falling object rather than a tracer, so it is lit and
            // painted like a piece of ordnance instead of glowing.
            let material = match projectile.kind {
                ProjectileKind::Bomb => assets.dark.clone(),
                _ => assets.projectile[idx].clone(),
            };
            let mut view = commands.spawn((
                ProjectileView,
                Mesh3d(mesh),
                MeshMaterial3d(material),
                Transform::from_translation(coords::sim_to_world_at(projectile.pos, 2.2)),
                // Unlit tracers; shadowing them costs a shadow-pass draw
                // per bullet for something nobody would see.
                NotShadowCaster,
            ));
            if projectile.kind == ProjectileKind::Bomb {
                view.insert(FallingBomb { released: now });
            }
            view.id()
        });
        if let Ok((mut transform, falling)) = views.get_mut(entity) {
            let height = match projectile.kind {
                ProjectileKind::Bullet => 2.2,
                ProjectileKind::Missile => 2.4,
                // How far it has left to fall. The snapshot has no room to
                // carry a height, so it is taken from how long this client has
                // been watching this bomb -- which is the same thing for every
                // bomb it sees released. One seen for the first time already
                // part-way down starts again from the top: that needs a bomb to
                // have been culled from a full projectile list and then come
                // back inside it mid-fall, and it is over in a second either way.
                ProjectileKind::Bomb => {
                    let age = falling.map_or(0.0, |f| now - f.released);
                    let fallen = (age / sim::BOMB_FALL_TIME).clamp(0.0, 1.0);
                    0.6 + (sim::PLANE_ALTITUDE - 0.6) * (1.0 - fallen)
                }
            };
            transform.translation = coords::sim_to_world_at(projectile.pos, height);
            transform.rotation = coords::yaw_to_quat(projectile.yaw);
            if projectile.kind == ProjectileKind::Bomb {
                // Nose-down as it goes, so a stick of them reads as falling
                // rather than as a line of boxes sliding downward.
                let age = falling.map_or(0.0, |f| now - f.released);
                let tipped = (age / sim::BOMB_FALL_TIME).clamp(0.0, 1.0);
                transform.rotation *= Quat::from_rotation_z(-tipped * 1.1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Model building
// ---------------------------------------------------------------------------

/// Lays a cylinder along the model's forward axis, for barrels. Bevy's stand
/// on Y, and every model here is built facing +X.
fn along_x() -> Quat {
    Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2)
}

/// Lays a cylinder across the model, for road wheels and the cutting drum.
fn across_z() -> Quat {
    Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)
}

/// A box with its edges and corners turned over. Sized like [`Cuboid`]: `size`
/// is the full extent it fills, and `radius` is how much of each edge is
/// rounded away.
///
/// Bevy has no such primitive, and without one the vehicles read as crates. A
/// sharp edge takes the sun as two flat sheets meeting at a hard line, where a
/// rounded one carries a highlight along its length, and that highlight is
/// most of what makes a shape look moulded rather than stacked.
///
/// The surface is a sphere of `radius` swept around an inner box of half
/// extents `size / 2 - radius`, so every point on it is `radius` out from that
/// box along its own normal. It is meshed as a latitude/longitude grid whose
/// rows and columns are *duplicated* at each quadrant boundary, each copy
/// carrying its own octant's corner offset. That duplication is what keeps the
/// flat faces flat: the strip between a duplicated pair spans the gap between
/// two corner offsets -- a face, or a quarter-cylinder edge -- while both
/// copies share one normal, so nothing bulges across it. Only the two poles
/// are left over, where the grid collapses onto the rectangle of corner
/// points, so the top and bottom faces are filled in afterwards.
pub fn rounded_box(size: Vec3, radius: f32, segments: usize) -> Mesh {
    let segments = segments.max(1);
    // Rounding deeper than an axis can carry would turn the inner box inside
    // out. Clamped, a thin part flattens into a pill, which is what it should
    // look like anyway.
    let radius = radius.clamp(0.0, size.min_element() * 0.5);
    let h = (size * 0.5 - Vec3::splat(radius)).max(Vec3::ZERO);
    let quarter = std::f32::consts::FRAC_PI_2;

    // Latitude, pole to pole, with the equator sampled twice: once belonging
    // to the lower half of the box, once to the upper.
    let mut rows: Vec<(f32, f32)> = Vec::new();
    for (half, sy) in [(0.0, -1.0), (1.0, 1.0)] {
        for j in 0..=segments {
            rows.push((quarter * (half - 1.0 + j as f32 / segments as f32), sy));
        }
    }
    // Longitude, the full turn in four quadrants, each sampled inclusively so
    // that every boundary appears twice for the same reason.
    let mut cols: Vec<(f32, f32, f32)> = Vec::new();
    for q in 0..4 {
        let sx = if q == 0 || q == 3 { 1.0 } else { -1.0 };
        let sz = if q < 2 { 1.0 } else { -1.0 };
        for i in 0..=segments {
            cols.push((quarter * (q as f32 + i as f32 / segments as f32), sx, sz));
        }
    }

    let vertex = |(phi, sy): (f32, f32), (theta, sx, sz): (f32, f32, f32)| {
        let n = Vec3::new(phi.cos() * theta.cos(), phi.sin(), phi.cos() * theta.sin());
        (Vec3::new(h.x * sx, h.y * sy, h.z * sz) + n * radius, n)
    };

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut tri = |a: (Vec3, Vec3), b: (Vec3, Vec3), c: (Vec3, Vec3)| {
        // Within one octant the pole row is a single corner point repeated, so
        // part of every strip that reaches a pole has no area.
        if a.0 == b.0 || b.0 == c.0 || c.0 == a.0 {
            return;
        }
        // Wound counter-clockwise seen from outside, which is the way round
        // Bevy treats as front facing.
        let (b, c) = if (b.0 - a.0).cross(c.0 - a.0).dot(a.1 + b.1 + c.1) < 0.0 {
            (c, b)
        } else {
            (b, c)
        };
        for (p, n) in [a, b, c] {
            positions.push(p.to_array());
            normals.push(n.to_array());
            // Nothing built from this is textured.
            uvs.push([0.0, 0.0]);
        }
    };

    for r in 0..rows.len() - 1 {
        for c in 0..cols.len() {
            let next = (c + 1) % cols.len();
            let a = vertex(rows[r], cols[c]);
            let b = vertex(rows[r], cols[next]);
            let d = vertex(rows[r + 1], cols[next]);
            let e = vertex(rows[r + 1], cols[c]);
            tri(a, b, d);
            tri(a, d, e);
        }
    }

    // The caps the collapsed poles left behind.
    for sy in [-1.0f32, 1.0] {
        let n = Vec3::new(0.0, sy, 0.0);
        let y = (h.y + radius) * sy;
        let corner = |sx: f32, sz: f32| (Vec3::new(h.x * sx, y, h.z * sz), n);
        tri(corner(-1.0, -1.0), corner(1.0, -1.0), corner(1.0, 1.0));
        tri(corner(-1.0, -1.0), corner(1.0, 1.0), corner(-1.0, 1.0));
    }

    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::mesh::{MeshVertexAttribute, VertexAttributeValues};

    fn vec3s_of(mesh: &Mesh, attribute: MeshVertexAttribute) -> Vec<Vec3> {
        match mesh.attribute(attribute).expect("the box has this attribute") {
            VertexAttributeValues::Float32x3(v) => v.iter().copied().map(Vec3::from_array).collect(),
            other => panic!("unexpected format: {other:?}"),
        }
    }

    /// A rounded box is sized the way a `Cuboid` is: `size` is what it fills.
    ///
    /// Every placement in `spawn_vehicle` is worked out against these extents
    /// -- a tread tucked against a hull, a drum set at the bow, a deck stepped
    /// in from the chassis below it. A mesh that quietly ran over or under the
    /// size it was asked for would push those parts through each other, and
    /// the numbers here would stop meaning anything.
    #[test]
    fn a_rounded_box_fills_exactly_the_size_it_is_given() {
        let size = Vec3::new(5.4, 1.0, 3.3);
        let mesh = rounded_box(size, 0.45, 3);
        let positions = vec3s_of(&mesh, Mesh::ATTRIBUTE_POSITION);
        assert!(!positions.is_empty(), "the box has triangles");

        // The furthest any vertex gets along each axis, which has to be the
        // half extent exactly: no less, or the box is undersized, and no more,
        // or it escapes the size it claims.
        let reach = positions.iter().fold(Vec3::ZERO, |reach, p| reach.max(p.abs()));
        for axis in 0..3 {
            assert!(
                (reach[axis] - size[axis] * 0.5).abs() < 1e-4,
                "axis {axis} reaches {} of a half extent of {}",
                reach[axis],
                size[axis] * 0.5,
            );
        }
    }

    /// What rounds the edges is the normals as much as the positions: every
    /// surface point sits `radius` out from the inner box along its own
    /// normal. Lose that and the rounding either shades as the hard crease it
    /// was built to get rid of, or bulges the flat faces into a cushion.
    #[test]
    fn a_rounded_box_is_a_sphere_swept_over_an_inner_box() {
        let size = Vec3::new(3.0, 2.0, 4.0);
        let radius = 0.6;
        let mesh = rounded_box(size, radius, 3);
        let inner = size * 0.5 - Vec3::splat(radius);

        let positions = vec3s_of(&mesh, Mesh::ATTRIBUTE_POSITION);
        let normals = vec3s_of(&mesh, Mesh::ATTRIBUTE_NORMAL);
        assert_eq!(positions.len(), normals.len(), "one normal per position");
        for (p, n) in positions.iter().zip(normals) {
            assert!((n.length() - 1.0).abs() < 1e-3, "{n} is not a unit normal");
            let out = *p - p.clamp(-inner, inner);
            assert!(
                (out - n * radius).length() < 1e-4,
                "{p} stands {out} off the inner box, not {radius} along {n}",
            );
        }
    }

    /// Asking a thin part for deeper rounding than it can carry has to flatten
    /// it into a pill, not fold it inside out. The treads sit close enough to
    /// that limit that it is worth pinning down.
    #[test]
    fn rounding_deeper_than_the_box_is_clamped_to_it() {
        let size = Vec3::new(4.0, 0.5, 1.0);
        let mesh = rounded_box(size, 2.0, 3);
        for p in vec3s_of(&mesh, Mesh::ATTRIBUTE_POSITION) {
            for axis in 0..3 {
                assert!(
                    p[axis].abs() <= size[axis] * 0.5 + 1e-4,
                    "{p} escapes a box of {size}",
                );
            }
        }
    }
}
