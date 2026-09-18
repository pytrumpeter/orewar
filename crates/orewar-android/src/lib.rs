//! Orewar on Android.
//!
//! There is no process to start and no command line to read, so this is the
//! whole of the platform's entry point: work out where the device will let the
//! game keep a file, hand that to the same [`build_app`] the desktop uses, and
//! run it.
//!
//! Nothing about the game lives here. If something needs to behave differently
//! on a tablet it belongs in a system added to the app below, not in a fork of
//! the client.
//!
//! # Telling a tablet where to play, before it can be typed at
//!
//! The connect screen wants an address and a name, and until the soft keyboard
//! is wired up there is no way to type either. There is no need to compile one
//! in: the client reads `.orewar-connect` from beside its token file, which is
//! the private directory named below, so two lines pushed onto the device fill
//! the screen in and the touch shim presses the button.
//!
//! ```text
//! printf '192.168.1.50:45701\nAsh\n' > connect.txt
//! adb push connect.txt /data/local/tmp/
//! adb shell run-as com.orewar.client \
//!     cp /data/local/tmp/connect.txt files/.orewar-connect
//! ```
//!
//! `run-as` is what reaches private storage, and it works because the APK is
//! built debuggable. Note this fills the screen in and stops: an address the
//! device was handed is still not an address anybody confirmed, so the button
//! is a tap away rather than pressed automatically.

#[cfg(target_os = "android")]
use std::path::PathBuf;

use bevy::prelude::*;
use orewar_client::{Args, build_app};

mod controls;

/// Android's entry point.
///
/// `#[bevy_main]` generates the `android_main(AndroidApp)` the activity calls,
/// which stashes the app handle where `bevy_winit` will find it and then calls
/// this.
///
/// No address is named here, and none ever should be. Where to play is a
/// question for the person holding the tablet, asked on the connect screen the
/// same as anywhere else; an address compiled into the binary would be a guess
/// about somebody else's network that goes stale the first time a router hands
/// out different numbers. The only thing this hands the client is where the
/// device will let it keep a file.
#[bevy_main]
fn main() {
    let args =
        Args { server: None, name: None, token: None, token_file: token_path() };

    build_app(args).add_plugins(controls::TouchControlsPlugin).run();
}

/// Where the identity token is kept.
///
/// The client's default is `.orewar-token`, relative to the working directory.
/// An Android process starts in a directory it may not write to, so the token
/// has to be named explicitly and put in the private storage the app is given.
#[cfg(target_os = "android")]
fn token_path() -> Option<PathBuf> {
    let app = bevy::android::ANDROID_APP.get()?;
    Some(app.internal_data_path()?.join("orewar-token"))
}

/// Off Android there is nowhere special to put it, so the client's own default
/// stands. This exists only so the crate type-checks on a desktop.
#[cfg(not(target_os = "android"))]
fn token_path() -> Option<std::path::PathBuf> {
    None
}
