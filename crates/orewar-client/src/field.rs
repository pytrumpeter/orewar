//! The playing field: grass, sky, lighting, base pads, and ore deposits.

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
use bevy::light::{CascadeShadowConfigBuilder, NotShadowCaster};
use bevy::mesh::Indices;
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, PrimitiveTopology, TextureDimension, TextureFormat,
};
use orewar_shared::rng::Rng;
use orewar_shared::world::{self, WORLD_SIZE};

use crate::coords;
use crate::state::GameState;

/// Sky, and the color distant ground fades into so the horizon has no seam.
pub const SKY_COLOR: Color = Color::srgb(0.44, 0.66, 0.92);

/// Radius of a full deposit's patch of ore-bearing rock, in world units. Kept
/// comfortably inside `sim::HARVEST_RADIUS` so that parking anywhere on the
/// visible rock is enough to mine it.
const ORE_PATCH_RADIUS: f32 = 5.4;
/// How far the tallest shard stands proud of the grass. Deliberately small:
/// a deposit reads as a scar in the ground, not as an obstacle.
const ORE_PATCH_HEIGHT: f32 = 0.55;
/// How many distinct patch outlines are built, so a field of deposits does not
/// look stamped from a single die.
const ORE_PATCH_VARIANTS: usize = 3;
/// A hill's height as a fraction of its radius. Tall enough that a tank cannot
/// see over one, which is what makes terrain worth driving around.
const HILL_HEIGHT_RATIO: f32 = 0.5;
/// How many distinct hill profiles are built.
const HILL_VARIANTS: usize = 3;

/// Height a patch's rim sits at, clear of both the grass and the grid lines
/// (0.03) so none of them fight for depth. The base pads need no thought here:
/// generation keeps deposits well away from them.
const ORE_PATCH_LIFT: f32 = 0.09;

#[derive(Component)]
pub struct OreView(pub usize);

#[derive(Component)]
pub struct HillView(pub usize);

#[derive(Resource)]
pub struct FieldAssets {
    ore_meshes: Vec<Handle<Mesh>>,
    ore_material: Handle<StandardMaterial>,
    ore_spent_material: Handle<StandardMaterial>,
    /// Built at unit radius, so one mesh serves every hill through its scale.
    hill_meshes: Vec<Handle<Mesh>>,
    hill_material: Handle<StandardMaterial>,
}

/// Builds a small tiling grass texture.
///
/// Generated rather than shipped so the client needs no asset files at all,
/// which keeps running it a single `cargo run`.
fn grass_texture() -> Image {
    const SIZE: usize = 64;
    let mut data = vec![0u8; SIZE * SIZE * 4];
    let mut rng = Rng::new(0x6C7A_5500_1234_ABCD);

    for pixel in data.chunks_exact_mut(4) {
        // A base green with per-pixel variation, plus occasional darker blades
        // so the ground reads as grass rather than flat paint when you drive
        // across it at a shallow angle.
        let n = rng.f32();
        let blade = rng.chance(0.18);
        let shade = if blade { 0.72 + n * 0.12 } else { 0.92 + n * 0.16 };
        pixel[0] = ((0.32 * shade) * 255.0).clamp(0.0, 255.0) as u8;
        pixel[1] = ((0.56 * shade) * 255.0).clamp(0.0, 255.0) as u8;
        pixel[2] = ((0.26 * shade) * 255.0).clamp(0.0, 255.0) as u8;
        pixel[3] = 255;
    }

    let mut image = Image::new_fill(
        Extent3d { width: SIZE as u32, height: SIZE as u32, depth_or_array_layers: 1 },
        TextureDimension::D2,
        &data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    // The texture is tiled many times across the field, so it has to wrap.
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        ..ImageSamplerDescriptor::linear()
    });
    image
}

pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    let half = WORLD_SIZE * 0.5;

    // ---- Ground -----------------------------------------------------------
    let grass = images.add(grass_texture());
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(WORLD_SIZE, WORLD_SIZE))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color_texture: Some(grass),
            // Tiles the 64px texture once every 8 world units.
            uv_transform: bevy::math::Affine2::from_scale(Vec2::splat(WORLD_SIZE / 8.0)),
            perceptual_roughness: 0.97,
            metallic: 0.0,
            ..default()
        })),
        Transform::from_xyz(half, coords::GROUND_Y, half),
    ));

    // ---- Sun and sky ------------------------------------------------------
    commands.spawn((
        DirectionalLight {
            illuminance: 8_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        // The default cascade configuration is sized for very large scenes and
        // renders far more shadow area than this field needs. Bounding it to
        // roughly the camera's useful range keeps the shadow pass cheap.
        CascadeShadowConfigBuilder {
            num_cascades: 3,
            first_cascade_far_bound: 40.0,
            maximum_distance: 240.0,
            ..default()
        }
        .build(),
        // A directional light is aimed by its rotation; `looking_to` takes the
        // direction the light should shine, where `looking_at` would want a
        // point in the world.
        Transform::from_xyz(half, 80.0, half).looking_to(Vec3::new(0.35, -1.0, 0.28), Vec3::Y),
    ));
    // Cool but nearly neutral, and gentle: a strongly tinted, very bright
    // ambient term washes the grass out until it reads as more sky.
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.78, 0.85, 0.96),
        brightness: 140.0,
        ..default()
    });

    // ---- Grid -------------------------------------------------------------
    commands.spawn((
        Mesh3d(meshes.add(grid_mesh())),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgba(0.12, 0.26, 0.11, 0.85),
            alpha_mode: AlphaMode::Blend,
            perceptual_roughness: 1.0,
            // Flat markings on the ground; lighting them adds nothing and the
            // unlit path avoids depending on face winding.
            unlit: true,
            double_sided: true,
            cull_mode: None,
            ..default()
        })),
        // Just clear of the ground plane, so the two do not fight for depth.
        Transform::from_xyz(0.0, coords::GROUND_Y + 0.03, 0.0),
        // Flat markings painted on the ground have no business casting shadows.
        NotShadowCaster,
    ));

    // ---- Boundary ---------------------------------------------------------
    // A low wall makes the edge of the world legible instead of an invisible
    // stop the vehicle bumps into.
    let wall_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.52, 0.50, 0.45),
        perceptual_roughness: 0.9,
        ..default()
    });
    let wall_thickness = 1.5;
    let wall_height = 2.4;
    let long = meshes.add(Cuboid::new(WORLD_SIZE + wall_thickness * 2.0, wall_height, wall_thickness));
    let side = meshes.add(Cuboid::new(wall_thickness, wall_height, WORLD_SIZE + wall_thickness * 2.0));
    let y = wall_height * 0.5;
    for (mesh, pos) in [
        (long.clone(), Vec3::new(half, y, -wall_thickness * 0.5)),
        (long, Vec3::new(half, y, WORLD_SIZE + wall_thickness * 0.5)),
        (side.clone(), Vec3::new(-wall_thickness * 0.5, y, half)),
        (side, Vec3::new(WORLD_SIZE + wall_thickness * 0.5, y, half)),
    ] {
        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(wall_material.clone()),
            Transform::from_translation(pos),
        ));
    }

    // ---- Base pads --------------------------------------------------------
    let pad_mesh = meshes.add(Cylinder::new(world::BASE_RADIUS, 0.12));
    for player in 0..world::MAX_PLAYERS as u8 {
        let color = coords::player_color(player);
        commands.spawn((
            Mesh3d(pad_mesh.clone()),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: color.with_alpha(0.55),
                // Emissive so a pad still reads as "yours" in shadow.
                emissive: LinearRgba::from(color) * 0.35,
                alpha_mode: AlphaMode::Blend,
                perceptual_roughness: 0.6,
                ..default()
            })),
            Transform::from_translation(coords::sim_to_world_at(
                world::base_position(player),
                0.06,
            )),
            NotShadowCaster,
        ));
    }

    // ---- Ore assets (deposits are spawned once the seed arrives) ----------
    commands.insert_resource(FieldAssets {
        ore_meshes: (0..ORE_PATCH_VARIANTS)
            .map(|i| meshes.add(ore_patch_mesh(0x0FE5_0DEB_1234_0000 ^ i as u64)))
            .collect(),
        ore_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.93, 0.68, 0.16),
            emissive: LinearRgba::new(0.28, 0.17, 0.02, 1.0),
            // Rough, barely metallic, and only lightly emissive: at the camera's
            // shallow look-down angle a glossy, glowing flat surface reads as a
            // puddle of paint, and the facets stop doing any work.
            perceptual_roughness: 0.55,
            metallic: 0.25,
            ..default()
        }),
        ore_spent_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.36, 0.32, 0.26),
            perceptual_roughness: 0.95,
            ..default()
        }),
        hill_meshes: (0..HILL_VARIANTS)
            .map(|i| meshes.add(hill_mesh(0x4111_0000_9AB1_0000 ^ i as u64)))
            .collect(),
        hill_material: materials.add(StandardMaterial {
            // Coarser and browner than the grass, so a hill reads as ground you
            // cannot cross rather than as more field.
            base_color: Color::srgb(0.35, 0.40, 0.21),
            perceptual_roughness: 0.98,
            ..default()
        }),
    });
}

/// Creates deposit entities once the world seed is known, then keeps their size
/// in step with how much ore is left.
pub fn sync_ore(
    mut commands: Commands,
    assets: Res<FieldAssets>,
    state: Res<GameState>,
    mut views: Query<(&OreView, &mut Transform, &mut MeshMaterial3d<StandardMaterial>)>,
    existing: Query<&OreView>,
) {
    if state.ore.is_empty() {
        return;
    }
    if existing.iter().count() < state.ore.len() {
        for (i, deposit) in state.ore.iter().enumerate() {
            commands.spawn((
                OreView(i),
                Mesh3d(assets.ore_meshes[i % assets.ore_meshes.len()].clone()),
                MeshMaterial3d(assets.ore_material.clone()),
                // Spun by index as well as cycled through the shapes, so two
                // patches of the same shape never sit in the same orientation.
                Transform::from_translation(coords::sim_to_world_at(deposit.pos, ORE_PATCH_LIFT))
                    .with_rotation(Quat::from_rotation_y(i as f32 * 1.107)),
            ));
        }
    }

    for (view, mut transform, mut material) in &mut views {
        let Some(deposit) = state.ore.get(view.0) else { continue };
        let fraction = if deposit.capacity > 0.0 {
            (deposit.amount / deposit.capacity).clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Deposits visibly shrink as they are worked, so a player can read the
        // state of the field from the driving seat. They flatten faster than
        // they narrow: a worked-out deposit should still be somewhere you can
        // see you have been, rather than vanishing to a speck.
        let spread = 0.55 + fraction * 0.45;
        transform.scale = Vec3::new(spread, 0.25 + fraction * 0.75, spread);
        // Followed every frame rather than only at spawn: a restarted match
        // reseeds the field, which moves every deposit without changing how
        // many there are.
        transform.translation = coords::sim_to_world_at(deposit.pos, ORE_PATCH_LIFT);

        let wanted =
            if fraction <= 0.001 { &assets.ore_spent_material } else { &assets.ore_material };
        if material.0.id() != wanted.id() {
            material.0 = wanted.clone();
        }
    }
}

/// Creates the hills once the world seed is known, and keeps them in step with
/// it: a restarted match reseeds the terrain as well as the ore.
///
/// The count never changes, so this only ever spawns once; the per-frame pass
/// exists so a reseed can move and resize what is already there.
pub fn sync_hills(
    mut commands: Commands,
    assets: Res<FieldAssets>,
    state: Res<GameState>,
    mut views: Query<(&HillView, &mut Transform)>,
    existing: Query<&HillView>,
) {
    if state.hills.is_empty() {
        return;
    }
    if existing.iter().count() < state.hills.len() {
        for i in 0..state.hills.len() {
            commands.spawn((
                HillView(i),
                Mesh3d(assets.hill_meshes[i % assets.hill_meshes.len()].clone()),
                MeshMaterial3d(assets.hill_material.clone()),
                Transform::default(),
            ));
        }
    }

    for (view, mut transform) in &mut views {
        let Some(hill) = state.hills.get(view.0) else { continue };
        // Uniform: the mesh is a unit-radius dome whose height is already the
        // ratio, so scaling by the radius keeps the profile of every hill the
        // same shape at a different size.
        *transform = Transform::from_translation(coords::sim_to_world(hill.pos))
            .with_scale(Vec3::splat(hill.radius))
            .with_rotation(Quat::from_rotation_y(view.0 as f32 * 0.83));
    }
}

/// Builds a hill: a low, faceted dome of unit radius.
///
/// The silhouette is jittered inward only. The simulation blocks a vehicle at
/// the exact radius, so a bump that stuck out past it would show as a hull
/// stopping against thin air.
fn hill_mesh(seed: u64) -> Mesh {
    const RINGS: usize = 4;
    const SPOKES: usize = 20;

    let mut rng = Rng::new(seed);
    // `rings[r][s]` is the vertex on ring `r` (1 = outermost) at spoke `s`.
    let mut rings: Vec<Vec<Vec3>> = Vec::with_capacity(RINGS);
    for ring in 1..=RINGS {
        // Outermost ring first, so index 0 is the rim.
        let frac = 1.0 - (ring - 1) as f32 / RINGS as f32;
        let mut points = Vec::with_capacity(SPOKES);
        for spoke in 0..SPOKES {
            let angle = spoke as f32 / SPOKES as f32 * std::f32::consts::TAU;
            let (sin, cos) = angle.sin_cos();
            let r = frac * rng.range_f32(0.86, 1.0);
            // A raised-cosine profile: flat-topped and flat-footed, so the hill
            // meets the grass without a visible seam and has a summit rather
            // than a spike.
            let profile = 0.5 * (1.0 + (std::f32::consts::PI * frac).cos());
            let h = HILL_HEIGHT_RATIO * profile * rng.range_f32(0.86, 1.14);
            points.push(Vec3::new(cos * r, h, sin * r));
        }
        rings.push(points);
    }
    let summit = Vec3::new(0.0, HILL_HEIGHT_RATIO * rng.range_f32(0.94, 1.06), 0.0);

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut tri = |a: Vec3, b: Vec3, c: Vec3| {
        let mut n = (b - a).cross(c - a);
        let (b, c) = if n.y < 0.0 {
            n = -n;
            (c, b)
        } else {
            (b, c)
        };
        let n = n.normalize_or_zero().to_array();
        for v in [a, b, c] {
            positions.push(v.to_array());
            normals.push(n);
            uvs.push([0.0, 0.0]);
        }
    };

    for s in 0..SPOKES {
        let t = (s + 1) % SPOKES;
        for r in 0..RINGS - 1 {
            tri(rings[r][s], rings[r + 1][s], rings[r][t]);
            tri(rings[r][t], rings[r + 1][s], rings[r + 1][t]);
        }
        tri(rings[RINGS - 1][s], summit, rings[RINGS - 1][t]);
    }

    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
}

/// Builds one deposit's patch of ore-bearing rock: flat, jagged, barely raised.
///
/// A ring of spokes gives the outline. Their radii alternate between a long
/// point and a deep notch rather than wandering freely, because a randomized
/// radius on every spoke averages out into a lumpy circle -- the outline only
/// reads as *broken* if consecutive spokes disagree sharply. A second, inner
/// ring carries the crags that catch the light. The outer ring sits at `y = 0`
/// and the lift off the ground lives in the entity transform, so [`sync_ore`]
/// can flatten a spent deposit toward the grass without any part of it sinking
/// below.
fn ore_patch_mesh(seed: u64) -> Mesh {
    const SPOKES: usize = 18;

    let mut rng = Rng::new(seed);
    let mut rim = Vec::with_capacity(SPOKES);
    let mut crag = Vec::with_capacity(SPOKES);
    for i in 0..SPOKES {
        let angle =
            i as f32 / SPOKES as f32 * std::f32::consts::TAU + rng.range_f32(-0.12, 0.12);
        let (sin, cos) = angle.sin_cos();
        let r = ORE_PATCH_RADIUS
            * if i % 2 == 0 { rng.range_f32(0.86, 1.0) } else { rng.range_f32(0.40, 0.58) };
        rim.push(Vec3::new(cos * r, 0.0, sin * r));
        // Crags stay well inside the shortest notch, so no spoke's inner point
        // can overtake its own rim and fold the surface over.
        let inner = ORE_PATCH_RADIUS * rng.range_f32(0.22, 0.38);
        // The tall rock sits behind the long points; behind a notch it barely
        // lifts, so the raised part follows the outline's teeth rather than
        // forming a ring of its own.
        let height = ORE_PATCH_HEIGHT
            * if i % 2 == 0 { rng.range_f32(0.55, 1.0) } else { rng.range_f32(0.10, 0.30) };
        crag.push(Vec3::new(cos * inner, height, sin * inner));
    }
    let peak = Vec3::new(0.0, ORE_PATCH_HEIGHT * rng.range_f32(0.45, 0.80), 0.0);

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();

    // Flat shaded: each triangle gets its own three vertices and a single
    // normal, which is what makes the facets read as broken rock rather than
    // as one smooth lump.
    let mut tri = |a: Vec3, b: Vec3, c: Vec3| {
        let mut n = (b - a).cross(c - a);
        // Winding decides which face is drawn, and these triangles are built
        // from randomized points. Flipping the ones that came out facing down
        // is cheaper than reasoning about the order of every case.
        let (b, c) = if n.y < 0.0 {
            n = -n;
            (c, b)
        } else {
            (b, c)
        };
        let n = n.normalize_or_zero().to_array();
        for v in [a, b, c] {
            positions.push(v.to_array());
            normals.push(n);
            uvs.push([0.0, 0.0]);
        }
    };

    for i in 0..SPOKES {
        let j = (i + 1) % SPOKES;
        tri(peak, crag[i], crag[j]);
        tri(crag[i], rim[i], rim[j]);
        tri(crag[i], rim[j], crag[j]);
    }

    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
}

/// Builds the field's grid lines as a single mesh.
///
/// Drawn as real geometry rather than with `Gizmos`: gizmos are a debug
/// facility that re-uploads its vertices every frame and leans on the debug
/// line pipeline, whereas this is one static mesh and one draw call.
fn grid_mesh() -> Mesh {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    // Half the width of a painted line, in world units.
    const HALF: f32 = 0.075;

    let mut quad = |x0: f32, z0: f32, x1: f32, z1: f32| {
        let base = positions.len() as u32;
        for (x, z) in [(x0, z0), (x1, z0), (x1, z1), (x0, z1)] {
            positions.push([x, 0.0, z]);
            normals.push([0.0, 1.0, 0.0]);
            uvs.push([0.0, 0.0]);
        }
        indices.extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
    };

    for i in 0..=world::GRID_CELLS {
        let at = i as f32 * world::CELL_SIZE;
        // Running north-south, then east-west.
        quad(at - HALF, 0.0, at + HALF, WORLD_SIZE);
        quad(0.0, at - HALF, WORLD_SIZE, at + HALF);
    }

    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_indices(Indices::U32(indices))
}
