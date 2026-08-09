//! pi-tui aligned primitives (fuzzy, kill-ring, undo, paste, keys, …).

pub mod fuzzy;
pub mod kill_ring;
pub mod undo_stack;
pub mod paste_burst;
pub mod word_navigation;
pub mod keybindings;
pub mod terminal_colors;
pub mod autocomplete;

pub use fuzzy::{fuzzy_filter, fuzzy_match, FuzzyMatch};
pub use kill_ring::KillRing;
pub use undo_stack::{EditorSnapshot, UndoStack};
pub use paste_burst::PasteBurst;
pub use word_navigation::{move_word_left, move_word_right};
pub use keybindings::{map_key, EditorAction};
pub use terminal_colors::Theme as PiTheme;
pub use autocomplete::{complete_path, complete_slash, Autocomplete, CompletionItem};
