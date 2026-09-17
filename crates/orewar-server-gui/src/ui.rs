//! The window: a form to start a match, and a panel that watches one.
//!
//! Both screens are built once at startup and shown by visibility, so nothing
//! is spawned or despawned while a match is running.

use std::collections::VecDeque;

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use orewar_server::server::Status;
use orewar_shared::protocol::GameStatus;
use orewar_shared::rng::Rng;
use orewar_shared::world::{DEFAULT_PORT, MAX_PLAYERS, TICK_HZ};

use crate::hosting::Hosting;

pub const BACKDROP: Color = Color::srgb(0.02, 0.04, 0.06);
const PANEL_BG: Color = Color::srgba(0.05, 0.08, 0.11, 0.98);
const FIELD_BG: Color = Color::srgba(1.0, 1.0, 1.0, 0.05);
const EDGE: Color = Color::srgba(0.5, 0.6, 0.7, 0.35);
const EDGE_FOCUS: Color = Color::srgb(0.45, 0.68, 0.92);
const TEXT: Color = Color::srgb(0.90, 0.94, 0.97);
const TEXT_DIM: Color = Color::srgb(0.62, 0.69, 0.75);
const LIVE: Color = Color::srgb(0.45, 0.85, 0.55);
const TROUBLE: Color = Color::srgb(0.95, 0.55, 0.42);

/// How many log lines the window keeps. The terminal server prints everything;
/// a window has a bottom, and the recent lines are the ones worth the room.
const LOG_SHOWN: usize = 10;

/// Which of the two screens is up.
#[derive(States, Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Screen {
    #[default]
    Setup,
    Running,
}

/// One of the two things the form asks for.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Port,
    Seed,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Port => "Port",
            Field::Seed => "World seed",
        }
    }

    fn other(self) -> Field {
        match self {
            Field::Port => Field::Seed,
            Field::Seed => Field::Port,
        }
    }

    /// A port is five digits; a seed is a whole `u64`.
    fn limit(self) -> usize {
        match self {
            Field::Port => 5,
            Field::Seed => 20,
        }
    }
}

/// Text that is written to as the match runs, identified rather than
/// separately marked: one query and a `match` beats a marker per widget.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Value(Field),
    SetupStatus,
    JoinAt,
    Summary,
    Player(usize),
    Log,
}

/// A button, and what pressing it does.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    NewMatch,
    Stop,
}

/// The root of one screen, shown when that screen is the one we are on.
#[derive(Component)]
pub struct Root(Screen);

#[derive(Resource)]
pub struct SetupForm {
    port: String,
    seed: String,
    focus: Field,
    status: String,
    trouble: bool,
}

impl Default for SetupForm {
    fn default() -> Self {
        SetupForm {
            port: DEFAULT_PORT.to_string(),
            // Empty means a map nobody has seen before, which is what a host
            // wants nearly every time. A seed is for repeating one on purpose.
            seed: String::new(),
            focus: Field::Port,
            status: String::new(),
            trouble: false,
        }
    }
}

/// What the match thread last said, kept where the window can paint it.
#[derive(Resource, Default)]
pub struct Live {
    status: Option<Status>,
    log: VecDeque<String>,
}

/// Adds a typed digit to a field. Nothing here takes anything but digits, so
/// a mistyped port is impossible rather than merely reported.
fn type_into(field: &mut String, text: &str, limit: usize) {
    for ch in text.chars() {
        if !ch.is_ascii_digit() || field.len() >= limit {
            continue;
        }
        field.push(ch);
    }
}

fn font(size: f32) -> TextFont {
    TextFont { font_size: FontSize::Px(size), ..default() }
}

pub fn setup(mut commands: Commands) {
    // There is no world to look at, but UI is still drawn through a camera.
    commands.spawn(Camera2d);
    spawn_setup_screen(&mut commands);
    spawn_running_screen(&mut commands);
}

fn screen_node() -> Node {
    Node {
        position_type: PositionType::Absolute,
        width: percent(100),
        height: percent(100),
        justify_content: JustifyContent::Center,
        align_items: AlignItems::Center,
        ..default()
    }
}

fn panel_node(width: f32) -> Node {
    Node {
        width: px(width),
        flex_direction: FlexDirection::Column,
        padding: UiRect::all(px(22)),
        border: UiRect::all(px(1)),
        border_radius: BorderRadius::all(px(10)),
        ..default()
    }
}

fn spawn_setup_screen(commands: &mut Commands) {
    commands.spawn((Root(Screen::Setup), screen_node())).with_children(|screen| {
        screen
            .spawn((panel_node(400.0), BackgroundColor(PANEL_BG), BorderColor::all(EDGE)))
            .with_children(|panel| {
                panel.spawn((Text::new("OREWAR"), font(26.0), TextColor(TEXT)));
                panel.spawn((
                    Node { margin: UiRect { bottom: px(16), ..default() }, ..default() },
                    Text::new("host a match"),
                    font(12.0),
                    TextColor(TEXT_DIM),
                ));

                for field in [Field::Port, Field::Seed] {
                    spawn_field(panel, field);
                }

                panel.spawn((
                    Label::SetupStatus,
                    Node {
                        margin: UiRect { top: px(10), bottom: px(4), ..default() },
                        ..default()
                    },
                    Text::new(String::new()),
                    font(12.0),
                    TextColor(TEXT_DIM),
                ));

                spawn_button(panel, Action::Start, "Start hosting", 0.0);

                panel.spawn((
                    Node { margin: UiRect { top: px(14), ..default() }, ..default() },
                    Text::new(
                        "Tab or click to switch  |  Enter to start\n\
                         Leave the seed empty for a map nobody has seen.\n\
                         The next screen shows the address to give players.",
                    ),
                    font(11.0),
                    TextColor(TEXT_DIM),
                ));
            });
    });
}

fn spawn_running_screen(commands: &mut Commands) {
    commands.spawn((Root(Screen::Running), screen_node(), Visibility::Hidden)).with_children(
        |screen| {
            screen
                .spawn((panel_node(560.0), BackgroundColor(PANEL_BG), BorderColor::all(EDGE)))
                .with_children(|panel| {
                    panel.spawn((Text::new("HOSTING"), font(20.0), TextColor(TEXT)));
                    panel.spawn((
                        Node { margin: UiRect { top: px(12), ..default() }, ..default() },
                        Text::new("players join at"),
                        font(11.0),
                        TextColor(TEXT_DIM),
                    ));
                    // The one thing on this screen anybody needs to read out
                    // loud, so it is the one thing set in a large face.
                    panel.spawn((
                        Label::JoinAt,
                        Node { margin: UiRect { bottom: px(10), ..default() }, ..default() },
                        Text::new(String::new()),
                        font(24.0),
                        TextColor(LIVE),
                    ));
                    panel.spawn((
                        Label::Summary,
                        Node { margin: UiRect { bottom: px(14), ..default() }, ..default() },
                        Text::new(String::new()),
                        font(12.0),
                        TextColor(TEXT_DIM),
                    ));

                    panel.spawn((Text::new("PLAYERS"), font(11.0), TextColor(TEXT_DIM)));
                    for slot in 0..MAX_PLAYERS {
                        panel.spawn((
                            Label::Player(slot),
                            Node { margin: UiRect { top: px(3), ..default() }, ..default() },
                            Text::new(String::new()),
                            font(14.0),
                            TextColor(TEXT),
                        ));
                    }

                    panel.spawn((
                        Node { margin: UiRect { top: px(16), ..default() }, ..default() },
                        Text::new("LOG"),
                        font(11.0),
                        TextColor(TEXT_DIM),
                    ));
                    panel.spawn((
                        Label::Log,
                        Node {
                            margin: UiRect { top: px(3), ..default() },
                            // Ten lines of room whether or not there are ten
                            // lines, so the buttons under it never move.
                            min_height: px(11.0 * 1.4 * LOG_SHOWN as f32),
                            ..default()
                        },
                        Text::new(String::new()),
                        font(11.0),
                        TextColor(TEXT_DIM),
                    ));

                    panel
                        .spawn(Node {
                            margin: UiRect { top: px(14), ..default() },
                            column_gap: px(10),
                            ..default()
                        })
                        .with_children(|row| {
                            spawn_button(row, Action::NewMatch, "New match", 150.0);
                            spawn_button(row, Action::Stop, "Stop", 150.0);
                        });
                });
        },
    );
}

/// One labelled box the host types into.
fn spawn_field(panel: &mut ChildSpawnerCommands, field: Field) {
    panel
        .spawn((
            field,
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
            boxed.spawn((Text::new(field.label()), font(10.0), TextColor(TEXT_DIM)));
            boxed.spawn((Label::Value(field), Text::new(String::new()), font(16.0), TextColor(TEXT)));
        });
}

/// A button. `width` of zero fills the row it is in, which is what the form's
/// single button wants and what a row of two does not.
fn spawn_button(parent: &mut ChildSpawnerCommands, action: Action, label: &str, width: f32) {
    let mut node = Node {
        padding: UiRect::axes(px(12), px(9)),
        justify_content: JustifyContent::Center,
        border: UiRect::all(px(1)),
        border_radius: BorderRadius::all(px(6)),
        ..default()
    };
    if width > 0.0 {
        node.width = px(width);
    }
    parent.spawn((
        action,
        Button,
        node,
        BackgroundColor(FIELD_BG),
        BorderColor::all(EDGE),
        children![(Text::new(label.to_string()), font(16.0), TextColor(TEXT))],
    ));
}

/// Shows the screen we are on, and hides the other.
pub fn show(screen: Res<State<Screen>>, mut roots: Query<(&Root, &mut Visibility)>) {
    for (root, mut visibility) in &mut roots {
        *visibility =
            if root.0 == *screen.get() { Visibility::Inherited } else { Visibility::Hidden };
    }
}

/// The form: typing, and the button that starts a match.
pub fn setup_screen(
    mut commands: Commands,
    mut typed: MessageReader<KeyboardInput>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut form: ResMut<SetupForm>,
    mut next: ResMut<NextState<Screen>>,
    fields: Query<(&Field, &Interaction)>,
    buttons: Query<(&Action, &Interaction)>,
) {
    let mut start = false;

    if mouse.just_pressed(MouseButton::Left) {
        for (field, interaction) in &fields {
            if *interaction != Interaction::None {
                form.focus = *field;
            }
        }
        start = buttons
            .iter()
            .any(|(action, interaction)| *action == Action::Start && *interaction != Interaction::None);
    }

    for event in typed.read() {
        if event.state != ButtonState::Pressed {
            continue;
        }
        match &event.logical_key {
            Key::Enter => start = true,
            Key::Tab | Key::ArrowUp | Key::ArrowDown => form.focus = form.focus.other(),
            Key::Backspace => {
                let focus = form.focus;
                match focus {
                    Field::Port => form.port.pop(),
                    Field::Seed => form.seed.pop(),
                };
            }
            _ => {
                if let Some(text) = &event.text {
                    let focus = form.focus;
                    let limit = focus.limit();
                    match focus {
                        Field::Port => type_into(&mut form.port, text, limit),
                        Field::Seed => type_into(&mut form.seed, text, limit),
                    }
                }
            }
        }
    }

    if !start {
        return;
    }

    // An empty port is the usual one, and an empty seed is a map nobody has
    // seen. Neither is a mistake worth stopping for.
    let port = if form.port.trim().is_empty() {
        DEFAULT_PORT
    } else {
        match form.port.trim().parse::<u16>() {
            Ok(0) | Err(_) => {
                form.status = format!("{} is not a port number", form.port.trim());
                form.trouble = true;
                return;
            }
            Ok(port) => port,
        }
    };
    let seed = match form.seed.trim() {
        "" => Rng::seed_from_time(),
        text => match text.parse::<u64>() {
            Ok(seed) => seed,
            Err(_) => {
                form.status = format!("{text} is too large for a seed");
                form.trouble = true;
                return;
            }
        },
    };

    // Every interface: a host who has to name one has to know which of their
    // addresses the other machines can reach, and the next screen answers that
    // question rather than asking it.
    match Hosting::start("0.0.0.0", port, seed) {
        Ok(hosting) => {
            commands.insert_resource(hosting);
            form.status = String::new();
            form.trouble = false;
            next.set(Screen::Running);
        }
        Err(e) => {
            form.status = e;
            form.trouble = true;
        }
    }
}

/// Takes whatever the match thread has left since the last frame.
pub fn poll(hosting: Res<Hosting>, mut live: ResMut<Live>) {
    if let Some(status) = hosting.take_status() {
        live.status = Some(status);
    }
    for line in hosting.take_log() {
        if live.log.len() >= LOG_SHOWN {
            live.log.pop_front();
        }
        live.log.push_back(line);
    }
}

/// The two buttons a running match has.
pub fn running_screen(
    mut commands: Commands,
    mouse: Res<ButtonInput<MouseButton>>,
    hosting: Res<Hosting>,
    mut live: ResMut<Live>,
    mut form: ResMut<SetupForm>,
    mut next: ResMut<NextState<Screen>>,
    buttons: Query<(&Action, &Interaction)>,
) {
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }
    let pressed = buttons
        .iter()
        .find(|(_, interaction)| **interaction != Interaction::None)
        .map(|(action, _)| *action);

    match pressed {
        Some(Action::NewMatch) => hosting.ask_for_new_match(),
        Some(Action::Stop) => {
            hosting.ask_to_stop();
            commands.remove_resource::<Hosting>();
            *live = Live::default();
            form.status = "The match was stopped".to_string();
            form.trouble = false;
            next.set(Screen::Setup);
        }
        Some(Action::Start) | None => {}
    }
}

/// Paints both screens. Whichever is hidden costs a few strings a frame, which
/// is cheaper than the two systems and two queries the alternative needs.
pub fn paint(
    form: Res<SetupForm>,
    live: Res<Live>,
    hosting: Option<Res<Hosting>>,
    mut texts: Query<(&Label, &mut Text, &mut TextColor)>,
    mut boxes: Query<(&Field, &mut BorderColor)>,
) {
    for (label, mut text, mut color) in &mut texts {
        match label {
            Label::Value(field) => {
                let value = match field {
                    Field::Port => &form.port,
                    Field::Seed => &form.seed,
                };
                let empty_note = match field {
                    Field::Port if value.is_empty() => format!("{DEFAULT_PORT}"),
                    Field::Seed if value.is_empty() => "random".to_string(),
                    _ => String::new(),
                };
                if value.is_empty() {
                    **text = empty_note;
                    *color = TextColor(TEXT_DIM);
                } else {
                    **text = value.clone();
                    *color = TextColor(TEXT);
                }
                if form.focus == *field {
                    text.push('|');
                }
            }
            Label::SetupStatus => {
                **text = form.status.clone();
                *color = TextColor(if form.trouble { TROUBLE } else { TEXT_DIM });
            }
            Label::JoinAt => {
                **text = hosting.as_ref().map(|h| h.join_at.clone()).unwrap_or_default();
            }
            Label::Summary => {
                // Two lines rather than one long one: a seed is twenty digits,
                // and a line the panel wraps for itself lands anywhere.
                **text = match (&hosting, &live.status) {
                    // The seed comes from the snapshot, not from what was
                    // asked for at startup: a new match is a new map, and a
                    // panel still showing the old seed says the button did
                    // nothing.
                    (Some(_), Some(status)) => format!(
                        "{}  |  tick {}\nseed {}  |  {TICK_HZ} Hz  |  up to {MAX_PLAYERS} players",
                        describe(status.state),
                        status.tick,
                        status.seed,
                    ),
                    // The first frame after starting, before the thread has
                    // pumped once.
                    (Some(hosting), None) => format!("seed {}  |  starting", hosting.seed),
                    (None, _) => String::new(),
                };
            }
            Label::Player(slot) => {
                let player = live.status.as_ref().and_then(|s| s.players.get(*slot));
                match player {
                    Some(p) => {
                        **text = format!(
                            "{} #{}   ore {}   captures {}   {}",
                            p.name,
                            p.id,
                            p.ore_mined,
                            p.captures,
                            match (p.connected, p.rtt_ms) {
                                (true, Some(ms)) => format!("{ms:.0} ms"),
                                (true, None) => "connected".to_string(),
                                (false, _) => "offline".to_string(),
                            }
                        );
                        *color = TextColor(if p.connected { TEXT } else { TEXT_DIM });
                    }
                    None => {
                        **text = "--".to_string();
                        *color = TextColor(TEXT_DIM);
                    }
                }
            }
            Label::Log => **text = live.log.iter().cloned().collect::<Vec<_>>().join("\n"),
        }
    }

    for (field, mut border) in &mut boxes {
        *border = BorderColor::all(if form.focus == *field { EDGE_FOCUS } else { EDGE });
    }
}

fn describe(state: GameStatus) -> &'static str {
    match state {
        GameStatus::Waiting => "waiting for a second player",
        GameStatus::Running => "in play",
        GameStatus::Finished => "finished",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The form takes numbers only, so there is no such thing as a port with a
    /// letter in it to report back to the host.
    #[test]
    fn only_digits_are_typed() {
        let mut field = String::new();
        type_into(&mut field, "4a5.7", 5);
        assert_eq!(field, "457");
    }

    #[test]
    fn a_field_stops_at_its_limit() {
        let mut field = String::new();
        type_into(&mut field, "123456789", Field::Port.limit());
        assert_eq!(field, "12345");
    }

    /// A seed is a whole `u64`, and its limit has to be long enough to hold
    /// one or the largest maps could not be asked for by name.
    #[test]
    fn a_seed_field_holds_any_seed() {
        assert!(Field::Seed.limit() >= u64::MAX.to_string().len());
    }

    #[test]
    fn tab_walks_between_the_two_fields() {
        assert_eq!(Field::Port.other(), Field::Seed);
        assert_eq!(Field::Seed.other(), Field::Port);
    }
}
