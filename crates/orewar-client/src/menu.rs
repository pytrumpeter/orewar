//! The in-game menu, opened with `Escape`.
//!
//! Built once at startup and toggled by visibility, like the rest of the UI in
//! [`crate::hud`]. Driven by both mouse and keyboard: a menu is the one place a
//! player expects to be able to click, and the one place they expect the arrow
//! keys not to drive a tank.

use bevy::prelude::*;

use crate::hud::{TEXT, TEXT_DIM};
use crate::input::LocalInput;
use crate::net::{self, NetClient};

/// Painted over the field, dark enough to say "the game is behind this".
const SCRIM: Color = Color::srgba(0.0, 0.02, 0.04, 0.55);
const PANEL_BG: Color = Color::srgba(0.04, 0.06, 0.09, 0.96);
const ROW_IDLE: Color = Color::srgba(1.0, 1.0, 1.0, 0.0);
const ROW_ACTIVE: Color = Color::srgba(0.35, 0.55, 0.75, 0.35);

/// One row of the menu.
///
/// The order here is the order they are spawned and the order the arrow keys
/// walk, so there is no separate list to keep in step.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Resume,
    NewGame,
    Settings,
    Quit,
}

impl MenuItem {
    const ALL: [MenuItem; 4] =
        [MenuItem::Resume, MenuItem::NewGame, MenuItem::Settings, MenuItem::Quit];

    fn label(self) -> &'static str {
        match self {
            MenuItem::Resume => "Resume",
            MenuItem::NewGame => "New Game",
            MenuItem::Settings => "Settings",
            MenuItem::Quit => "Quit",
        }
    }

    fn note(self) -> &'static str {
        match self {
            MenuItem::Resume => "back to the field",
            MenuItem::NewGame => "restart the match for everyone, on a fresh map",
            MenuItem::Settings => "not yet",
            MenuItem::Quit => "leave the match and close the game",
        }
    }

    /// Whether the item does anything. A disabled item is skipped by the arrow
    /// keys and carries no [`Button`], so it never lights up under the cursor.
    fn enabled(self) -> bool {
        !matches!(self, MenuItem::Settings)
    }
}

/// Marks the full-screen overlay, shown and hidden as a whole.
#[derive(Component)]
pub struct MenuRoot;

#[derive(Resource, Default)]
pub struct MenuState {
    pub open: bool,
    /// Index into [`MenuItem::ALL`]; always an enabled item.
    selected: usize,
}

pub fn setup(mut commands: Commands) {
    commands
        .spawn((
            MenuRoot,
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(SCRIM),
            // Startup systems are not ordered against each other, so the HUD's
            // spawn order cannot be relied on to put this on top. An explicit
            // global index can be.
            GlobalZIndex(10),
            Visibility::Hidden,
        ))
        .with_children(|overlay| {
            overlay
                .spawn((
                    Node {
                        width: px(330),
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::all(px(18)),
                        row_gap: px(4),
                        border: UiRect::all(px(1)),
                        border_radius: BorderRadius::all(px(8)),
                        ..default()
                    },
                    BackgroundColor(PANEL_BG),
                    BorderColor::all(Color::srgba(0.5, 0.6, 0.7, 0.55)),
                ))
                .with_children(|panel| {
                    panel.spawn((
                        Node { margin: UiRect { bottom: px(10), ..default() }, ..default() },
                        Text::new("OREWAR"),
                        TextFont { font_size: FontSize::Px(22.0), ..default() },
                        TextColor(TEXT),
                    ));

                    for item in MenuItem::ALL {
                        let mut row = panel.spawn((
                            item,
                            Node {
                                flex_direction: FlexDirection::Column,
                                padding: UiRect::axes(px(10), px(7)),
                                row_gap: px(2),
                                border_radius: BorderRadius::all(px(5)),
                                ..default()
                            },
                            BackgroundColor(ROW_IDLE),
                            children![
                                (
                                    Text::new(item.label()),
                                    TextFont { font_size: FontSize::Px(16.0), ..default() },
                                    TextColor(if item.enabled() { TEXT } else { TEXT_DIM }),
                                ),
                                (
                                    Text::new(item.note()),
                                    TextFont { font_size: FontSize::Px(11.0), ..default() },
                                    TextColor(TEXT_DIM),
                                ),
                            ],
                        ));
                        // Only enabled rows get an `Interaction` to read, which
                        // is what keeps `Settings` inert under the cursor.
                        if item.enabled() {
                            row.insert(Button);
                        }
                    }

                    panel.spawn((
                        Node { margin: UiRect { top: px(12), ..default() }, ..default() },
                        Text::new("arrows or mouse  |  Enter to choose  |  Esc to close"),
                        TextFont { font_size: FontSize::Px(11.0), ..default() },
                        TextColor(TEXT_DIM),
                    ));
                });
        });
}

/// `Escape` backs out of whatever is in front of the player.
///
/// Runs before [`crate::input::gather`], which is what stops the same press
/// from also being read as a game control on the frame the menu closes.
pub fn toggle(
    keys: Res<ButtonInput<KeyCode>>,
    mut menu: ResMut<MenuState>,
    mut input: ResMut<LocalInput>,
) {
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    if input.build_menu {
        input.build_menu = false;
    } else {
        menu.open = !menu.open;
    }
}

/// Moves the selection, activates an item, and paints the result.
pub fn update(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut menu: ResMut<MenuState>,
    mut net: ResMut<NetClient>,
    mut rows: Query<(&MenuItem, Option<&Interaction>, &mut BackgroundColor)>,
    mut root: Query<&mut Visibility, With<MenuRoot>>,
) {
    for mut visibility in &mut root {
        *visibility = if menu.open { Visibility::Visible } else { Visibility::Hidden };
    }
    if !menu.open {
        return;
    }

    if keys.just_pressed(KeyCode::ArrowDown) {
        menu.selected = step(menu.selected, 1);
    }
    if keys.just_pressed(KeyCode::ArrowUp) {
        menu.selected = step(menu.selected, -1);
    }

    // The cursor takes the selection with it, so the keyboard and the mouse
    // never disagree about what Enter would do.
    let mut clicked = None;
    for (item, interaction, _) in &rows {
        if interaction.is_some_and(|i| *i != Interaction::None) {
            if let Some(index) = MenuItem::ALL.iter().position(|c| c == item) {
                menu.selected = index;
            }
            // Activated on the press rather than the release: releasing over a
            // menu that has already closed would land the click on the game.
            if buttons.just_pressed(MouseButton::Left) {
                clicked = Some(*item);
            }
        }
    }

    let chosen = clicked.or_else(|| {
        (keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter))
            .then(|| MenuItem::ALL[menu.selected])
    });

    for (item, _, mut background) in &mut rows {
        let active = item.enabled() && *item == MenuItem::ALL[menu.selected];
        *background = BackgroundColor(if active { ROW_ACTIVE } else { ROW_IDLE });
    }

    match chosen {
        Some(MenuItem::Resume) => menu.open = false,
        Some(MenuItem::NewGame) => {
            net.request_new_game();
            menu.open = false;
        }
        Some(MenuItem::Quit) => net::leave_and_exit(&mut net),
        // Disabled, and unreachable while it carries no `Button`; spelled out
        // rather than left to a catch-all so enabling it later is a compiler
        // error here.
        Some(MenuItem::Settings) | None => {}
    }
}

/// Walks the selection by `delta`, wrapping, and skipping disabled items.
fn step(from: usize, delta: isize) -> usize {
    let len = MenuItem::ALL.len();
    let mut at = from;
    for _ in 0..len {
        at = (at as isize + delta).rem_euclid(len as isize) as usize;
        if MenuItem::ALL[at].enabled() {
            return at;
        }
    }
    from
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The selection must never land on an item that does nothing, in either
    /// direction, from anywhere -- including from the disabled item itself,
    /// which is where it would start if the default were ever changed.
    #[test]
    fn stepping_never_stops_on_a_disabled_item() {
        for start in 0..MenuItem::ALL.len() {
            for delta in [1, -1] {
                let mut at = start;
                for _ in 0..MenuItem::ALL.len() * 2 {
                    at = step(at, delta);
                    assert!(MenuItem::ALL[at].enabled(), "landed on {}", MenuItem::ALL[at].label());
                }
            }
        }
    }

    /// Walking one way and back must return where it started, or holding an
    /// arrow key would drift the selection.
    #[test]
    fn stepping_is_reversible() {
        assert_eq!(step(step(0, 1), -1), 0);
        assert_eq!(step(step(3, -1), 1), 3);
    }

    #[test]
    fn only_settings_is_disabled() {
        let disabled: Vec<_> =
            MenuItem::ALL.iter().filter(|i| !i.enabled()).map(|i| i.label()).collect();
        assert_eq!(disabled, ["Settings"]);
    }
}
