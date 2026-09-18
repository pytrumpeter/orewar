//! A finger, told to the game as if it were the mouse.
//!
//! The client was written for a keyboard and a mouse, and it reads the mouse in
//! four places: the connect screen's button, the pause menu's items, the aim
//! that follows the cursor, and the primary trigger. None of them see a touch,
//! because a touch sets no [`MouseButton`] and moves no cursor -- the connect
//! screen would light up under a finger and never actually open.
//!
//! Rather than teach each of those four places about touch, this puts the touch
//! where they already look. One finger becomes the cursor and the left button,
//! and every existing path works unchanged. Aim-at-cursor becomes aim-at-touch
//! for nothing, which is the mapping a tablet wanted anyway.
//!
//! What this deliberately does not do is invent a right button. Overview pan
//! and the secondary trigger both read one, and guessing a gesture for them
//! here would be a control decision made in a compatibility shim. They get a
//! real answer later.

use bevy::input::InputSystems;
use bevy::input::touch::{TouchInput, TouchPhase};
use bevy::prelude::*;
use bevy::window::PrimaryWindow;

pub struct TouchAsMousePlugin;

impl Plugin for TouchAsMousePlugin {
    fn build(&self, app: &mut App) {
        // In `PreUpdate`, after the input systems: they clear last frame's
        // `just_pressed` edges, so a press written before them is wiped before
        // anything in `Update` can see it. After them, the edge survives the
        // frame, which is exactly how a real button behaves.
        app.init_resource::<PrimaryFinger>()
            .add_systems(PreUpdate, drive_mouse_with_touch.after(InputSystems));
    }
}

/// The finger the mouse is following.
///
/// A tablet reports every finger on the glass, and a second one landing while
/// the first is held would otherwise drag the cursor away mid-shot. The first
/// finger down owns the mouse until it lifts; the rest are ignored.
#[derive(Resource, Default)]
struct PrimaryFinger(Option<u64>);

fn drive_mouse_with_touch(
    mut touches: MessageReader<TouchInput>,
    mut finger: ResMut<PrimaryFinger>,
    mut buttons: ResMut<ButtonInput<MouseButton>>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    let Ok(mut window) = windows.single_mut() else {
        return;
    };

    for touch in touches.read() {
        match touch.phase {
            TouchPhase::Started => {
                if finger.0.is_some() {
                    continue;
                }
                finger.0 = Some(touch.id);
                // Placed before the press, so whatever reads the click this
                // frame reads it at the spot that was actually touched.
                window.set_cursor_position(Some(touch.position));
                buttons.press(MouseButton::Left);
            }
            TouchPhase::Moved => {
                if finger.0 == Some(touch.id) {
                    window.set_cursor_position(Some(touch.position));
                }
            }
            TouchPhase::Ended | TouchPhase::Canceled => {
                if finger.0 == Some(touch.id) {
                    finger.0 = None;
                    buttons.release(MouseButton::Left);
                    // The cursor stays where the finger left it. Aim is read
                    // from it every frame, and a turret that snapped back to a
                    // corner on release would be worse than one that holds --
                    // which is what the client already does when input stops.
                }
            }
        }
    }
}
