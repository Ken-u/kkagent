//! pi-tui aligned primitives (fuzzy, kill-ring, undo, paste, keys, …).

pub mod autocomplete;
pub mod fuzzy;
pub mod keybindings;
pub mod kill_ring;
pub mod paste_burst;
pub mod terminal_colors;
pub mod terminal_image;
pub mod undo_stack;
pub mod word_navigation;

pub use autocomplete::{
    complete_at_files, complete_path, complete_slash, extract_at_token, format_at_completion,
    Autocomplete, CompletionItem,
};
pub use fuzzy::{fuzzy_filter, fuzzy_match, FuzzyMatch};
pub use keybindings::{map_key, EditorAction};
pub use kill_ring::KillRing;
pub use paste_burst::PasteBurst;
pub use terminal_colors::Theme as PiTheme;
pub use undo_stack::{EditorSnapshot, UndoStack};
pub use word_navigation::{move_word_left, move_word_right};
