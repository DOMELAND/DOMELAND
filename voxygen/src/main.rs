#![feature(drain_filter)]
#![recursion_limit = "2048"]

pub mod assets;
pub mod anim;
pub mod error;
pub mod hud;
pub mod key_state;
pub mod menu;
pub mod mesh;
pub mod render;
pub mod scene;
pub mod session;
pub mod settings;
pub mod singleplayer;
pub mod ui;
pub mod window;

// Reexports
pub use crate::error::Error;

use crate::{menu::main::MainMenuState, settings::Settings, window::Window};
use log;
use simplelog::{CombinedLogger, Config, TermLogger, WriteLogger};
use std::{fs::File, mem, panic, str::FromStr, thread};

/// The URL of the default public server that Voxygen will connect to
const DEFAULT_PUBLIC_SERVER: &'static str = "server.veloren.net";

/// A type used to store state that is shared between all play states
pub struct GlobalState {
    settings: Settings,
    window: Window,
}

impl GlobalState {
    /// Called after a change in play state has occured (usually used to reverse any temporary
    /// effects a state may have made).
    pub fn on_play_state_changed(&mut self) {
        self.window.grab_cursor(false);
        self.window.needs_refresh_resize();
    }
}

pub enum Direction {
    Forwards,
    Backwards,
}

// States can either close (and revert to a previous state), push a new state on top of themselves,
// or switch to a totally different state
pub enum PlayStateResult {
    /// Pop all play states in reverse order and shut down the program
    Shutdown,
    /// Close the current play state and pop it from the play state stack
    Pop,
    /// Push a new play state onto the play state stack
    Push(Box<dyn PlayState>),
    /// Switch the current play state with a new play state
    Switch(Box<dyn PlayState>),
}

/// A trait representing a playable game state. This may be a menu, a game session, the title
/// screen, etc.
pub trait PlayState {
    /// Play the state until some change of state is required (i.e: a menu is opened or the game
    /// is closed).
    fn play(&mut self, direction: Direction, global_state: &mut GlobalState) -> PlayStateResult;

    /// Get a descriptive name for this state type
    fn name(&self) -> &'static str;
}

fn main() {
    // Set up the global state
    let settings = match Settings::load() {
        Ok(settings) => settings,
        Err(err) => {
            let settings = Settings::default();
            settings.save_to_file();
            settings
        }
    };
    let window = Window::new(&settings).expect("Failed to create window");

    // Init logging
    let term_log_level = std::env::var_os("VOXYGEN_LOG")
        .and_then(|env| env.to_str().map(|s| s.to_owned()))
        .and_then(|s| log::LevelFilter::from_str(&s).ok())
        .unwrap_or(log::LevelFilter::Warn);
    CombinedLogger::init(vec![
        TermLogger::new(term_log_level, Config::default()).unwrap(),
        WriteLogger::new(
            log::LevelFilter::Info,
            Config::default(),
            File::create(&settings.log.file).unwrap(),
        ),
    ])
    .unwrap();

    // Set up panic handler to relay swish panic messages to the user
    let settings_clone = settings.clone();
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let msg = format!(" \
A critical error has occured and Voxygen has been forced to terminate in an unusual manner. Details about the error can be found below.

> What should I do?

We need your help to fix this! You can help by contacting us and reporting this problem. To do this, open an issue on the Veloren issue tracker:

https://www.gitlab.com/veloren/veloren/issues/new

If you're on the Veloren community Discord server, we'd be grateful if you could also post a message in the #support channel.

> What should I include?

The error information below will be useful in finding and fixing the problem. Please include as much information about your setup and the events that led up to the panic as possible.

Voxygen has logged information about the problem (including this message) to the file {:#?}. Please include the contents of this file in your bug report.

> Error information

The information below is intended for developers and testers.

{:?}", settings_clone.log.file, panic_info);

        log::error!("VOXYGEN HAS PANICKED\n\n{}", msg);

        msgbox::create("Voxygen has panicked", &msg, msgbox::IconType::ERROR);

        default_hook(panic_info);
    }));

    let mut global_state = GlobalState { settings, window };

    // Set up the initial play state
    let mut states: Vec<Box<dyn PlayState>> = vec![Box::new(MainMenuState::new(&mut global_state))];
    states
        .last()
        .map(|current_state| log::info!("Started game with state '{}'", current_state.name()));

    // What's going on here?
    // ---------------------
    // The state system used by Voxygen allows for the easy development of stack-based menus.
    // For example, you may want a "title" state that can push a "main menu" state on top of it,
    // which can in turn push a "settings" state or a "game session" state on top of it.
    // The code below manages the state transfer logic automatically so that we don't have to
    // re-engineer it for each menu we decide to add to the game.
    let mut direction = Direction::Forwards;
    while let Some(state_result) = states
        .last_mut()
        .map(|last| last.play(direction, &mut global_state))
    {
        // Implement state transfer logic
        match state_result {
            PlayStateResult::Shutdown => {
                direction = Direction::Backwards;
                log::info!("Shutting down all states...");
                while states.last().is_some() {
                    states.pop().map(|old_state| {
                        log::info!("Popped state '{}'", old_state.name());
                        global_state.on_play_state_changed();
                    });
                }
            }
            PlayStateResult::Pop => {
                direction = Direction::Backwards;
                states.pop().map(|old_state| {
                    log::info!("Popped state '{}'", old_state.name());
                    global_state.on_play_state_changed();
                });
            }
            PlayStateResult::Push(new_state) => {
                direction = Direction::Forwards;
                log::info!("Pushed state '{}'", new_state.name());
                states.push(new_state);
                global_state.on_play_state_changed();
            }
            PlayStateResult::Switch(mut new_state) => {
                direction = Direction::Forwards;
                states.last_mut().map(|old_state| {
                    log::info!(
                        "Switching to state '{}' from state '{}'",
                        new_state.name(),
                        old_state.name()
                    );
                    mem::swap(old_state, &mut new_state);
                    global_state.on_play_state_changed();
                });
            }
        }
    }
}
