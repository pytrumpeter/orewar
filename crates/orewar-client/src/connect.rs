//! The connect screen: where a match is joined.
//!
//! The client used to take its server and its name from the command line and
//! settle the handshake before a window ever opened. A refusal was a line on a
//! terminal, which is no use at all to somebody who started the game by
//! double-clicking it. This asks the same two questions on screen -- who to
//! join, and under what name -- and shows whatever the server answers with the
//! fields still there to correct.
//!
//! The handshake itself is the one in [`crate::net`], unchanged: it already
//! retries, reports silence, and handles a refusal. [`crate::net::poll`] runs
//! in both states, and this screen only starts a handshake and reads how it
//! went.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::ui::FocusPolicy;
use orewar_shared::protocol::DenyReason;
use orewar_shared::world::DEFAULT_PORT;

use crate::hud::{TEXT, TEXT_DIM};
use crate::net::{Link, NetClient};
use crate::{Args, token_from_file, token_from_name};

/// How long a handshake goes unanswered before the screen says so.
///
/// UDP gives nothing to observe: a wrong address, a closed port, and a server
/// that was never started all look exactly like waiting. Matches the interval
/// [`crate::net`] uses for the same warning on the terminal.
const QUIET: f64 = 4.0;

/// What the last successful server and name are kept in, so a second session
/// does not have to type them again. Local to one machine, like the token it
/// sits beside; [`remembered_beside`] decides which directory that is.
const REMEMBERED: &str = ".orewar-connect";

/// Longest name the screen accepts. The wire truncates at 255 bytes, but a
/// roster row has room for far less than that, and a name nobody can read on
/// the scoreboard is not a name.
const NAME_LIMIT: usize = 20;
/// Longest address the screen accepts; comfortably past any host name.
const ADDRESS_LIMIT: usize = 60;

const BACKDROP: Color = Color::srgb(0.02, 0.04, 0.06);
const PANEL_BG: Color = Color::srgba(0.05, 0.08, 0.11, 0.98);
const FIELD_BG: Color = Color::srgba(1.0, 1.0, 1.0, 0.05);
const EDGE: Color = Color::srgba(0.5, 0.6, 0.7, 0.35);
const EDGE_FOCUS: Color = Color::srgb(0.45, 0.68, 0.92);
const TROUBLE: Color = Color::srgb(0.95, 0.55, 0.42);

/// Which screen the client is on.
///
/// Every system that drives, draws, or reads the match runs only in
/// [`AppState::Playing`], which is also the only state in which a
/// [`NetClient`] is guaranteed to exist.
#[derive(States, Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AppState {
    #[default]
    Connect,
    Playing,
}

/// One of the two things a player types.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    Server,
    Name,
}

impl FieldId {
    fn label(self) -> &'static str {
        match self {
            FieldId::Server => "Server",
            FieldId::Name => "Name",
        }
    }

    fn other(self) -> FieldId {
        match self {
            FieldId::Server => FieldId::Name,
            FieldId::Name => FieldId::Server,
        }
    }
}

/// Text widgets on this screen, identified rather than separately marked, for
/// the reason given in [`crate::hud`]: two `Query<&mut Text>` in one system are
/// more trouble than one query and a `match`.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum ConnectText {
    Value(FieldId),
    Status,
    Button,
}

/// Marks the full-screen overlay, which goes away for good once a match starts.
#[derive(Component)]
struct ConnectRoot;

/// The one button that connects.
///
/// Marked rather than found by `With<Button>`, which would also find the
/// miner's mode buttons behind the overlay and read a click on empty space as
/// a press of this one.
#[derive(Component)]
pub struct ConnectButton;

/// A handshake in flight.
struct Attempt {
    addr: SocketAddr,
    /// When it started, on the render clock, for the silence warning.
    started: f64,
}

#[derive(Resource)]
pub struct ConnectForm {
    server: String,
    name: String,
    focus: FieldId,
    /// The line under the fields: what is happening, or what went wrong.
    status: String,
    /// Whether that line is a complaint, which is painted differently.
    trouble: bool,
    attempt: Option<Attempt>,
    /// An identity given explicitly on the command line, which outranks the
    /// name in the box.
    token: Option<u64>,
    token_file: Option<PathBuf>,
    /// Where the last server and name are kept, worked out once at startup so
    /// that writing them back lands wherever reading them came from.
    remembered_at: PathBuf,
    /// Set when the command line already said where to go: the screen submits
    /// itself on the first frame rather than making a scripted launch stop and
    /// press a button.
    auto: bool,
}

impl ConnectForm {
    /// Fills the form from the command line, then from what was typed last
    /// time, then from the host's own account name -- so the commonest case is
    /// pressing Enter on a screen that already says the right thing.
    pub fn new(args: Args) -> Self {
        let remembered_at = remembered_beside(args.token_file.as_deref());
        let remembered = remembered(&remembered_at);
        let given_address = args.server.is_some();
        let server = args
            .server
            .or_else(|| remembered.as_ref().map(|(s, _)| s.clone()))
            .unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_PORT}"));
        let name = args
            .name
            .clone()
            .or_else(|| remembered.map(|(_, n)| n))
            .unwrap_or_else(default_name);
        let auto = skips_the_screen(given_address, &name);
        ConnectForm {
            server,
            name,
            focus: FieldId::Name,
            status: String::new(),
            trouble: false,
            attempt: None,
            token: args.token,
            token_file: args.token_file,
            remembered_at,
            auto,
        }
    }

    fn field_mut(&mut self, which: FieldId) -> &mut String {
        match which {
            FieldId::Server => &mut self.server,
            FieldId::Name => &mut self.name,
        }
    }

    fn field(&self, which: FieldId) -> &str {
        match which {
            FieldId::Server => &self.server,
            FieldId::Name => &self.name,
        }
    }

    fn limit(which: FieldId) -> usize {
        match which {
            FieldId::Server => ADDRESS_LIMIT,
            FieldId::Name => NAME_LIMIT,
        }
    }

    /// The identity this form would connect under.
    ///
    /// Precedence matches what the command line has always done: an explicit
    /// token wins, then a token file, then the name -- which is what lets two
    /// windows on one machine be two players -- and finally the token file that
    /// is written for a player who never gave a name.
    fn identity(&self) -> u64 {
        let name = self.name.trim();
        match (self.token, &self.token_file) {
            (Some(token), _) => token,
            (None, Some(path)) => token_from_file(path),
            (None, None) if !name.is_empty() => token_from_name(name),
            (None, None) => token_from_file(Path::new(".orewar-token")),
        }
    }
}

/// Whether a launch has already said enough to go straight to the field.
///
/// Only an address does that. An identity -- a token, or a token file -- says
/// who you are and never where to go, so on its own it is not an answer to
/// anything this screen asks; a build that hands one over as a matter of
/// course, the way the Android one does to keep its token in private storage,
/// would otherwise dial the default address without a soul having asked it to.
/// That default is this machine, which on a tablet is the tablet, where nothing
/// is listening.
///
/// A name alone is not enough either, for the same reason in a quieter form:
/// acting on it would mean assuming an address, and the address is the one
/// thing this screen exists to be told.
///
/// The name still has to be there, because a launch that skips the screen skips
/// the chance to notice it is joining as nobody.
fn skips_the_screen(server_given: bool, name: &str) -> bool {
    server_given && !name.trim().is_empty()
}

/// The player's own account name, as a first guess at what to call them.
fn default_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .map(|n| n.chars().take(NAME_LIMIT).collect())
        .unwrap_or_default()
}

/// Where the remembered server and name live.
///
/// Beside the identity, whenever the identity was given a home of its own. The
/// two belong together -- both say something about this device rather than
/// about the match -- and a platform that has to put the token somewhere
/// specific has to put this somewhere specific for exactly the same reason.
/// Android is the case in point: its working directory is not writable, so
/// without this the screen could never remember anything.
///
/// It is also how a tablet gets told where to play before it can be typed at.
/// Two lines pushed onto the device fill the screen in, and the button does the
/// rest -- no address compiled into the build, and no keyboard needed.
fn remembered_beside(token_file: Option<&Path>) -> PathBuf {
    match token_file.and_then(Path::parent) {
        // A bare file name has an empty parent, which would turn an absolute
        // answer into a relative one.
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(REMEMBERED),
        _ => PathBuf::from(REMEMBERED),
    }
}

/// Reads the remembered server and name: address on the first line, name on
/// the second. Absent or unreadable is simply "nothing remembered".
fn remembered(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let server = lines.next()?.trim().to_string();
    let name = lines.next().unwrap_or_default().trim().to_string();
    (!server.is_empty()).then_some((server, name))
}

/// Keeps what worked, for the next launch. Failing to write is not worth
/// interrupting a match that has just started.
fn remember(path: &Path, server: &str, name: &str) {
    let _ = std::fs::write(path, format!("{server}\n{name}\n"));
}

/// Adds a typed character to a field, dropping anything that is not text and
/// holding the field to its limit.
///
/// Enter and Tab arrive with text of their own -- `"\r"` and `"\t"` -- so the
/// control-character test is what stops a submitted form from also gaining a
/// character nobody typed.
fn type_into(field: &mut String, text: &str, limit: usize) {
    for ch in text.chars() {
        if ch.is_control() || field.chars().count() >= limit {
            continue;
        }
        field.push(ch);
    }
}

/// Picks which resolved address to talk to, preferring IPv4.
///
/// `localhost` resolves to `::1` before `127.0.0.1` on Windows, and the server
/// binds `0.0.0.0` by default -- IPv4 only. Taking the resolver's first answer
/// therefore sent every packet to an IPv6 loopback nothing was listening on.
/// Nothing reports that: UDP has no connection to refuse, so the client simply
/// waits forever on a handshake that cannot arrive.
///
/// An explicit IPv6 address still works; this only decides ties.
fn prefer_ipv4(resolved: &[SocketAddr]) -> Option<SocketAddr> {
    resolved.iter().find(|a| a.is_ipv4()).or_else(|| resolved.first()).copied()
}

/// Turns what was typed into an address, supplying the default port for a bare
/// host and the loopback for an empty box.
pub fn resolve(typed: &str) -> Result<SocketAddr, String> {
    let mut text = typed.trim().to_string();
    if text.is_empty() {
        text = "127.0.0.1".to_string();
    }
    // Supply the default port to anything typed without one. A colon does not
    // settle that on its own: an IPv6 address is made of them, and only
    // carries a port when it is bracketed and one follows the bracket.
    text = match text.split_once(']') {
        // Bracketed: `[::1]` wants a port, `[::1]:45701` already has one.
        Some((_, after)) => {
            if after.starts_with(':') { text.clone() } else { format!("{text}:{DEFAULT_PORT}") }
        }
        // Bare, with colons to spare: an IPv6 literal, which has to be
        // bracketed before a port can be put on it.
        None if text.matches(':').count() > 1 => format!("[{text}]:{DEFAULT_PORT}"),
        None if !text.contains(':') => format!("{text}:{DEFAULT_PORT}"),
        None => text.clone(),
    };
    let resolved: Vec<SocketAddr> = text
        .to_socket_addrs()
        .map_err(|_| format!("Cannot find {}", typed.trim()))?
        .collect();
    prefer_ipv4(&resolved).ok_or_else(|| format!("No address for {}", typed.trim()))
}

pub fn setup(mut commands: Commands) {
    commands
        .spawn((
            ConnectRoot,
            // Gone once a match begins. The screen is never returned to: a
            // player who leaves leaves the process.
            DespawnOnEnter(AppState::Playing),
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            // Nothing behind this is clickable. The HUD's own buttons are
            // still there under it, and a click that reached one would press
            // something the player cannot see.
            FocusPolicy::Block,
            // Opaque, not a scrim: the HUD is built at startup like everything
            // else, and a player choosing a server should not be reading a
            // scoreboard for a match they have not joined.
            BackgroundColor(BACKDROP),
            // Above the menu, which is at 10.
            GlobalZIndex(20),
        ))
        .with_children(|screen| {
            screen
                .spawn((
                    Node {
                        width: px(400),
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::all(px(22)),
                        row_gap: px(6),
                        border: UiRect::all(px(1)),
                        border_radius: BorderRadius::all(px(10)),
                        ..default()
                    },
                    BackgroundColor(PANEL_BG),
                    BorderColor::all(EDGE),
                ))
                .with_children(|panel| {
                    panel.spawn((
                        Text::new("OREWAR"),
                        TextFont { font_size: FontSize::Px(26.0), ..default() },
                        TextColor(TEXT),
                    ));
                    panel.spawn((
                        Node { margin: UiRect { bottom: px(16), ..default() }, ..default() },
                        Text::new("join a match"),
                        TextFont { font_size: FontSize::Px(12.0), ..default() },
                        TextColor(TEXT_DIM),
                    ));

                    for field in [FieldId::Server, FieldId::Name] {
                        spawn_field(panel, field);
                    }

                    panel.spawn((
                        ConnectText::Status,
                        Node {
                            margin: UiRect { top: px(10), bottom: px(4), ..default() },
                            ..default()
                        },
                        Text::new(String::new()),
                        TextFont { font_size: FontSize::Px(12.0), ..default() },
                        TextColor(TEXT_DIM),
                    ));

                    panel.spawn((
                        ConnectButton,
                        Button,
                        Node {
                            margin: UiRect { top: px(6), ..default() },
                            padding: UiRect::axes(px(12), px(9)),
                            justify_content: JustifyContent::Center,
                            border: UiRect::all(px(1)),
                            border_radius: BorderRadius::all(px(6)),
                            ..default()
                        },
                        BackgroundColor(FIELD_BG),
                        BorderColor::all(EDGE),
                        children![(
                            ConnectText::Button,
                            Text::new("Connect"),
                            TextFont { font_size: FontSize::Px(16.0), ..default() },
                            TextColor(TEXT),
                        )],
                    ));

                    panel.spawn((
                        Node { margin: UiRect { top: px(14), ..default() }, ..default() },
                        // Broken into lines the panel does not have to wrap:
                        // a hint that reflows itself reads as an accident.
                        Text::new(
                            "Tab or click to switch  |  Enter to connect\n\
                             A name is your identity here. It is what takes\n\
                             you back to your own ore and vehicles after a\n\
                             drop, so no two players can share one.",
                        ),
                        TextFont { font_size: FontSize::Px(11.0), ..default() },
                        TextColor(TEXT_DIM),
                    ));
                });
        });
}

/// One labelled box the player types into.
fn spawn_field(panel: &mut ChildSpawnerCommands, field: FieldId) {
    panel
        .spawn((
            field,
            // Clicking a box moves the caret to it, which is the one thing a
            // player will try without being told.
            Button,
            Node {
                flex_direction: FlexDirection::Column,
                margin: UiRect { bottom: px(10), ..default() },
                padding: UiRect::axes(px(10), px(7)),
                row_gap: px(3),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            BackgroundColor(FIELD_BG),
            BorderColor::all(EDGE),
        ))
        .with_children(|boxed| {
            boxed.spawn((
                Text::new(field.label()),
                TextFont { font_size: FontSize::Px(10.0), ..default() },
                TextColor(TEXT_DIM),
            ));
            boxed.spawn((
                ConnectText::Value(field),
                Text::new(String::new()),
                TextFont { font_size: FontSize::Px(16.0), ..default() },
                TextColor(TEXT),
            ));
        });
}

/// Reads the keyboard and the mouse, starts a handshake, and watches how it
/// goes.
pub fn update(
    mut commands: Commands,
    mut typed: MessageReader<KeyboardInput>,
    buttons: Res<ButtonInput<MouseButton>>,
    time: Res<Time>,
    mut form: ResMut<ConnectForm>,
    net: Option<Res<NetClient>>,
    mut next: ResMut<NextState<AppState>>,
    fields: Query<(&FieldId, &Interaction)>,
    connect_button: Query<&Interaction, With<ConnectButton>>,
) {
    let now = time.elapsed_secs_f64();
    let mut submit = false;
    let mut cancel = false;

    // The cursor picks a field on click, and the button is pressed rather than
    // released -- the screen is gone by the time a release lands.
    let hovering_connect = connect_button.iter().any(|i| *i != Interaction::None);
    if buttons.just_pressed(MouseButton::Left) {
        for (field, interaction) in &fields {
            if *interaction != Interaction::None {
                form.focus = *field;
            }
        }
        if hovering_connect {
            submit = true;
        }
    }

    for event in typed.read() {
        if event.state != ButtonState::Pressed {
            continue;
        }
        match &event.logical_key {
            Key::Enter => submit = true,
            Key::Escape => cancel = true,
            Key::Tab | Key::ArrowUp | Key::ArrowDown => form.focus = form.focus.other(),
            Key::Backspace => {
                // Ignored while connecting, along with everything else that
                // edits: the fields belong to the attempt until it is over.
                if form.attempt.is_none() {
                    let focus = form.focus;
                    form.field_mut(focus).pop();
                }
            }
            _ => {
                if form.attempt.is_some() {
                    continue;
                }
                if let Some(text) = &event.text {
                    let focus = form.focus;
                    let limit = ConnectForm::limit(focus);
                    type_into(form.field_mut(focus), text, limit);
                }
            }
        }
    }

    if cancel && form.attempt.is_some() {
        commands.remove_resource::<NetClient>();
        form.attempt = None;
        form.status = "Stopped trying".to_string();
        form.trouble = false;
        return;
    }

    if submit && form.attempt.is_none() {
        start(&mut commands, &mut form, now);
        return;
    }

    // A command line that already said where to go presses the button itself.
    if form.auto {
        form.auto = false;
        start(&mut commands, &mut form, now);
        return;
    }

    let Some(net) = net else { return };
    let Some(attempt) = &form.attempt else { return };
    match &net.link {
        Link::Connected => {
            remember(&form.remembered_at, form.server.trim(), form.name.trim());
            next.set(AppState::Playing);
        }
        Link::Denied(reason) => {
            let reason = *reason;
            commands.remove_resource::<NetClient>();
            form.attempt = None;
            form.status = refusal(reason);
            form.trouble = true;
        }
        Link::Connecting => {
            let addr = attempt.addr;
            let waiting = now - attempt.started;
            if waiting >= QUIET {
                // Nothing here is an error yet -- the server may still be
                // starting -- so the handshake keeps running underneath while
                // the screen says what is worth checking.
                // Left to the panel to wrap. A line break put in by hand only
                // lands where it was meant to at one window size.
                form.status = format!(
                    "No reply from {addr} after {waiting:.0}s. Is the server running, \
                     and is it listening on that address? A server on 0.0.0.0 cannot \
                     hear an IPv6 client. Esc to stop and edit.",
                );
                form.trouble = true;
            }
        }
    }
}

/// What to tell a player whose connection was refused.
fn refusal(reason: DenyReason) -> String {
    match reason {
        DenyReason::NameTaken => {
            "Somebody is already playing under that name. A name is an \
             identity here, so two clients cannot share one. Try another."
                .to_string()
        }
        other => format!("Refused: {}", other.describe()),
    }
}

/// Opens a socket and starts a handshake, or says why it could not.
fn start(commands: &mut Commands, form: &mut ConnectForm, now: f64) {
    // The two things the screen is for. Every way of leaving it comes through
    // here -- the button, Enter, and a command line that answered in advance --
    // so this is the one place both have to be true.
    //
    // A blank address would otherwise resolve to this machine and a blank name
    // would join as nobody: two answers that nobody typed, arrived at by
    // default, and the second is impossible to correct once a match has started.
    if form.server.trim().is_empty() {
        form.status = "Nowhere to go: type the address of the server to join.".to_string();
        form.trouble = true;
        return;
    }
    if form.name.trim().is_empty() {
        form.status = "Nobody to be: type the name to play under.".to_string();
        form.trouble = true;
        return;
    }

    let addr = match resolve(&form.server) {
        Ok(addr) => addr,
        Err(e) => {
            form.status = e;
            form.trouble = true;
            return;
        }
    };
    let name = form.name.trim().to_string();
    match NetClient::connect(addr, form.identity(), name) {
        Ok(client) => {
            commands.insert_resource(client);
            form.attempt = Some(Attempt { addr, started: now });
            form.status = format!("Connecting to {addr}...");
            form.trouble = false;
        }
        Err(e) => {
            form.status = format!("Could not open a socket: {e}");
            form.trouble = true;
        }
    }
}

/// Paints the form. Separate from [`update`] so that reading input and drawing
/// the result of it never fight over the same frame's state.
pub fn paint(
    form: Res<ConnectForm>,
    mut texts: Query<(&ConnectText, &mut Text, &mut TextColor)>,
    mut boxes: Query<(&FieldId, &mut BorderColor)>,
) {
    let editing = form.attempt.is_none();
    for (id, mut text, mut color) in &mut texts {
        match id {
            ConnectText::Value(field) => {
                let focused = editing && form.focus == *field;
                // A caret rather than a blinking one: it says which box the
                // keys are going into, which is all it is there for.
                **text = format!("{}{}", form.field(*field), if focused { "|" } else { "" });
                *color = TextColor(if editing { TEXT } else { TEXT_DIM });
            }
            ConnectText::Status => {
                **text = form.status.clone();
                *color = TextColor(if form.trouble { TROUBLE } else { TEXT_DIM });
            }
            ConnectText::Button => {
                **text = if editing { "Connect".into() } else { "Connecting...".into() };
                *color = TextColor(if editing { TEXT } else { TEXT_DIM });
            }
        }
    }
    for (field, mut border) in &mut boxes {
        let focused = editing && form.focus == *field;
        *border = BorderColor::all(if focused { EDGE_FOCUS } else { EDGE });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }
    fn v6(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv6Addr::LOCALHOST, port))
    }

    /// The resolver lists `::1` first for `localhost` on Windows. The server
    /// binds IPv4 by default, so following that order sends every packet into
    /// a void that reports nothing back.
    #[test]
    fn ipv4_wins_when_the_resolver_lists_ipv6_first() {
        assert_eq!(prefer_ipv4(&[v6(45701), v4(45701)]), Some(v4(45701)));
    }

    #[test]
    fn ipv6_is_used_when_it_is_all_there_is() {
        assert_eq!(prefer_ipv4(&[v6(45701)]), Some(v6(45701)));
    }

    #[test]
    fn nothing_resolved_is_not_a_panic() {
        assert_eq!(prefer_ipv4(&[]), None);
    }

    /// The resolver's own answer to nothing at all. The screen no longer lets
    /// a blank address reach it -- [`start`] turns one away -- but a caller
    /// that does ask gets this machine rather than an error.
    #[test]
    fn an_empty_address_resolves_to_this_machine() {
        assert_eq!(resolve("   "), Ok(v4(DEFAULT_PORT)));
    }

    #[test]
    fn an_address_and_a_name_go_straight_to_the_field() {
        assert!(skips_the_screen(true, "Ash"));
    }

    /// Nothing but an address gets you past the screen, which is why an
    /// identity is not even a question this asks: a build that always supplies
    /// one -- Android, keeping its token in private storage -- cannot talk its
    /// way to a destination it never named.
    #[test]
    fn a_name_alone_does_not_assume_an_address() {
        assert!(!skips_the_screen(false, "Ash"));
        assert!(!skips_the_screen(false, ""));
    }

    #[test]
    fn an_address_without_a_name_still_asks() {
        assert!(!skips_the_screen(true, ""));
        assert!(!skips_the_screen(true, "   "));
    }

    #[test]
    fn what_is_remembered_sits_beside_the_identity() {
        let token = PathBuf::from("/data/user/0/com.orewar.client/files/orewar-token");
        assert_eq!(
            remembered_beside(Some(&token)),
            Path::new("/data/user/0/com.orewar.client/files/.orewar-connect")
        );
    }

    /// The desktop default: no token file was named, so it stays where it has
    /// always been, in whatever directory the client was started from.
    #[test]
    fn with_no_identity_file_it_stays_in_the_working_directory() {
        assert_eq!(remembered_beside(None), Path::new(REMEMBERED));
    }

    /// A bare file name has an empty parent. Joining onto that would give
    /// `/.orewar-connect` -- the root of the disk -- rather than here.
    #[test]
    fn a_bare_identity_file_name_does_not_escape_to_the_root() {
        assert_eq!(remembered_beside(Some(Path::new("my.token"))), Path::new(REMEMBERED));
    }

    #[test]
    fn a_bare_address_gains_the_default_port() {
        assert_eq!(resolve("127.0.0.1"), Ok(v4(DEFAULT_PORT)));
        assert_eq!(resolve("127.0.0.1:45999"), Ok(v4(45999)));
    }

    /// An IPv6 literal is full of colons, so the test for "has a port already"
    /// cannot simply be one.
    #[test]
    fn an_ipv6_literal_keeps_its_shape() {
        assert_eq!(resolve("::1"), Ok(v6(DEFAULT_PORT)));
        assert_eq!(resolve("[::1]:45999"), Ok(v6(45999)));
    }

    #[test]
    fn a_name_that_resolves_to_nothing_is_reported_not_panicked() {
        assert!(resolve("no-such-host.invalid").is_err());
    }

    /// Enter and Tab carry text of their own. Typing it would leave a carriage
    /// return in the address the moment the player pressed the button.
    #[test]
    fn control_characters_are_not_typed() {
        let mut field = String::from("Ash");
        type_into(&mut field, "\r", NAME_LIMIT);
        type_into(&mut field, "\t", NAME_LIMIT);
        assert_eq!(field, "Ash");
    }

    #[test]
    fn a_field_stops_at_its_limit() {
        let mut field = String::new();
        type_into(&mut field, &"x".repeat(100), NAME_LIMIT);
        assert_eq!(field.chars().count(), NAME_LIMIT);
    }

    /// Counted in characters, not bytes, or a name in a non-Latin script would
    /// be cut to a third of the length everybody else gets.
    #[test]
    fn the_limit_counts_characters() {
        let mut field = String::new();
        type_into(&mut field, &"\u{4e2d}".repeat(30), NAME_LIMIT);
        assert_eq!(field.chars().count(), NAME_LIMIT);
    }

    #[test]
    fn tab_walks_between_the_two_fields() {
        assert_eq!(FieldId::Server.other(), FieldId::Name);
        assert_eq!(FieldId::Name.other(), FieldId::Server);
    }
}
