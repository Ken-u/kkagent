use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionResponse {
    pub question_id: String,
    /// Selected option ids (supports multi-select).
    #[serde(default)]
    pub selected_option_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
    /// True when the user dismissed / interrupted the question.
    #[serde(default)]
    pub cancelled: bool,
}
