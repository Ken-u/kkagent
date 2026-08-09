//! Text undo/redo stack for the editor.

#[derive(Debug, Clone)]
pub struct EditorSnapshot {
    pub text: String,
    pub cursor: usize,
}

#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<EditorSnapshot>,
    redo: Vec<EditorSnapshot>,
    cap: usize,
}

impl UndoStack {
    pub fn new(cap: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            cap: cap.max(8),
        }
    }

    pub fn push(&mut self, snap: EditorSnapshot) {
        if self
            .undo
            .last()
            .map(|s| s.text == snap.text)
            .unwrap_or(false)
        {
            return;
        }
        self.undo.push(snap);
        if self.undo.len() > self.cap {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self, current: EditorSnapshot) -> Option<EditorSnapshot> {
        let prev = self.undo.pop()?;
        self.redo.push(current);
        Some(prev)
    }

    pub fn redo(&mut self, current: EditorSnapshot) -> Option<EditorSnapshot> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        Some(next)
    }
}
