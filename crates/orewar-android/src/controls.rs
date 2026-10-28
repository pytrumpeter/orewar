//! Two thumbs: one drives, one aims and fires.
//!
//! The client was written for a keyboard and a mouse, and a tablet has neither.
//! Everything here turns fingers into the input the client already understands,
//! without the client learning that it is being played on glass.
//!
//! Every touch is given an owner the moment it lands, and keeps it until it
//! lifts. A finger that comes down on a stick drives that stick even if it
//! slides off; a finger that comes down anywhere else stands in for the mouse,
//! which is what keeps the menus, the build screen, the miner's mode buttons
//! and the connect screen working exactly as they do on a desktop. Deciding
//! once, on the way down, is what stops a thumb sliding out of its ring from
//! suddenly becoming a click somewhere behind it.
//!
//! Two fingers on open ground are the camera instead: pinching zooms it, and
//! sliding both up or down the glass tilts it. The pair is recognised when the
//! second finger lands, which is the first moment it can be told apart from a
//! tap, and the first finger's press is let go then so that a pinch does not
//! also hold the trigger down.
//!
//! The sticks write to [`LocalInput`] after the client has gathered the
//! keyboard, so they take precedence over keys that are not being pressed while
//! leaving everything else alone. Nothing in the client is modified.

use std::collections::HashMap;

use bevy::input::InputSystems;
use bevy::input::touch::{TouchInput, TouchPhase};
use bevy::prelude::*;
use bevy::ui::FocusPolicy;
use bevy::window::PrimaryWindow;

use orewar_client::camera::{CameraNudge, ChaseCamera, Overview, ZOOM_PER_NOTCH};
use orewar_client::connect::AppState;
use orewar_client::coords;
use orewar_client::input::{self, LocalInput};
use orewar_client::menu::MenuState;
use orewar_shared::protocol::VehicleSlot;

/// How far the knob travels from the middle of its ring, in logical pixels.
const TRAVEL: f32 = 68.0;
/// How near a thumb has to land to take a stick. Wider than the ring, because
/// a thumb aimed at a circle it is not looking at lands near it, not on it.
const GRAB: f32 = 96.0;
/// Radius of the knob the thumb appears to be pushing.
const KNOB: f32 = 30.0;
/// Gap from the screen edges to the outside of a ring.
const MARGIN: f32 = 52.0;

const BUTTON: f32 = 34.0;
const BUTTON_GRAB: f32 = 48.0;
/// How far above the right stick's middle the button sits. Far enough that its
/// catchment and the stick's cannot overlap, near enough for the same thumb.
const BUTTON_RISE: f32 = GRAB + BUTTON_GRAB + 16.0;

/// How far the aiming stick has to move before it means anything. Small: the
/// turret should answer a nudge.
const AIM_DEADZONE: f32 = 0.16;
/// How far it has to move before it also pulls the trigger. Well past aiming,
/// so lining a shot up is not the same as taking it.
const FIRE_POINT: f32 = 0.45;

/// How far the camera tilts for each logical pixel two fingers slide up or
/// down the glass together. A thumb's length of travel is most of the range.
const TILT_PER_PIXEL: f32 = 0.005;
/// The same for the driving stick, which only has to ignore a resting thumb.
const MOVE_DEADZONE: f32 = 0.12;

const RING_BG: Color = Color::srgba(0.60, 0.70, 0.82, 0.10);
const RING_EDGE: Color = Color::srgba(0.60, 0.70, 0.82, 0.30);
const KNOB_BG: Color = Color::srgba(0.70, 0.82, 0.95, 0.30);
const FIRE_BG: Color = Color::srgba(0.95, 0.55, 0.42, 0.28);
const FIRE_EDGE: Color = Color::srgba(0.95, 0.55, 0.42, 0.60);
const FIRE_BG_HELD: Color = Color::srgba(0.95, 0.55, 0.42, 0.65);

pub struct TouchControlsPlugin;

impl Plugin for TouchControlsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Touches>()
            .add_systems(Startup, spawn)
            // After the input systems, which clear last frame's `just_pressed`
            // edges: a press written before them is wiped before anything in
            // `Update` can see it.
            .add_systems(PreUpdate, route.after(InputSystems))
            // After the client has read the keyboard, so that what a thumb is
            // holding wins over a key nobody is pressing.
            .add_systems(Update, (apply.after(input::gather), paint).chain());
    }
}

/// Which side of the screen, and therefore which job.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    /// Throttle and steering, or the yoke and the bank in the air.
    Drive,
    /// Where the turret points, and past a certain lean, the trigger.
    Aim,
}

/// What a finger is doing, decided when it lands and kept until it lifts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Stick(Side),
    Fire,
    /// Standing in for the mouse, which is how everything that is not a stick
    /// -- menus, panels, the connect screen -- carries on working.
    Cursor,
    /// One of a pair on open ground, moving the camera.
    Camera,
    /// A second finger somewhere that already has one. Tracked so that lifting
    /// it does not release something it never took.
    Spare,
}

#[derive(Default)]
struct Stick {
    /// Where the thumb is relative to the middle of the ring, in screen
    /// pixels, clamped to [`TRAVEL`]. Screen `y` runs downward.
    offset: Vec2,
}

impl Stick {
    /// The stick as the game thinks of it: `x` to the right, `y` **up**, each
    /// running to 1 at the edge of the ring.
    fn axes(&self) -> Vec2 {
        Vec2::new(self.offset.x, -self.offset.y) / TRAVEL
    }

    fn lean(&self) -> f32 {
        self.offset.length() / TRAVEL
    }
}

#[derive(Resource, Default)]
struct Touches {
    owners: HashMap<u64, Owner>,
    drive: Stick,
    aim: Stick,
    firing_secondary: bool,
    /// Where each finger of a camera pair is, so the next move can be measured
    /// against it.
    camera: HashMap<u64, Vec2>,
}

/// Where the three controls sit, for a window of this size.
struct Layout {
    drive: Vec2,
    aim: Vec2,
    button: Vec2,
}

fn layout(window: &Window) -> Layout {
    let bottom = window.height() - MARGIN - TRAVEL;
    let aim = Vec2::new(window.width() - MARGIN - TRAVEL, bottom);
    Layout {
        drive: Vec2::new(MARGIN + TRAVEL, bottom),
        aim,
        button: Vec2::new(aim.x, aim.y - BUTTON_RISE),
    }
}

/// Hands every touch to whatever owns that part of the screen.
fn route(
    mut arriving: MessageReader<TouchInput>,
    mut touches: ResMut<Touches>,
    state: Res<State<AppState>>,
    menu: Res<MenuState>,
    mut buttons: ResMut<ButtonInput<MouseButton>>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut nudges: MessageWriter<CameraNudge>,
) {
    let Ok(mut window) = windows.single_mut() else { return };
    let layout = layout(&window);

    // The sticks are a way of driving, so they exist only where driving does.
    // With the pause menu up the whole screen goes back to being a mouse, which
    // is what lets the menu be pressed at all.
    let sticks_live = *state.get() == AppState::Playing && !menu.open;

    for touch in arriving.read() {
        match touch.phase {
            TouchPhase::Started => {
                let owner = claim(&touches, &layout, touch.position, sticks_live);
                touches.owners.insert(touch.id, owner);
                match owner {
                    Owner::Stick(side) => push(&mut touches, side, &layout, touch.position),
                    Owner::Fire => touches.firing_secondary = true,
                    Owner::Cursor => {
                        // Placed before the press, so whatever reads the click
                        // this frame reads it where the screen was touched.
                        window.set_cursor_position(Some(touch.position));
                        buttons.press(MouseButton::Left);
                    }
                    Owner::Camera => {
                        // The finger already down was taken for the mouse.
                        // It becomes the other half of the pair, starting
                        // from wherever it moved the cursor to, and its press
                        // is let go -- with the sticks up a held left button
                        // is the gun.
                        let first = touches
                            .owners
                            .iter()
                            .find(|(_, o)| **o == Owner::Cursor)
                            .map(|(id, _)| *id);
                        if let Some(first) = first {
                            let at = window.cursor_position().unwrap_or(touch.position);
                            touches.owners.insert(first, Owner::Camera);
                            touches.camera.insert(first, at);
                        }
                        touches.camera.insert(touch.id, touch.position);
                        buttons.release(MouseButton::Left);
                    }
                    Owner::Spare => {}
                }
            }
            TouchPhase::Moved => match touches.owners.get(&touch.id).copied() {
                Some(Owner::Stick(side)) => push(&mut touches, side, &layout, touch.position),
                Some(Owner::Cursor) => window.set_cursor_position(Some(touch.position)),
                Some(Owner::Camera) => {
                    if let Some(nudge) = move_camera(&mut touches.camera, touch.id, touch.position)
                    {
                        nudges.write(nudge);
                    }
                }
                _ => {}
            },
            TouchPhase::Ended | TouchPhase::Canceled => {
                match touches.owners.remove(&touch.id) {
                    Some(Owner::Stick(side)) => release(&mut touches, side),
                    Some(Owner::Fire) => touches.firing_secondary = false,
                    Some(Owner::Camera) => {
                        // One finger is not a pinch. The one left behind does
                        // nothing until it lifts too, rather than turning back
                        // into a mouse halfway through a gesture and clicking
                        // wherever it happens to be.
                        touches.camera.clear();
                        for owner in touches.owners.values_mut() {
                            if *owner == Owner::Camera {
                                *owner = Owner::Spare;
                            }
                        }
                    }
                    Some(Owner::Cursor) => {
                        buttons.release(MouseButton::Left);
                        // The cursor stays where the finger left it, exactly as
                        // the client leaves the aim when input stops: a turret
                        // that snaps to a corner on release is worse than one
                        // that waits.
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Decides what a landing finger is for.
///
/// The button is tested first so that the two catchments have a definite
/// winner where they meet, and a stick already held by another finger is not
/// offered to a second one -- two thumbs on one ring would fight over it.
fn claim(touches: &Touches, layout: &Layout, at: Vec2, sticks_live: bool) -> Owner {
    if !sticks_live {
        return if touches.owners.values().any(|o| *o == Owner::Cursor) {
            Owner::Spare
        } else {
            Owner::Cursor
        };
    }

    let held = |want: Owner| touches.owners.values().any(|o| *o == want);

    if at.distance(layout.button) <= BUTTON_GRAB {
        return if held(Owner::Fire) { Owner::Spare } else { Owner::Fire };
    }
    for (side, centre) in [(Side::Drive, layout.drive), (Side::Aim, layout.aim)] {
        if at.distance(centre) <= GRAB {
            return if held(Owner::Stick(side)) { Owner::Spare } else { Owner::Stick(side) };
        }
    }
    if held(Owner::Camera) {
        Owner::Spare
    } else if held(Owner::Cursor) {
        // A second finger on open ground: the two of them are a pinch.
        Owner::Camera
    } else {
        Owner::Cursor
    }
}

/// Moves one finger of a camera pair, and says what that did to the camera.
///
/// Nothing, until there are two fingers to measure between.
fn move_camera(pair: &mut HashMap<u64, Vec2>, id: u64, to: Vec2) -> Option<CameraNudge> {
    let before: Vec<Vec2> = pair.values().copied().collect();
    *pair.get_mut(&id)? = to;
    let after: Vec<Vec2> = pair.values().copied().collect();
    match (before.as_slice(), after.as_slice()) {
        ([a, b], [c, d]) => Some(gesture([*a, *b], [*c, *d])),
        _ => None,
    }
}

/// The camera move two fingers make between one position and the next.
///
/// Spreading them apart is zooming in, by as many wheel notches as it takes to
/// grow the view by the same proportion the gap grew -- so the ground under
/// the fingers roughly stays under them. Sliding them down the glass together
/// tilts toward looking straight down, and up toward the horizon, which is the
/// way a map on a phone does it.
fn gesture(before: [Vec2; 2], after: [Vec2; 2]) -> CameraNudge {
    let gap_before = before[0].distance(before[1]);
    let gap_after = after[0].distance(after[1]);
    let zoom = if gap_before > 1.0 && gap_after > 1.0 {
        (gap_after / gap_before).ln() / ZOOM_PER_NOTCH.ln()
    } else {
        0.0
    };
    let middle = |p: [Vec2; 2]| (p[0] + p[1]) * 0.5;
    let tilt = (middle(after).y - middle(before).y) * TILT_PER_PIXEL;
    CameraNudge { zoom, tilt }
}

/// Moves a stick to follow its thumb, held inside the ring.
///
/// A thumb that slides past the edge keeps pushing at full lean rather than
/// losing the stick, which is what a real one does under a thumb that has
/// travelled further than the stick can.
fn push(touches: &mut Touches, side: Side, layout: &Layout, at: Vec2) {
    let centre = match side {
        Side::Drive => layout.drive,
        Side::Aim => layout.aim,
    };
    let offset = at - centre;
    let clamped =
        if offset.length() > TRAVEL { offset.normalize() * TRAVEL } else { offset };
    stick_mut(touches, side).offset = clamped;
}

fn release(touches: &mut Touches, side: Side) {
    stick_mut(touches, side).offset = Vec2::ZERO;
}

fn stick_mut(touches: &mut Touches, side: Side) -> &mut Stick {
    match side {
        Side::Drive => &mut touches.drive,
        Side::Aim => &mut touches.aim,
    }
}

/// Turns the sticks into the frame the client is about to send.
fn apply(
    touches: Res<Touches>,
    menu: Res<MenuState>,
    overview: Res<Overview>,
    camera: Query<&GlobalTransform, With<ChaseCamera>>,
    mut input: ResMut<LocalInput>,
) {
    // The same gate the keyboard goes through. Only the pause menu takes the
    // controls away; the overview does not, because an aircraft keeps flying
    // whether or not it is being watched.
    if !input::controls_for(menu.open, overview.active).live {
        return;
    }

    let drive = touches.drive.axes();
    if drive.length() > MOVE_DEADZONE {
        // Increasing yaw turns toward the vehicle's right, so pushing the stick
        // right is positive steer. In the air this is the bank, which the
        // simulation holds on its own -- the stick sets how fast it rolls, not
        // how far over it is, so letting go leaves the turn where it was. A
        // self-centring stick is the keyboard exactly.
        input.steer = drive.x.clamp(-1.0, 1.0);

        let forward = drive.y.clamp(-1.0, 1.0);
        if input.controlling == VehicleSlot::Plane {
            // Pushed away is down, pulled back is up: a control column, and
            // what `W` already does. Negate this line to fly it the other way.
            input.climb = -forward;
        } else {
            input.throttle = forward;
        }
    }

    let aim = touches.aim.axes();
    let lean = touches.aim.lean();
    if lean > AIM_DEADZONE {
        if let Ok(camera) = camera.single()
            && let Some(angle) = aim_angle(camera, aim)
        {
            input.aim = angle;
        }
        // Leaning further is the trigger. Aiming and firing are deliberately
        // different distances: a thumb that has only just moved is lining a
        // shot up, not taking it.
        if lean > FIRE_POINT {
            input.fire_primary = true;
        }
    }

    if touches.firing_secondary {
        input.fire_secondary = true;
    }
}

/// Turns a direction on the screen into the simulation angle it points at.
///
/// The camera trails the vehicle and turns with it, so "up the screen" is
/// wherever the vehicle is heading rather than a fixed compass point. Taking
/// the direction from the camera is what makes the stick mean what it looks
/// like it means from behind the hull.
///
/// The two axes come straight out of [`coords`]: a heading maps to the screen's
/// up, and its `perp` maps to the screen's right.
fn aim_angle(camera: &GlobalTransform, axes: Vec2) -> Option<f32> {
    let look = camera.rotation() * Vec3::NEG_Z;
    // Flattened onto the ground: the camera looks down at the field, and only
    // the part of its gaze that runs along the ground says which way is north
    // from where the player is sitting.
    let flat = Vec3::new(look.x, 0.0, look.z);
    if flat.length_squared() < 1e-6 {
        return None;
    }
    let forward = coords::world_to_sim(flat.normalize());
    let dir = forward * axes.y + forward.perp() * axes.x;
    (dir.length_squared() > 1e-6).then(|| dir.to_angle())
}

#[derive(Component)]
struct ControlsRoot;

/// One of the five circles on the glass. Kept as a single component so that
/// laying them out is one query over one list rather than three that have to be
/// told they cannot overlap.
#[derive(Component, Clone, Copy)]
enum Piece {
    /// The ring a stick travels inside.
    Ring(Side),
    /// The knob that follows the thumb.
    Knob(Side),
    Fire,
}

/// Builds the three controls once, hidden, and leaves them to [`paint`].
fn spawn(mut commands: Commands) {
    commands
        .spawn((
            ControlsRoot,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            // Nothing here is pressed in the usual way -- touches are routed
            // before the UI ever sees them -- so this must not stand between a
            // finger and the panels underneath it.
            FocusPolicy::Pass,
            Visibility::Hidden,
        ))
        .with_children(|parent| {
            for side in [Side::Drive, Side::Aim] {
                let ring = (circle(TRAVEL), BackgroundColor(RING_BG), border(RING_EDGE));
                let knob = (circle(KNOB), BackgroundColor(KNOB_BG), border(RING_EDGE));
                parent.spawn((Piece::Ring(side), ring));
                parent.spawn((Piece::Knob(side), knob));
            }
            parent.spawn((
                Piece::Fire,
                circle(BUTTON),
                BackgroundColor(FIRE_BG),
                border(FIRE_EDGE),
            ));
        });
}

fn circle(radius: f32) -> (Node, FocusPolicy) {
    (
        Node {
            position_type: PositionType::Absolute,
            width: px(radius * 2.0),
            height: px(radius * 2.0),
            border: UiRect::all(px(2)),
            border_radius: BorderRadius::all(px(radius)),
            ..default()
        },
        FocusPolicy::Pass,
    )
}

fn border(color: Color) -> BorderColor {
    BorderColor::all(color)
}

/// Puts the controls where the window says they go, and shows them only while
/// there is something to drive.
fn paint(
    touches: Res<Touches>,
    state: Res<State<AppState>>,
    menu: Res<MenuState>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut root: Query<&mut Visibility, With<ControlsRoot>>,
    mut pieces: Query<(&Piece, &mut Node, &mut BackgroundColor)>,
) {
    let Ok(window) = windows.single() else { return };
    let Ok(mut visibility) = root.single_mut() else { return };

    let showing = *state.get() == AppState::Playing && !menu.open;
    *visibility = if showing { Visibility::Inherited } else { Visibility::Hidden };
    // Nothing needs moving while nothing is on screen.
    if !showing {
        return;
    }

    let layout = layout(window);
    for (piece, mut node, mut color) in &mut pieces {
        match *piece {
            Piece::Ring(side) => place(&mut node, centre_of(&layout, side), TRAVEL),
            Piece::Knob(side) => {
                let stick = match side {
                    Side::Drive => &touches.drive,
                    Side::Aim => &touches.aim,
                };
                place(&mut node, centre_of(&layout, side) + stick.offset, KNOB);
            }
            Piece::Fire => {
                place(&mut node, layout.button, BUTTON);
                *color = BackgroundColor(if touches.firing_secondary {
                    FIRE_BG_HELD
                } else {
                    FIRE_BG
                });
            }
        }
    }
}

fn centre_of(layout: &Layout, side: Side) -> Vec2 {
    match side {
        Side::Drive => layout.drive,
        Side::Aim => layout.aim,
    }
}

/// A node is placed by its top-left corner; everything here is thought about as
/// a middle and a radius.
fn place(node: &mut Node, centre: Vec2, radius: f32) {
    node.left = px(centre.x - radius);
    node.top = px(centre.y - radius);
}

#[cfg(test)]
mod tests {
    use super::*;
    use orewar_shared::math::Vec2 as SimVec2;

    /// The two catchments must not overlap, or a thumb reaching for the missile
    /// button would sometimes grab the stick under it instead -- and which one
    /// it got would depend on a pixel.
    #[test]
    fn the_button_is_clear_of_the_stick_below_it() {
        assert!(BUTTON_RISE > GRAB + BUTTON_GRAB);
    }

    /// Lining a shot up and taking it have to be different distances, or the
    /// turret fires the instant it is asked to point somewhere.
    #[test]
    fn aiming_starts_before_firing_does() {
        assert!(AIM_DEADZONE < FIRE_POINT);
    }

    /// Fingers spreading is closer, closing is further, and nothing else
    /// about a pinch should tilt the camera.
    #[test]
    fn a_pinch_zooms_without_tilting() {
        let near = [Vec2::new(400.0, 300.0), Vec2::new(500.0, 300.0)];
        let far = [Vec2::new(350.0, 300.0), Vec2::new(550.0, 300.0)];

        let spread = gesture(near, far);
        assert!(spread.zoom > 0.0);
        assert_eq!(spread.tilt, 0.0);
        // Twice the gap is however many notches double the view.
        assert!((ZOOM_PER_NOTCH.powf(spread.zoom) - 2.0).abs() < 1e-3);

        let closed = gesture(far, near);
        assert!((closed.zoom + spread.zoom).abs() < 1e-4);
    }

    /// Down the glass looks down at the ground; up looks toward the horizon.
    #[test]
    fn two_fingers_sliding_down_tilt_toward_the_ground() {
        let start = [Vec2::new(400.0, 300.0), Vec2::new(500.0, 300.0)];
        let lower = start.map(|p| p + Vec2::new(0.0, 40.0));
        let nudge = gesture(start, lower);
        assert!(nudge.tilt > 0.0);
        assert!(nudge.zoom.abs() < 1e-6);
        assert!(gesture(lower, start).tilt < 0.0);
    }

    /// A finger with no partner yet must not move the camera at all.
    #[test]
    fn one_finger_is_not_a_gesture() {
        let mut pair = HashMap::from([(1, Vec2::new(10.0, 10.0))]);
        assert_eq!(move_camera(&mut pair, 1, Vec2::new(50.0, 50.0)), None);
        assert_eq!(move_camera(&mut pair, 7, Vec2::new(50.0, 50.0)), None);
    }

    #[test]
    fn a_stick_pushed_right_reads_right_and_level() {
        let stick = Stick { offset: Vec2::new(TRAVEL, 0.0) };
        assert_eq!(stick.axes(), Vec2::new(1.0, 0.0));
        assert_eq!(stick.lean(), 1.0);
    }

    /// Screen `y` runs down the glass and the game's does not, so pushing a
    /// stick up the screen has to come out positive.
    #[test]
    fn pushing_up_the_screen_is_positive() {
        let stick = Stick { offset: Vec2::new(0.0, -TRAVEL) };
        assert_eq!(stick.axes(), Vec2::new(0.0, 1.0));
    }

    /// With the camera behind the vehicle, up the screen must aim where the
    /// vehicle is already pointed, and right must be its right.
    #[test]
    fn up_the_screen_aims_where_the_camera_looks() {
        for step in 0..16 {
            let yaw = step as f32 * std::f32::consts::TAU / 16.0;
            // A camera looking along the vehicle's heading, tilted down at it
            // the way the chase camera is.
            let heading = coords::sim_to_world(SimVec2::from_angle(yaw));
            let looking = GlobalTransform::from(
                Transform::from_translation(Vec3::Y * 20.0)
                    .looking_to(heading - Vec3::Y * 0.6, Vec3::Y),
            );

            let ahead = aim_angle(&looking, Vec2::new(0.0, 1.0)).unwrap();
            assert!(
                (SimVec2::from_angle(ahead) - SimVec2::from_angle(yaw)).length() < 1e-3,
                "yaw {yaw}: pushing up aimed at {ahead} instead of straight ahead"
            );

            let right = aim_angle(&looking, Vec2::new(1.0, 0.0)).unwrap();
            let want = SimVec2::from_angle(yaw).perp();
            assert!(
                (SimVec2::from_angle(right) - want).length() < 1e-3,
                "yaw {yaw}: pushing right aimed at {right}, not the vehicle's right"
            );
        }
    }
}
