//! Orewar client, as a library.
//!
//! The whole game lives here so that more than one entry point can start it.
//! `main.rs` is the desktop one: it reads a command line and runs. Android has
//! no command line and loads a shared library instead, so it builds [`Args`]
//! from what the device can tell it and calls [`build_app`] the same way.
//!
//! The client renders and predicts; it never decides anything. Every outcome
//! comes from the server.

pub mod camera;
pub mod connect;
pub mod coords;
pub mod effects;
pub mod field;
pub mod hud;
pub mod input;
pub mod menu;
pub mod miner_panel;
pub mod net;
pub mod sentinels;
pub mod state;
pub mod vehicles;

use std::path::{Path, PathBuf};

use bevy::prelude::*;
use orewar_shared::rng::Rng;
use orewar_shared::world::TICK_HZ;

use connect::{AppState, ConnectForm};
use input::LocalInput;
use menu::MenuState;
use net::NetClient;
use state::GameState;

/// What the command line had to say, all of it optional: the connect screen
/// fills in whatever was left out.
pub struct Args {
    pub server: Option<String>,
    pub name: Option<String>,
    pub token: Option<u64>,
    pub token_file: Option<PathBuf>,
}

/// Derives a stable token from a name, so `--name Ash` always returns to the
/// same player slot without needing a file on disk.
pub fn token_from_name(name: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    // Run it through the mixer so similar names land far apart.
    Rng::new(hash).next_u64()
}

/// Reads the token file, creating it with a fresh random token if absent.
pub fn token_from_file(path: &Path) -> u64 {
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(token) = text.trim().parse::<u64>() {
            return token;
        }
    }
    let token = Rng::seed_from_time();
    if let Err(e) = std::fs::write(path, token.to_string()) {
        warn!("could not save identity to {}: {e}", path.display());
    }
    token
}

/// Assembles the game, ready to `run`.
///
/// Built rather than run so that a caller who has to own the event loop --
/// which is every platform that is not a plain desktop process -- can still get
/// exactly the same app.
pub fn build_app(args: Args) -> App {
    let mut app = App::new();

    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Orewar".into(),
            // Logical pixels, so on a HiDPI display the physical window is
            // larger by the scale factor. Keep the default modest enough to
            // fit a 1280x720 logical desktop; the window is resizable.
            resolution: (1180, 680).into(),
            position: WindowPosition::At(IVec2::new(30, 30)),
            ..default()
        }),
        ..default()
    }))
    .insert_resource(ClearColor(field::SKY_COLOR))
    // Prediction must advance in the same sized steps as the server's
    // simulation, or it drifts from authority every single tick.
    .insert_resource(Time::<Fixed>::from_hz(TICK_HZ as f64))
    .init_state::<AppState>()
    .insert_resource(ConnectForm::new(args))
    .init_resource::<GameState>()
    .init_resource::<LocalInput>()
    .init_resource::<MenuState>()
    .init_resource::<camera::Overview>()
    .init_resource::<camera::CameraRig>()
    .add_message::<camera::CameraNudge>()
    .add_systems(
        Startup,
        (
            field::setup,
            vehicles::setup,
            camera::setup,
            hud::setup,
            menu::setup,
            miner_panel::setup,
            sentinels::setup,
            effects::setup,
            connect::setup,
        ),
    )
    // After `vehicles::setup`, which is where the mesh it draws is built.
    .add_systems(Startup, vehicles::setup_bombsight.after(vehicles::setup))
    // Networking runs before anything reads state, so a frame always sees
    // the freshest snapshot that has arrived. It runs on the connect screen
    // too: that is where the handshake it drives begins.
    .add_systems(PreUpdate, net::poll.run_if(resource_exists::<NetClient>))
    .add_systems(
        Update,
        (connect::update, connect::paint).chain().run_if(in_state(AppState::Connect)),
    )
    .add_systems(
        FixedUpdate,
        input::send_input.run_if(in_state(AppState::Playing)),
    )
    .add_systems(
        Update,
        (
            // The menu takes Escape first and gates the controls, so it
            // has to settle before input is gathered.
            menu::toggle,
            // The overview settles first: while it is up the controls are
            // gated, and gathering before that would drive a vehicle for a
            // frame after the camera had already left it.
            camera::overview_controls,
            input::gather,
            menu::update,
            state::interpolate_system,
            (
                vehicles::sync_vehicles,
                vehicles::sync_projectiles,
                vehicles::sync_bombsight,
                field::sync_ore,
                field::sync_hills,
                sentinels::sync,
                camera::follow,
                hud::update_texts,
                hud::update_bars,
                hud::update_panels,
                hud::update_radar,
                miner_panel::update,
            ),
            // After the render view is rebuilt: a shield arc rides the
            // interpolated hull, not the one from last frame.
            effects::spawn,
            effects::animate,
            quit_on_request,
        )
            .chain()
            .run_if(in_state(AppState::Playing)),
    );

    app
}

/// Leaves the match cleanly on Ctrl-Q, for players who never open the menu.
fn quit_on_request(keys: Res<ButtonInput<KeyCode>>, mut net: ResMut<NetClient>) {
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if ctrl && keys.just_pressed(KeyCode::KeyQ) {
        net::leave_and_exit(&mut net);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_maps_to_a_stable_identity() {
        assert_eq!(token_from_name("Ash"), token_from_name("Ash"));
        assert_ne!(token_from_name("Ash"), token_from_name("Bo"));
    }
}
