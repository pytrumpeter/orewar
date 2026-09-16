//! Turning keyboard and mouse into the one [`InputFrame`] sent each tick.
//!
//! Continuous controls (throttle, steering, firing) are sampled every frame and
//! consumed by the fixed-rate sender. Discrete ones (switching vehicles, the
//! build menu, purchases) are handled here in `Update`, because `FixedUpdate`
//! can run twice or not at all in a frame and would double or drop an edge.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use orewar_shared::math::Vec2 as SimVec2;
use orewar_shared::protocol::{InputFrame, VehicleSlot};
use orewar_shared::world::PowerUp;

use crate::coords;
use crate::menu::MenuState;
use crate::net::{Link, NetClient};
use crate::state::GameState;

#[derive(Resource)]
pub struct LocalInput {
    /// Monotonic client tick; the server echoes it back for reconciliation.
    pub tick: u32,
    pub controlling: VehicleSlot,
    pub throttle: f32,
    pub steer: f32,
    pub aim: f32,
    pub fire_primary: bool,
    pub fire_secondary: bool,
    pub build_menu: bool,
    /// The frame sent this tick, kept so prediction applies exactly what was sent.
    pub current: InputFrame,
}

impl Default for LocalInput {
    fn default() -> Self {
        LocalInput {
            tick: 0,
            controlling: VehicleSlot::Tank,
            throttle: 0.0,
            steer: 0.0,
            aim: 0.0,
            fire_primary: false,
            fire_secondary: false,
            build_menu: false,
            current: InputFrame::default(),
        }
    }
}

/// Purchase hotkeys, in the order the build menu lists them.
pub const BUY_KEYS: [KeyCode; 8] = [
    KeyCode::Digit1,
    KeyCode::Digit2,
    KeyCode::Digit3,
    KeyCode::Digit4,
    KeyCode::Digit5,
    KeyCode::Digit6,
    KeyCode::Digit7,
    KeyCode::Digit8,
];

/// Calls up a sortie.
pub const LAUNCH_KEY: KeyCode = KeyCode::KeyG;

pub fn gather(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform)>,
    menu: Res<MenuState>,
    overview: Res<crate::camera::Overview>,
    mut input: ResMut<LocalInput>,
    mut state: ResMut<GameState>,
    mut net: ResMut<NetClient>,
) {
    // With the menu up, or the camera lifted off the vehicle, it coasts: it
    // should not carry on driving on whatever was held at the time, and a
    // click on a menu item or a mode button is not a trigger pull. Gating the
    // overview this way is also what keeps it from being free reconnaissance:
    // looking at the whole field means not fighting while you do it.
    //
    // Aim is left where it was, exactly as it is when input stops arriving at
    // all -- a turret that snaps is worse than one that waits.
    if menu.open || overview.active {
        input.throttle = 0.0;
        input.steer = 0.0;
        input.fire_primary = false;
        input.fire_secondary = false;
        return;
    }

    let held = |a: KeyCode, b: KeyCode| keys.pressed(a) || keys.pressed(b);

    let mut throttle = 0.0;
    if held(KeyCode::KeyW, KeyCode::ArrowUp) {
        throttle += 1.0;
    }
    if held(KeyCode::KeyS, KeyCode::ArrowDown) {
        throttle -= 1.0;
    }

    // `step_vehicle` adds `steer * turn_rate * dt` to the yaw, and increasing
    // yaw rotates the heading toward the vehicle's right (see
    // `coords::sim_perp_is_screen_right`). So turning left is negative steer.
    let mut steer = 0.0;
    if held(KeyCode::KeyA, KeyCode::ArrowLeft) {
        steer -= 1.0;
    }
    if held(KeyCode::KeyD, KeyCode::ArrowRight) {
        steer += 1.0;
    }

    input.throttle = throttle;
    input.steer = steer;
    input.fire_primary = buttons.pressed(MouseButton::Left) || keys.pressed(KeyCode::Space);
    input.fire_secondary = buttons.pressed(MouseButton::Right) || keys.pressed(KeyCode::KeyF);

    // Call up a sortie. Reliable, like a purchase, and only sent when it can
    // actually be granted -- the client has the upgrade mask and the cooldown
    // in every snapshot, so a key that would be declined is a key that does
    // nothing here rather than a request the server quietly drops.
    if keys.just_pressed(LAUNCH_KEY) && net.link == Link::Connected {
        if state.local().is_some_and(|me| me.sortie_ready()) {
            net.launch_plane();
        }
    }

    // Swap which vehicle you are driving. The cycle only stops on what you
    // actually have, so it is two vehicles until a sortie is up and three
    // while it is, rather than a third stop that is empty most of a match.
    if keys.just_pressed(KeyCode::Tab) {
        if let Some(me) = state.local() {
            let next = input.controlling.next_available(|slot| me.vehicle(slot).is_some());
            if next != input.controlling {
                input.controlling = next;
                state.set_predicted_slot(next);
            }
        }
    }

    // Follow the vehicle you actually have. A destroyed tank leaves nothing
    // to drive, and an aircraft running dry takes the controls out from under
    // you mid-flight with nothing to announce it. The server already falls
    // through, but prediction and the turret still have to be pointed at the
    // right hull or the harvester moves under you while the camera and aim
    // stay behind. Coming back the same way puts you in the tank the moment it
    // respawns.
    if let Some(me) = state.local() {
        if me.vehicle(input.controlling).is_none() {
            let next = input.controlling.next_available(|slot| me.vehicle(slot).is_some());
            if next != input.controlling {
                input.controlling = next;
                state.set_predicted_slot(next);
            }
        }
    }

    // Build menu. Closing it with Escape is handled in `menu::toggle`, which
    // owns that key so the two menus cannot both react to one press.
    if keys.just_pressed(KeyCode::KeyB) {
        input.build_menu = !input.build_menu;
    }
    if input.build_menu && net.link == Link::Connected {
        for (i, key) in BUY_KEYS.iter().enumerate() {
            if keys.just_pressed(*key) {
                if let Some(powerup) = PowerUp::ALL.get(i) {
                    net.purchase(*powerup);
                }
            }
        }
    }

    // Aim the turret wherever the cursor is over the ground.
    if let Some(ground) = cursor_ground_position(&windows, &cameras) {
        let origin = state
            .local()
            .and_then(|p| p.vehicle(input.controlling).map(|v| v.pos))
            .unwrap_or(SimVec2::ZERO);
        let offset = ground - origin;
        if offset.length_squared() > 1.0 {
            input.aim = offset.to_angle();
        }
    }
}

/// Projects the mouse cursor onto the ground plane.
fn cursor_ground_position(
    windows: &Query<&Window, With<PrimaryWindow>>,
    cameras: &Query<(&Camera, &GlobalTransform)>,
) -> Option<SimVec2> {
    let window = windows.iter().next()?;
    let cursor = window.cursor_position()?;
    let (camera, transform) = cameras.iter().next()?;
    let ray = camera.viewport_to_world(transform, cursor).ok()?;

    // Intersect with y = GROUND_Y. A ray parallel to the ground never meets it.
    let dir_y = ray.direction.y;
    if dir_y.abs() < 1e-6 {
        return None;
    }
    let t = (coords::GROUND_Y - ray.origin.y) / dir_y;
    if t <= 0.0 {
        return None;
    }
    Some(coords::world_to_sim(ray.origin + *ray.direction * t))
}

/// Sends this tick's input and advances local prediction with it.
///
/// Runs in `FixedUpdate` at the simulation rate, so the client integrates
/// exactly the same number of steps, with the same `dt`, as the server does.
pub fn send_input(
    mut input: ResMut<LocalInput>,
    mut net: ResMut<NetClient>,
    mut state: ResMut<GameState>,
) {
    if net.link != Link::Connected {
        return;
    }
    input.tick = input.tick.wrapping_add(1);
    let frame = InputFrame {
        tick: input.tick,
        controlling: input.controlling,
        throttle: input.throttle,
        steer: input.steer,
        aim: input.aim,
        fire_primary: input.fire_primary,
        fire_secondary: input.fire_secondary,
    };
    input.current = frame;
    net.send_input(frame);

    let powerups = state.local().map_or(0, |p| p.powerups);
    // Reborrowed as a plain reference first: going through `ResMut`'s
    // `DerefMut` would borrow the whole resource, where this borrows two
    // fields the compiler can see are different.
    let state = &mut *state;
    state.prediction.apply(frame, powerups, &state.hills);
}
