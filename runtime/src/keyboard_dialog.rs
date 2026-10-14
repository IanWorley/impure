//! The on-screen keyboard's dialog. When the title asks for text (XamShowKeyboardUI, see
//! src/kernel/keyboard.rs), the window shows its prompt over the game and takes the text from the
//! host keyboard, or from an input script's keys: Enter accepts it and Escape cancels.
//!
//! While the dialog is up the title's controller is at rest, as on the console. Keys pressed into
//! it stay hidden from the controller until they are released, so the Enter that accepts the name
//! does not also press A in the game and open the keyboard again.

use std::collections::HashSet;

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;

use crate::{ScriptedInput, kernel};

const PANEL_WIDTH: Val = Val::Px(640.0);
const PANEL_PADDING: UiRect = UiRect::all(Val::Px(24.0));
const ROW_GAP: Val = Val::Px(12.0);
const ENTRY_PADDING: UiRect = UiRect::axes(Val::Px(12.0), Val::Px(8.0));
const TITLE_SIZE: f32 = 28.0;
const TEXT_SIZE: f32 = 20.0;
const HINT_SIZE: f32 = 16.0;
const PERCENT: f32 = 100.0;
const BACKDROP_COLOR: Color = Color::srgba(0.0, 0.0, 0.0, 0.6);
const PANEL_COLOR: Color = Color::srgb(0.08, 0.08, 0.11);
const ENTRY_COLOR: Color = Color::srgb(0.18, 0.18, 0.22);
const TEXT_COLOR: Color = Color::WHITE;
const HINT_COLOR: Color = Color::srgb(0.7, 0.7, 0.75);
/// Drawn over the game's frame and the launcher.
const DIALOG_LAYER: GlobalZIndex = GlobalZIndex(1);
/// Shown after the text, where the next character goes.
const CURSOR: char = '_';
const HINT: &str = "Type, then Enter to accept or Escape to cancel.";
/// Input scripts name letter keys as Bevy does (`KeyW`); they type the lowercase letter.
const LETTER_KEY_PREFIX: &str = "Key";

pub struct KeyboardDialogPlugin;

impl Plugin for KeyboardDialogPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<KeyboardDialog>()
            .add_systems(Startup, spawn_dialog)
            .add_systems(Update, show_dialog.after(type_into_dialog));
    }
}

/// The dialog, while it is up, and the keys it keeps from the controller.
#[derive(Resource, Default)]
pub struct KeyboardDialog {
    entry: Option<Entry>,
    /// Keys pressed while the dialog was up, hidden from the controller until released.
    swallowed: HashSet<KeyCode>,
}

impl KeyboardDialog {
    pub fn is_open(&self) -> bool {
        self.entry.is_some()
    }

    /// The held keys the controller should see.
    pub fn unswallowed(&self, keys: &ButtonInput<KeyCode>) -> ButtonInput<KeyCode> {
        let mut visible = ButtonInput::default();
        for &key in keys.get_pressed() {
            if !self.swallowed.contains(&key) {
                visible.press(key);
            }
        }
        visible
    }
}

/// The text being typed for a prompt.
struct Entry {
    prompt: kernel::KeyboardPrompt,
    text: String,
}

/// One thing the player does in the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    Type(char),
    Erase,
    Accept,
    Cancel,
}

/// How the player closed the dialog.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Accepted,
    Cancelled,
}

impl Entry {
    fn new(prompt: kernel::KeyboardPrompt) -> Self {
        let text = prompt.default_text.clone();
        Self { prompt, text }
    }

    /// Applies an edit; typing past the title's buffer is ignored.
    fn apply(&mut self, edit: Edit) -> Option<Outcome> {
        match edit {
            Edit::Type(character) => {
                let length = self.text.encode_utf16().count() + character.len_utf16();
                if length <= self.prompt.maximum_length {
                    self.text.push(character);
                }
                None
            }
            Edit::Erase => {
                self.text.pop();
                None
            }
            Edit::Accept => Some(Outcome::Accepted),
            Edit::Cancel => Some(Outcome::Cancelled),
        }
    }
}

/// What a host key press does in the dialog: its text as the keyboard layout produces it. A held
/// key repeats its text and erasing, but only a fresh press accepts or cancels, so a held A that
/// opened the dialog does not also close it.
fn host_edits(input: &KeyboardInput) -> Vec<Edit> {
    if input.state != ButtonState::Pressed {
        return Vec::new();
    }
    match &input.logical_key {
        Key::Enter | Key::Escape if input.repeat => Vec::new(),
        Key::Enter => vec![Edit::Accept],
        Key::Escape => vec![Edit::Cancel],
        Key::Backspace => vec![Edit::Erase],
        _ => input
            .text
            .iter()
            .flat_map(|text| text.chars())
            .filter(|character| !character.is_control())
            .map(Edit::Type)
            .collect(),
    }
}

/// What an input script's key press does in the dialog.
fn scripted_edit(key: KeyCode) -> Option<Edit> {
    match key {
        KeyCode::Enter => Some(Edit::Accept),
        KeyCode::Escape => Some(Edit::Cancel),
        KeyCode::Backspace => Some(Edit::Erase),
        KeyCode::Space => Some(Edit::Type(' ')),
        _ => {
            let name = format!("{key:?}");
            let mut letter = name.strip_prefix(LETTER_KEY_PREFIX)?.chars();
            match (letter.next(), letter.next()) {
                (Some(letter), None) => Some(Edit::Type(letter.to_ascii_lowercase())),
                _ => None,
            }
        }
    }
}

/// Opens the dialog when the title asks for text, applies the player's typing, and finishes
/// the title's request when the player accepts or cancels. Runs before the controller is read.
pub fn type_into_dialog(
    mut dialog: ResMut<KeyboardDialog>,
    mut host_input: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    scripted: Option<Res<ScriptedInput>>,
    mut scripted_before: Local<HashSet<KeyCode>>,
) {
    let scripted_held: HashSet<KeyCode> = scripted
        .map(|scripted| scripted.held.get_pressed().copied().collect())
        .unwrap_or_default();
    let scripted_pressed: Vec<KeyCode> = scripted_held
        .difference(&scripted_before)
        .copied()
        .collect();
    *scripted_before = scripted_held.clone();
    let host_pressed: Vec<KeyboardInput> = host_input.read().cloned().collect();

    let dialog = &mut *dialog;
    dialog
        .swallowed
        .retain(|key| keys.pressed(*key) || scripted_held.contains(key));
    let entry = match &mut dialog.entry {
        Some(entry) => entry,
        None => {
            let Some(prompt) = kernel::keyboard_prompt() else {
                return;
            };
            dialog.entry.insert(Entry::new(prompt))
        }
    };
    // Every key held while the dialog is up is the dialog's, including the press that opened it.
    dialog.swallowed.extend(keys.get_pressed().copied());
    dialog.swallowed.extend(scripted_held);

    let edits = host_pressed
        .iter()
        .flat_map(host_edits)
        .chain(scripted_pressed.into_iter().filter_map(scripted_edit));
    for edit in edits {
        let Some(outcome) = entry.apply(edit) else {
            continue;
        };
        kernel::finish_keyboard(match outcome {
            Outcome::Accepted => Some(&entry.text),
            Outcome::Cancelled => None,
        });
        dialog.entry = None;
        return;
    }
}

#[derive(Component)]
struct Dialog;
/// The text in the dialog that changes with each prompt.
#[derive(Component)]
enum DialogField {
    Title,
    Description,
    Entry,
}

fn spawn_dialog(mut commands: Commands) {
    let text = |size: f32| TextFont::from_font_size(size);
    commands
        .spawn((
            Dialog,
            DIALOG_LAYER,
            Node {
                width: Val::Percent(PERCENT),
                height: Val::Percent(PERCENT),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                display: Display::None,
                ..default()
            },
            BackgroundColor(BACKDROP_COLOR),
        ))
        .with_children(|backdrop| {
            backdrop
                .spawn((
                    Node {
                        width: PANEL_WIDTH,
                        padding: PANEL_PADDING,
                        row_gap: ROW_GAP,
                        flex_direction: FlexDirection::Column,
                        ..default()
                    },
                    BackgroundColor(PANEL_COLOR),
                ))
                .with_children(|panel| {
                    panel.spawn((
                        DialogField::Title,
                        Text::default(),
                        text(TITLE_SIZE),
                        TextColor(TEXT_COLOR),
                    ));
                    panel.spawn((
                        DialogField::Description,
                        Text::default(),
                        text(TEXT_SIZE),
                        TextColor(TEXT_COLOR),
                    ));
                    panel
                        .spawn((
                            Node {
                                padding: ENTRY_PADDING,
                                ..default()
                            },
                            BackgroundColor(ENTRY_COLOR),
                        ))
                        .with_child((
                            DialogField::Entry,
                            Text::default(),
                            text(TEXT_SIZE),
                            TextColor(TEXT_COLOR),
                        ));
                    panel.spawn((Text::new(HINT), text(HINT_SIZE), TextColor(HINT_COLOR)));
                });
        });
}

fn show_dialog(
    dialog: Res<KeyboardDialog>,
    mut node: Single<&mut Node, With<Dialog>>,
    mut fields: Query<(&DialogField, &mut Text)>,
) {
    if !dialog.is_changed() {
        return;
    }
    let Some(entry) = &dialog.entry else {
        node.display = Display::None;
        return;
    };
    node.display = Display::Flex;
    for (field, mut text) in &mut fields {
        text.0 = match field {
            DialogField::Title => entry.prompt.title.clone(),
            DialogField::Description => entry.prompt.description.clone(),
            DialogField::Entry => format!("{}{CURSOR}", entry.text),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(default_text: &str, maximum_length: usize) -> Entry {
        Entry::new(kernel::KeyboardPrompt {
            title: String::new(),
            description: String::new(),
            default_text: default_text.to_string(),
            maximum_length,
        })
    }

    #[test]
    fn typing_stops_at_the_titles_buffer() {
        let mut entry = entry("ab", 3);
        for character in ['c', 'd'] {
            assert_eq!(entry.apply(Edit::Type(character)), None);
        }
        assert_eq!(entry.text, "abc");
    }

    #[test]
    fn a_held_enter_accepts_only_when_first_pressed() {
        let press = |repeat| KeyboardInput {
            key_code: KeyCode::Enter,
            logical_key: Key::Enter,
            state: ButtonState::Pressed,
            text: None,
            repeat,
            window: Entity::PLACEHOLDER,
        };
        assert_eq!(host_edits(&press(false)), vec![Edit::Accept]);
        assert!(host_edits(&press(true)).is_empty());
    }

    #[test]
    fn script_keys_type_lowercase_letters_and_close_the_dialog() {
        assert_eq!(scripted_edit(KeyCode::KeyW), Some(Edit::Type('w')));
        assert_eq!(scripted_edit(KeyCode::Enter), Some(Edit::Accept));
        assert_eq!(scripted_edit(KeyCode::Escape), Some(Edit::Cancel));
        assert_eq!(scripted_edit(KeyCode::ArrowUp), None);
    }
}
