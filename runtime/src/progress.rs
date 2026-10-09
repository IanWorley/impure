//! What a long preparation step (extracting the disc, building the game library) is doing,
//! shared between the thread doing it and the window or terminal showing it.

use std::sync::Mutex;

use bevy::log::info;

/// Percent steps at which terminal output reports a count.
const ECHO_PERCENT_STEP: u64 = 10;
const PERCENT: u64 = 100;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub stage: String,
    /// Units done and the total (bytes or files); a total of 0 means there is no count.
    pub done: u64,
    pub total: u64,
}

impl State {
    pub fn fraction(&self) -> Option<f32> {
        (self.total > 0).then(|| self.done as f32 / self.total as f32)
    }
}

pub struct Progress {
    state: Mutex<State>,
    /// Also print to the terminal, for runs without a window (`--build-game`).
    echo: bool,
}

impl Progress {
    pub fn new(echo: bool) -> Self {
        Self {
            state: Mutex::new(State::default()),
            echo,
        }
    }

    /// Starts a stage, with no count yet.
    pub fn stage(&self, stage: impl Into<String>) {
        let stage = stage.into();
        info!(target: "prepare", "{stage}");
        if self.echo {
            eprintln!("{stage}");
        }
        *self.state.lock().unwrap() = State {
            stage,
            ..State::default()
        };
    }

    pub fn count(&self, done: u64, total: u64) {
        let mut state = self.state.lock().unwrap();
        let step = |done: u64| done * PERCENT / total.max(1) / ECHO_PERCENT_STEP;
        if self.echo && step(done) != step(state.done) {
            eprintln!("  {}%", step(done) * ECHO_PERCENT_STEP);
        }
        state.done = done;
        state.total = total;
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap().clone()
    }
}
