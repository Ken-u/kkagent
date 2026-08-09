//! Reverse-RPC UI layer for approval / question modals (kimi-code tui/reverse-rpc).

use kkagent_protocol::{ApprovalDecision, ApprovalScope, QuestionOption, QuestionPayload};

#[derive(Debug, Clone)]
pub struct ApprovalPanelData {
    pub request_id: String,
    pub tool_name: String,
    pub summary: String,
    pub detail: Option<String>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct QuestionPanelData {
    pub question_id: String,
    pub text: String,
    pub options: Vec<QuestionOption>,
    pub allow_free_text: bool,
    pub selected: usize,
    pub free_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKind {
    Approval,
    Question,
}

/// Coordinates exclusive modal display (approval vs question).
#[derive(Debug, Default)]
pub struct ModalCoordinator {
    active: Option<ModalKind>,
    pub approval: Option<ApprovalPanelData>,
    pub question: Option<QuestionPanelData>,
}

impl ModalCoordinator {
    pub fn show_approval(&mut self, data: ApprovalPanelData) {
        self.question = None;
        self.approval = Some(data);
        self.active = Some(ModalKind::Approval);
    }

    pub fn show_question(&mut self, data: QuestionPanelData) {
        self.approval = None;
        self.question = Some(data);
        self.active = Some(ModalKind::Question);
    }

    pub fn hide(&mut self, kind: ModalKind) {
        match kind {
            ModalKind::Approval => self.approval = None,
            ModalKind::Question => self.question = None,
        }
        if self.active == Some(kind) {
            self.active = if self.approval.is_some() {
                Some(ModalKind::Approval)
            } else if self.question.is_some() {
                Some(ModalKind::Question)
            } else {
                None
            };
        }
    }

    pub fn clear(&mut self) {
        self.approval = None;
        self.question = None;
        self.active = None;
    }

    pub fn active(&self) -> Option<ModalKind> {
        self.active
    }
}

#[derive(Debug, Clone)]
pub enum ReverseRpcResponse {
    Approval {
        request_id: String,
        decision: ApprovalDecision,
        scope: Option<ApprovalScope>,
    },
    Question {
        question_id: String,
        selected: Vec<String>,
        free_text: Option<String>,
    },
}

#[derive(Debug, Default)]
pub struct ApprovalController {
    pending: Option<ApprovalPanelData>,
}

impl ApprovalController {
    pub fn present(&mut self, data: ApprovalPanelData) -> &ApprovalPanelData {
        self.pending = Some(data);
        self.pending.as_ref().unwrap()
    }

    pub fn select(&mut self, idx: usize) {
        if let Some(p) = &mut self.pending {
            p.selected = idx.min(2);
        }
    }

    pub fn move_sel(&mut self, delta: i32) {
        if let Some(p) = &mut self.pending {
            let n = p.selected as i32 + delta;
            p.selected = n.clamp(0, 2) as usize;
        }
    }

    pub fn resolve(&mut self) -> Option<ReverseRpcResponse> {
        let p = self.pending.take()?;
        let (decision, scope) = match p.selected {
            0 => (ApprovalDecision::Approved, None),
            1 => (ApprovalDecision::Approved, Some(ApprovalScope::Session)),
            _ => (ApprovalDecision::Rejected, None),
        };
        Some(ReverseRpcResponse::Approval {
            request_id: p.request_id,
            decision,
            scope,
        })
    }

    pub fn reject(&mut self) -> Option<ReverseRpcResponse> {
        let p = self.pending.take()?;
        Some(ReverseRpcResponse::Approval {
            request_id: p.request_id,
            decision: ApprovalDecision::Rejected,
            scope: None,
        })
    }

    pub fn pending(&self) -> Option<&ApprovalPanelData> {
        self.pending.as_ref()
    }
}

#[derive(Debug, Default)]
pub struct QuestionController {
    pending: Option<QuestionPanelData>,
}

impl QuestionController {
    pub fn from_payload(q: &QuestionPayload) -> QuestionPanelData {
        QuestionPanelData {
            question_id: q.question_id.clone(),
            text: q.text.clone(),
            options: q.options.clone(),
            allow_free_text: q.allow_free_text,
            selected: 0,
            free_text: String::new(),
        }
    }

    pub fn present(&mut self, data: QuestionPanelData) -> &QuestionPanelData {
        self.pending = Some(data);
        self.pending.as_ref().unwrap()
    }

    pub fn move_sel(&mut self, delta: i32) {
        if let Some(p) = &mut self.pending {
            if p.options.is_empty() {
                return;
            }
            let n = p.selected as i32 + delta;
            let max = (p.options.len() - 1) as i32;
            p.selected = n.clamp(0, max) as usize;
        }
    }

    pub fn resolve(&mut self) -> Option<ReverseRpcResponse> {
        let p = self.pending.take()?;
        let selected = p
            .options
            .get(p.selected)
            .map(|o| vec![o.label.clone()])
            .unwrap_or_default();
        let free = if p.allow_free_text && !p.free_text.is_empty() {
            Some(p.free_text)
        } else {
            None
        };
        Some(ReverseRpcResponse::Question {
            question_id: p.question_id,
            selected,
            free_text: free,
        })
    }

    pub fn pending(&self) -> Option<&QuestionPanelData> {
        self.pending.as_ref()
    }

    pub fn pending_mut(&mut self) -> Option<&mut QuestionPanelData> {
        self.pending.as_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_resolve() {
        let mut c = ApprovalController::default();
        c.present(ApprovalPanelData {
            request_id: "r1".into(),
            tool_name: "Bash".into(),
            summary: "run".into(),
            detail: None,
            selected: 1,
        });
        let r = c.resolve().unwrap();
        match r {
            ReverseRpcResponse::Approval { scope, .. } => {
                assert!(matches!(scope, Some(ApprovalScope::Session)));
            }
            _ => panic!("expected approval"),
        }
    }
}
