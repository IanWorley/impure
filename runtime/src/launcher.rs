//! The window before the game runs: asks for the player's Pure disc image (or its
//! `default.xex`) in the system's Open dialog, shows the game being prepared (src/prepare.rs),
//! then loads the game library and starts the guest. Also shows why, if the game stops.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};

use crate::prepare::{self, Prepared};
use crate::progress::Progress;
use crate::{game_module, guest, status};

const TITLE: &str = "Pure";
const OPEN_LABEL: &str = "Open Pure disc image or default.xex…";
const DIALOG_TITLE: &str = "Open your copy of Pure";
const FILTER_NAME: &str = "Pure disc image or executable";
const FILTER_EXTENSIONS: [&str; 2] = ["iso", "xex"];
const CHOOSE_PROMPT: &str = "Choose your own copy of Pure (World release): its disc image, or the \
    default.xex of a disc you copied. The first time, the game is translated and compiled on this \
    computer, which takes about 10 to 15 minutes; after that it starts at once.";
const PREPARE_THREAD_NAME: &str = "prepare-game";

const TITLE_SIZE: f32 = 48.0;
const TEXT_SIZE: f32 = 18.0;
const TEXT_WIDTH: Val = Val::Px(720.0);
const ROW_GAP: Val = Val::Px(24.0);
const BUTTON_PADDING: UiRect = UiRect::axes(Val::Px(24.0), Val::Px(12.0));
const BAR_WIDTH: Val = Val::Px(480.0);
const BAR_HEIGHT: Val = Val::Px(12.0);
const PERCENT: f32 = 100.0;
const BACKGROUND: Color = Color::srgb(0.05, 0.05, 0.08);
const BUTTON_COLOR: Color = Color::srgb(0.55, 0.08, 0.08);
const BUTTON_HOVER_COLOR: Color = Color::srgb(0.7, 0.12, 0.12);
const BAR_TRACK_COLOR: Color = Color::srgb(0.2, 0.2, 0.24);
const BAR_FILL_COLOR: Color = Color::srgb(0.85, 0.15, 0.15);
const ERROR_COLOR: Color = Color::srgb(1.0, 0.55, 0.5);
const TEXT_COLOR: Color = Color::WHITE;

/// Where the player is between opening the window and playing.
#[derive(Resource)]
enum Launch {
    Choosing,
    /// The Open dialog is up.
    Picking(Task<Option<PathBuf>>),
    Preparing {
        progress: Arc<Progress>,
        result: Arc<Mutex<Option<Result<Prepared, String>>>>,
    },
    Failed(String),
    Running,
    /// The guest stopped; the reason is shown over the last frame.
    Stopped,
}

/// The game to open at start-up, if any: from the command line, or the one opened last time.
#[derive(Resource)]
pub struct StartWith(pub Option<PathBuf>);

#[derive(Component)]
struct Screen;
#[derive(Component)]
struct Message;
#[derive(Component)]
struct BarFill;
/// When an element of the screen is visible.
#[derive(Component)]
struct ShownWhen(fn(&Launch) -> bool);

/// Openable: waiting for a choice, or after a failure.
fn can_open(launch: &Launch) -> bool {
    matches!(launch, Launch::Choosing | Launch::Failed(_))
}

fn has_count(launch: &Launch) -> bool {
    progress_fraction(launch).is_some()
}

fn not_running(launch: &Launch) -> bool {
    !matches!(launch, Launch::Running)
}

fn progress_fraction(launch: &Launch) -> Option<f32> {
    match launch {
        Launch::Preparing { progress, .. } => progress.snapshot().fraction(),
        _ => None,
    }
}

pub struct LauncherPlugin;

impl Plugin for LauncherPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Launch::Choosing)
            .add_systems(Startup, (spawn_screen, begin).chain())
            .add_systems(
                Update,
                (
                    open_button,
                    advance,
                    (show_elements, show_message, show_count),
                )
                    .chain(),
            );
    }
}

fn spawn_screen(mut commands: Commands) {
    let text = |size: f32| TextFont::from_font_size(size);
    commands
        .spawn((
            Screen,
            ShownWhen(not_running),
            Node {
                width: Val::Percent(PERCENT),
                height: Val::Percent(PERCENT),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: ROW_GAP,
                ..default()
            },
            BackgroundColor(BACKGROUND),
        ))
        .with_children(|screen| {
            screen.spawn((Text::new(TITLE), text(TITLE_SIZE), TextColor(TEXT_COLOR)));
            screen.spawn((
                Message,
                Text::new(CHOOSE_PROMPT),
                text(TEXT_SIZE),
                TextColor(TEXT_COLOR),
                TextLayout::justify(Justify::Center),
                Node {
                    max_width: TEXT_WIDTH,
                    ..default()
                },
            ));
            screen
                .spawn((
                    ShownWhen(has_count),
                    Node {
                        width: BAR_WIDTH,
                        height: BAR_HEIGHT,
                        ..default()
                    },
                    BackgroundColor(BAR_TRACK_COLOR),
                    Visibility::Hidden,
                ))
                .with_child((
                    BarFill,
                    Node {
                        width: Val::Percent(0.0),
                        height: Val::Percent(PERCENT),
                        ..default()
                    },
                    BackgroundColor(BAR_FILL_COLOR),
                ));
            screen
                .spawn((
                    ShownWhen(can_open),
                    Button,
                    Node {
                        padding: BUTTON_PADDING,
                        ..default()
                    },
                    BackgroundColor(BUTTON_COLOR),
                ))
                .with_child((
                    Text::new(OPEN_LABEL),
                    text(TEXT_SIZE),
                    TextColor(TEXT_COLOR),
                ));
        });
}

/// Opens the game named at start-up, or waits for the player to choose one.
fn begin(start_with: Res<StartWith>, mut launch: ResMut<Launch>) {
    if let Some(path) = &start_with.0 {
        *launch = start_preparing(path.clone());
    }
}

fn start_preparing(source: PathBuf) -> Launch {
    let progress = Arc::new(Progress::new(false));
    let result = Arc::new(Mutex::new(None));
    let (thread_progress, thread_result) = (progress.clone(), result.clone());
    let spawned = thread::Builder::new()
        .name(PREPARE_THREAD_NAME.into())
        .spawn(move || {
            let prepared = prepare::prepare(&source, &thread_progress);
            *thread_result.lock().unwrap() = Some(prepared);
        });
    match spawned {
        Ok(_) => Launch::Preparing { progress, result },
        Err(error) => Launch::Failed(format!("cannot start preparing the game: {error}")),
    }
}

/// The Open button (the launcher's only button) shows the system's Open dialog.
fn open_button(
    mut buttons: Query<(&Interaction, &mut BackgroundColor), Changed<Interaction>>,
    mut launch: ResMut<Launch>,
) {
    for (interaction, mut color) in &mut buttons {
        *color = BackgroundColor(match interaction {
            Interaction::Hovered => BUTTON_HOVER_COLOR,
            _ => BUTTON_COLOR,
        });
        if *interaction == Interaction::Pressed && can_open(&launch) {
            let dialog = rfd::AsyncFileDialog::new()
                .set_title(DIALOG_TITLE)
                .add_filter(FILTER_NAME, &FILTER_EXTENSIONS);
            let task = AsyncComputeTaskPool::get().spawn(async move {
                dialog
                    .pick_file()
                    .await
                    .map(|file| file.path().to_path_buf())
            });
            *launch = Launch::Picking(task);
        }
    }
}

/// Moves on when the dialog closes, the preparation finishes or the guest stops.
fn advance(mut launch: ResMut<Launch>) {
    let next = match &mut *launch {
        Launch::Picking(task) => match block_on(poll_once(task)) {
            Some(Some(path)) => Some(start_preparing(path)),
            Some(None) => Some(Launch::Choosing),
            None => None,
        },
        Launch::Preparing { result, .. } => result.lock().unwrap().take().map(|prepared| {
            match prepared.and_then(|prepared| start(&prepared)) {
                Ok(()) => Launch::Running,
                Err(reason) => {
                    // Scripted runs (`--exit-on-stop`) end here too.
                    status::report_stop(reason.clone());
                    Launch::Failed(reason)
                }
            }
        }),
        Launch::Running => status::stop_reason().map(|_| Launch::Stopped),
        Launch::Choosing | Launch::Failed(_) | Launch::Stopped => None,
    };
    if let Some(next) = next {
        *launch = next;
    }
}

fn start(prepared: &Prepared) -> Result<(), String> {
    game_module::load(&prepared.library)?;
    guest::start(&prepared.xex)
}

fn show_elements(launch: Res<Launch>, mut elements: Query<(&ShownWhen, &mut Visibility)>) {
    for (shown_when, mut visibility) in &mut elements {
        *visibility = if (shown_when.0)(&launch) {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

fn show_message(
    launch: Res<Launch>,
    message: Single<(&mut Text, &mut TextColor), With<Message>>,
    screen: Single<&mut BackgroundColor, With<Screen>>,
) {
    let (mut text, mut color) = message.into_inner();
    text.0 = match &*launch {
        Launch::Preparing { progress, .. } => progress.snapshot().stage,
        Launch::Failed(reason) => reason.clone(),
        Launch::Stopped => format!(
            "The game stopped: {}",
            status::stop_reason().unwrap_or_default()
        ),
        Launch::Choosing | Launch::Picking(_) | Launch::Running => CHOOSE_PROMPT.to_string(),
    };
    let problem = matches!(*launch, Launch::Failed(_) | Launch::Stopped);
    color.0 = if problem { ERROR_COLOR } else { TEXT_COLOR };
    // The stop reason is shown over the last frame rather than on the opaque background.
    screen.into_inner().0 = if matches!(*launch, Launch::Stopped) {
        Color::NONE
    } else {
        BACKGROUND
    };
}

fn show_count(launch: Res<Launch>, fill: Single<&mut Node, With<BarFill>>) {
    fill.into_inner().width = Val::Percent(progress_fraction(&launch).unwrap_or(0.0) * PERCENT);
}
