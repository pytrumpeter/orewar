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
    /// A sortie has been asked for and has not arrived yet.
    ///
    /// The launch key cannot hand over the controls on the spot, because the
    /// aircraft does not exist here until the server says so -- and the
    /// fallback below, which keeps the player on something they actually have,
    /// would bounce them straight back off a slot that is still empty. So the
    /// intent is remembered and spent the moment the sortie shows up.
    pub awaiting_sortie: bool,
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
            awaiting_sortie: false,
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
const LAUNCH_KEY: KeyCode = KeyCode::KeyG;

/// Turns cheat mode on or off, held with either Alt.
///
/// Behind a modifier on purpose: it changes the rules for everybody in the
/// match, so it should not be one letter away from the keys used to drive.
///
/// Private, along with [`LAUNCH_KEY`], so that a binding whose handler goes
/// missing is an unused-constant warning rather than silence. Published for no
/// reason, that is precisely how this one was lost once already.
const CHEAT_KEY: KeyCode = KeyCode::KeyC;

/// How much of the controls is live, given what else is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controls {
    /// Whether the vehicle can be driven and fired at all.
    pub live: bool,
    /// Whether the mouse buttons pull the triggers. The keyboard ones always
    /// do while the controls are live.
    pub mouse_fires: bool,
}

/// Works out the two answers above.
///
/// A plain function rather than a couple of conditions inside [`gather`], which
/// would need a whole Bevy world to exercise. What it decides is a rule about
/// the game and worth pinning down on its own.
pub fn controls_for(menu_open: bool, overview_active: bool) -> Controls {
    Controls {
        // Only the pause menu takes the controls away. The overview does not:
        // an aircraft keeps flying whether or not you are watching it.
        live: !menu_open,
        // Up in the overview the mouse belongs to the camera and the panels --
        // right-drag pans the map, and the miner's mode buttons float over
        // it -- so the triggers come off the mouse while it is up.
        mouse_fires: !menu_open && !overview_active,
    }
}

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
    // With the menu up the vehicle coasts: it should not carry on driving on
    // whatever was held at the time, and a click on a menu item is not a
    // trigger pull.
    //
    // The pause menu is the only thing that takes the controls away. Raising
    // the overview used to as well, on the reasoning that looking at the whole
    // field meant not fighting while you did it -- but an aircraft keeps flying
    // whether or not you are watching it, and a fuel clock does not stop for
    // the camera, so being unable to fly while looking is a way to lose a
    // sortie rather than a considered restriction.
    //
    // Aim is left where it was, exactly as it is when input stops arriving at
    // all -- a turret that snaps is worse than one that waits.
    // Cheat mode, for everybody. Sent rather than applied: the server owns the
    // rules, and the flag comes back on the next snapshot.
    //
    // Above the gate deliberately. It is not a control -- it changes what the
    // match is -- so it should work whatever else happens to be on screen.
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if alt && keys.just_pressed(CHEAT_KEY) && net.link == Link::Connected {
        net.toggle_cheats();
    }

    let controls = controls_for(menu.open, overview.active);
    if !controls.live {
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

    // Space and F are already the keyboard way to fire, which is what makes
    // taking the triggers off the mouse in the overview cost nothing: up there
    // the keyboard flies and fights, and the mouse looks.
    input.fire_primary = (controls.mouse_fires && buttons.pressed(MouseButton::Left))
        || keys.pressed(KeyCode::Space);
    input.fire_secondary = (controls.mouse_fires && buttons.pressed(MouseButton::Right))
        || keys.pressed(KeyCode::KeyF);

    // What to drive by the end of this frame, if anything below changes it.
    let mut take: Option<VehicleSlot> = None;

    // Call up a sortie. Reliable, like a purchase, and only sent when it can
    // actually be granted -- the client has the upgrade mask and the cooldown
    // in every snapshot, so a key that would be declined is a key that does
    // nothing here rather than a request the server quietly drops.
    if keys.just_pressed(LAUNCH_KEY) && net.link == Link::Connected {
        // One key, one meaning: get me to the bomber. With a sortie already up
        // that is just a switch; without one it is a request, and the switch
        // happens when it arrives.
        let already_up = state.local().is_some_and(|me| me.plane.is_some());
        if already_up {
            take = Some(VehicleSlot::Plane);
        } else if state.local().is_some_and(|me| me.sortie_ready()) {
            net.launch_plane();
            input.awaiting_sortie = true;
        }
    }

    // The sortie asked for above has arrived: take it.
    if input.awaiting_sortie && state.local().is_some_and(|me| me.plane.is_some()) {
        take = Some(VehicleSlot::Plane);
        input.awaiting_sortie = false;
    }

    // Swap which vehicle you are driving. The cycle only stops on what you
    // actually have, so it is two vehicles until a sortie is up and three
    // while it is, rather than a third stop that is empty most of a match.
    if keys.just_pressed(KeyCode::Tab) {
        if let Some(me) = state.local() {
            take = Some(input.controlling.next_available(|slot| me.vehicle(slot).is_some()));
        }
        // Choosing something by hand is the player saying where they want to
        // be, so a sortie still in the post no longer gets to move them.
        input.awaiting_sortie = false;
    }

    // Follow the vehicle you actually have. A destroyed tank leaves nothing
    // to drive, and an aircraft running dry takes the controls out from under
    // you mid-flight with nothing to announce it. The server already falls
    // through, but prediction and the turret still have to be pointed at the
    // right hull or the miner moves under you while the camera and aim
    // stay behind. Coming back the same way puts you in the tank the moment it
    // respawns.
    if let Some(me) = state.local() {
        if me.vehicle(input.controlling).is_none() {
            take = Some(input.controlling.next_available(|slot| me.vehicle(slot).is_some()));
        }
    }

    // Applied in one place at the end: every route above decides *what* to
    // drive, and prediction has to be restarted exactly once for whatever wins.
    if let Some(slot) = take {
        if slot != input.controlling {
            input.controlling = slot;
            state.set_predicted_slot(slot);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Raising the overview must not put the vehicle down.
    ///
    /// It used to: the camera lifting off the hull zeroed the throttle, the
    /// steering and both triggers, on the reasoning that looking at the whole
    /// field meant not fighting while you did it. That reasoning does not
    /// survive an aircraft. A sortie keeps flying and keeps burning fuel
    /// whether or not anybody is watching it, so being unable to fly while
    /// looking is a way to lose one rather than a considered restriction --
    /// and the same key that lifts the camera is the one you want while
    /// working out where to go next.
    ///
    /// What the overview does take is the mouse, because up there it already
    /// has two jobs: the right button drags the map and the left one reaches
    /// the miner's mode buttons floating over it. Neither should also pull
    /// a trigger. Nothing is lost by it -- `Space` and `F` fire.
    #[test]
    fn only_the_pause_menu_takes_the_controls_away() {
        let playing = controls_for(false, false);
        assert!(playing.live);
        assert!(playing.mouse_fires, "the mouse fires when nothing else wants it");

        let looking = controls_for(false, true);
        assert!(looking.live, "the overview put the vehicle down");
        assert!(!looking.mouse_fires, "dragging the map would also have fired");

        // The menu is the one thing that stops everything, and it stops it
        // whether or not the overview happens to be up behind it.
        for overview in [false, true] {
            let paused = controls_for(true, overview);
            assert!(!paused.live, "the menu left the controls live");
            assert!(!paused.mouse_fires, "a click on a menu item was a trigger pull");
        }
    }
}
