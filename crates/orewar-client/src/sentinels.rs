//! Draws the gun emplacement standing in each player's home corner.
//!
//! Simpler than [`crate::vehicles`], because a sentinel never moves. There is
//! one per player slot at a position fixed by [`world::sentinel_position`], so
//! all four views are spawned once at startup and only ever change what they
//! show: which way the gun is pointing, and whether it is standing at all.

use std::collections::HashMap;

use bevy::prelude::*;
use orewar_shared::world::{self, MAX_PLAYERS};

use crate::coords;
use crate::state::{GameState, RenderSentinel};

/// The emplacement belonging to one player slot.
#[derive(Component)]
pub struct SentinelView(pub u8);

/// The part that turns.
#[derive(Component)]
pub struct SentinelTurret;

/// What is shown in its place once it has been shot down.
#[derive(Component)]
pub struct SentinelRubble;

/// How tall the plinth stands. Enough to read as a structure next to a tank
/// without blocking the view out of your own corner.
const PLINTH_HEIGHT: f32 = 1.9;
const PLINTH_RADIUS: f32 = 2.2;

#[derive(Resource)]
pub struct SentinelAssets {
    plinth: Handle<Mesh>,
    housing: Handle<Mesh>,
    barrel: Handle<Mesh>,
    rubble: Handle<Mesh>,
    /// Per-player, so an emplacement reads as belonging to whoever's corner it
    /// is standing in.
    body: Vec<Handle<StandardMaterial>>,
    trim: Vec<Handle<StandardMaterial>>,
    dark: Handle<StandardMaterial>,
    rubble_material: Handle<StandardMaterial>,
}

pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut body = Vec::new();
    let mut trim = Vec::new();
    for id in 0..MAX_PLAYERS as u8 {
        body.push(materials.add(StandardMaterial {
            base_color: coords::player_color_dim(id, 0.8),
            perceptual_roughness: 0.7,
            metallic: 0.3,
            ..default()
        }));
        trim.push(materials.add(StandardMaterial {
            base_color: coords::player_color(id),
            perceptual_roughness: 0.5,
            metallic: 0.45,
            ..default()
        }));
    }

    let assets = SentinelAssets {
        plinth: meshes.add(Cylinder::new(PLINTH_RADIUS, PLINTH_HEIGHT)),
        housing: meshes.add(Cuboid::new(2.4, 1.1, 2.0)),
        barrel: meshes.add(Cuboid::new(3.2, 0.42, 0.42)),
        // Sunk low and wide: a heap where the gun was.
        rubble: meshes.add(Cylinder::new(PLINTH_RADIUS * 1.1, 0.5)),
        body,
        trim,
        dark: materials.add(StandardMaterial {
            base_color: Color::srgb(0.16, 0.17, 0.19),
            perceptual_roughness: 0.85,
            ..default()
        }),
        rubble_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.28, 0.26, 0.24),
            perceptual_roughness: 0.95,
            ..default()
        }),
    };

    // One per slot, whether or not anyone is in it yet. Hidden until a snapshot
    // says otherwise, so an empty slot shows nothing.
    for id in 0..MAX_PLAYERS as u8 {
        let pos = world::sentinel_position(id);
        commands
            .spawn((
                SentinelView(id),
                Transform::from_translation(coords::sim_to_world(pos)),
                Visibility::Hidden,
            ))
            .with_children(|parent| {
                parent.spawn((
                    Mesh3d(assets.plinth.clone()),
                    MeshMaterial3d(assets.body[id as usize].clone()),
                    Transform::from_xyz(0.0, PLINTH_HEIGHT * 0.5, 0.0),
                ));
                // The turret is a child so that rotating it leaves the plinth
                // where it is, exactly as a tank's turret rides its hull.
                parent
                    .spawn((
                        SentinelTurret,
                        Transform::from_xyz(0.0, PLINTH_HEIGHT + 0.4, 0.0),
                        Visibility::Inherited,
                    ))
                    .with_children(|turret| {
                        turret.spawn((
                            Mesh3d(assets.housing.clone()),
                            MeshMaterial3d(assets.trim[id as usize].clone()),
                            Transform::default(),
                        ));
                        // Model +X is the firing direction, matching
                        // `yaw_to_quat` and the muzzle the server spawns from.
                        turret.spawn((
                            Mesh3d(assets.barrel.clone()),
                            MeshMaterial3d(assets.dark.clone()),
                            Transform::from_xyz(1.9, 0.0, 0.0),
                        ));
                    });
                parent.spawn((
                    SentinelRubble,
                    Mesh3d(assets.rubble.clone()),
                    MeshMaterial3d(assets.rubble_material.clone()),
                    Transform::from_xyz(0.0, 0.25, 0.0),
                    Visibility::Hidden,
                ));
            });
    }

    commands.insert_resource(assets);
}

pub fn sync(
    state: Res<GameState>,
    mut roots: Query<(Entity, &SentinelView, &mut Visibility), Without<SentinelTurret>>,
    mut turrets: Query<
        (&ChildOf, &mut Transform, &mut Visibility),
        (With<SentinelTurret>, Without<SentinelRubble>, Without<SentinelView>),
    >,
    mut rubble: Query<
        (&ChildOf, &mut Visibility),
        (With<SentinelRubble>, Without<SentinelTurret>, Without<SentinelView>),
    >,
) {
    // Position the roots and record what each turned out to be, so the child
    // parts can find their emplacement without querying the world again -- the
    // same shape `vehicles::sync_vehicles` uses.
    let mut resolved: HashMap<Entity, Option<RenderSentinel>> = HashMap::new();
    for (entity, view, mut visibility) in &mut roots {
        // Shown for anyone still in the match. Whether it is a gun or a heap is
        // decided below, from whether the snapshot carried one.
        let player = state.render.players.get(view.0 as usize).and_then(Option::as_ref);
        let present = player.is_some_and(|p| !p.eliminated);
        *visibility = if present { Visibility::Inherited } else { Visibility::Hidden };
        if present {
            resolved.insert(entity, player.and_then(|p| p.sentinel));
        }
    }

    for (parent, mut transform, mut visibility) in &mut turrets {
        let Some(sentinel) = resolved.get(&parent.parent()) else {
            continue;
        };
        match sentinel {
            Some(s) => {
                *visibility = Visibility::Inherited;
                // Model +X is the firing direction, so this is the same
                // conversion the shield arcs and vehicle turrets use.
                transform.rotation = coords::yaw_to_quat(s.turret_yaw);
            }
            // Rubble: the gun itself is gone, not merely still.
            None => *visibility = Visibility::Hidden,
        }
    }

    for (parent, mut visibility) in &mut rubble {
        let Some(sentinel) = resolved.get(&parent.parent()) else {
            continue;
        };
        *visibility =
            if sentinel.is_none() { Visibility::Inherited } else { Visibility::Hidden };
    }
}
