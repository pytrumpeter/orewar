//! Spawns and updates the 3D views of vehicles and projectiles.
//!
//! Nothing here simulates anything. Every frame it reconciles the set of
//! entities on screen against [`GameState::render`], creating what is new,
//! removing what is gone, and moving the rest.

use std::collections::HashMap;

use bevy::light::NotShadowCaster;
use bevy::prelude::*;
use orewar_shared::protocol::{ProjectileKind, VehicleSlot};
use orewar_shared::sim::{self, VehicleKind};
use orewar_shared::world::MAX_PLAYERS;

use crate::coords;
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

#[derive(Resource)]
pub struct VehicleAssets {
    tank_hull: Handle<Mesh>,
    tank_turret: Handle<Mesh>,
    tank_barrel: Handle<Mesh>,
    harvester_hull: Handle<Mesh>,
    harvester_scoop: Handle<Mesh>,
    harvester_turret: Handle<Mesh>,
    tread: Handle<Mesh>,
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

    for id in 0..MAX_PLAYERS as u8 {
        let color = coords::player_color(id);
        body.push(materials.add(StandardMaterial {
            base_color: color,
            perceptual_roughness: 0.55,
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
    }

    commands.insert_resource(VehicleAssets {
        // Every model is built facing +X, which is what `coords::yaw_to_quat`
        // assumes.
        tank_hull: meshes.add(Cuboid::new(5.4, 1.5, 3.4)),
        tank_turret: meshes.add(Cuboid::new(2.6, 1.1, 2.2)),
        tank_barrel: meshes.add(Cuboid::new(3.4, 0.38, 0.38)),
        harvester_hull: meshes.add(Cuboid::new(6.2, 2.1, 4.2)),
        harvester_scoop: meshes.add(Cuboid::new(1.3, 1.7, 4.8)),
        harvester_turret: meshes.add(Cuboid::new(1.8, 0.7, 1.0)),
        tread: meshes.add(Cuboid::new(5.6, 0.8, 0.9)),
        shield: meshes.add(Sphere::new(1.0).mesh().uv(16, 10)),
        capture_bar: meshes.add(Cuboid::new(1.0, 0.45, 0.45)),
        bullet: meshes.add(Sphere::new(0.34).mesh().uv(8, 6)),
        missile: meshes.add(Cuboid::new(1.7, 0.36, 0.36)),
        body,
        trim,
        shield_material,
        projectile,
        dark: materials.add(StandardMaterial {
            base_color: Color::srgb(0.14, 0.14, 0.16),
            perceptual_roughness: 0.85,
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
                    Mesh3d(assets.tank_hull.clone()),
                    MeshMaterial3d(assets.body[idx].clone()),
                    Transform::from_xyz(0.0, 1.25, 0.0),
                ));
                for z in [-1.75, 1.75] {
                    parent.spawn((
                        Mesh3d(assets.tread.clone()),
                        MeshMaterial3d(assets.dark.clone()),
                        Transform::from_xyz(0.0, 0.55, z),
                    ));
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
                            Mesh3d(assets.tank_barrel.clone()),
                            MeshMaterial3d(assets.dark.clone()),
                            Transform::from_xyz(2.4, 0.0, 0.0),
                        ));
                    });
            });
        }
        VehicleSlot::Harvester => {
            commands.entity(root).with_children(|parent| {
                parent.spawn((
                    Mesh3d(assets.harvester_hull.clone()),
                    MeshMaterial3d(assets.body[idx].clone()),
                    Transform::from_xyz(0.0, 1.6, 0.0),
                ));
                parent.spawn((
                    Mesh3d(assets.harvester_scoop.clone()),
                    MeshMaterial3d(assets.trim[idx].clone()),
                    Transform::from_xyz(3.4, 0.9, 0.0),
                ));
                for z in [-2.15, 2.15] {
                    parent.spawn((
                        Mesh3d(assets.tread.clone()),
                        MeshMaterial3d(assets.dark.clone()),
                        Transform::from_xyz(0.0, 0.55, z),
                    ));
                }
                parent
                    .spawn((TurretView, Transform::from_xyz(0.0, 2.9, 0.0), Visibility::Inherited))
                    .with_children(|turret| {
                        turret.spawn((
                            Mesh3d(assets.harvester_turret.clone()),
                            MeshMaterial3d(assets.dark.clone()),
                            Transform::default(),
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
        transform.rotation = coords::yaw_to_quat(v.yaw);
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
    mut views: Query<&mut Transform, With<ProjectileView>>,
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

    for projectile in &state.render.projectiles {
        let idx = projectile.owner as usize % MAX_PLAYERS;
        let entity = *registry.projectiles.entry(projectile.id).or_insert_with(|| {
            let (mesh, height) = match projectile.kind {
                ProjectileKind::Bullet => (assets.bullet.clone(), 2.2),
                ProjectileKind::Missile => (assets.missile.clone(), 2.4),
            };
            commands
                .spawn((
                    ProjectileView,
                    Mesh3d(mesh),
                    MeshMaterial3d(assets.projectile[idx].clone()),
                    Transform::from_translation(coords::sim_to_world_at(projectile.pos, height)),
                    // Unlit tracers; shadowing them costs a shadow-pass draw
                    // per bullet for something nobody would see.
                    NotShadowCaster,
                ))
                .id()
        });
        if let Ok(mut transform) = views.get_mut(entity) {
            let height = match projectile.kind {
                ProjectileKind::Bullet => 2.2,
                ProjectileKind::Missile => 2.4,
            };
            transform.translation = coords::sim_to_world_at(projectile.pos, height);
            transform.rotation = coords::yaw_to_quat(projectile.yaw);
        }
    }
}
