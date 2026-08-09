//! Session-level undo coordination across participants.

use crate::context_memory::{default_undo_participants, UndoParticipant};
use crate::session::Session;

#[derive(Debug, Clone)]
pub struct UndoResult {
    pub undone_turns: usize,
    pub message_count: usize,
    pub participants: Vec<UndoParticipant>,
}

#[derive(Debug, Default)]
pub struct UndoService {
    participants: Vec<UndoParticipant>,
}

impl UndoService {
    pub fn new() -> Self {
        Self {
            participants: default_undo_participants(),
        }
    }

    pub fn with_participants(participants: Vec<UndoParticipant>) -> Self {
        Self { participants }
    }

    /// Undo up to `count` committed turns on the session (messages + files).
    pub fn undo_turns(session: &mut Session, count: usize) -> UndoResult {
        let mut undone = 0usize;
        let mut message_count = session.messages.len();
        for _ in 0..count {
            match session.undo_last_turn() {
                Ok(n) => {
                    message_count = n;
                    undone += 1;
                }
                Err(_) => break,
            }
        }
        UndoResult {
            undone_turns: undone,
            message_count,
            participants: default_undo_participants(),
        }
    }

    pub fn participants(&self) -> &[UndoParticipant] {
        &self.participants
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kkagent_protocol::PermissionMode;
    use std::path::PathBuf;

    #[test]
    fn undo_empty() {
        let mut s = Session::new(
            "t".into(),
            PathBuf::from("/tmp"),
            PermissionMode::Manual,
            "m".into(),
        );
        let r = UndoService::undo_turns(&mut s, 1);
        assert_eq!(r.undone_turns, 0);
    }
}
