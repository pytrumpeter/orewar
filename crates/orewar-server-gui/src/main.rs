//! Orewar server, with a window.
//!
//! The same match the dedicated server hosts, for a host who would rather have
//! a button than a terminal: it asks for a port and a seed, then shows the
//! address to give players, who is in the match, and what is happening in it.
//!
//! The match runs on its own thread at its own rate -- see [`hosting`] -- so
//! nothing here decides anything about the game. This is a window onto it.

// A console window behind this one has nothing to show: everything the server
// says is on screen. Debug builds keep it, since that is where a panic goes.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod hosting;
mod ui;

use std::time::Duration;

use bevy::prelude::*;
use bevy::winit::{UpdateMode, WinitSettings};

use hosting::Hosting;
use ui::{Live, Screen, SetupForm};

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Orewar Server".into(),
                resolution: (620, 700).into(),
                position: WindowPosition::At(IVec2::new(60, 60)),
                ..default()
            }),
            ..default()
        }))
        // A status panel has no business spending a core on redrawing itself
        // sixty times a second next to the match it is hosting. Four times a
        // second is faster than anything on it changes.
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(Duration::from_millis(250)),
            unfocused_mode: UpdateMode::reactive_low_power(Duration::from_millis(500)),
        })
        .insert_resource(ClearColor(ui::BACKDROP))
        .init_state::<Screen>()
        .init_resource::<SetupForm>()
        .init_resource::<Live>()
        .add_systems(Startup, ui::setup)
        .add_systems(Update, ui::setup_screen.run_if(in_state(Screen::Setup)))
        .add_systems(
            Update,
            (ui::poll, ui::running_screen)
                .chain()
                .run_if(in_state(Screen::Running).and_then(resource_exists::<Hosting>)),
        )
        // After both, so a frame paints what this frame's input and this
        // frame's snapshot say rather than the last one's.
        .add_systems(Update, (ui::show, ui::paint).chain().after(ui::setup_screen).after(ui::poll))
        .run();
}
