//! Full compaction aligned with kimi-code agent-core:
//! LLM summary → early trigger → user-message retention → overflow recovery →
//! blocking protection. Tool exchanges are dropped from the live context after
//! compaction (covered by the summary) so Anthropic-style tool_use/tool_result
//! pairing cannot 400 the next request.

use kkagent_config::AppConfig;
use kkagent_llm::{create_provider, ChatContent, ChatMessage, LlmRequest, StreamEvent, ToolDef};
use kkagent_protocol::TokenUsage;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::context_projector::{
    build_compaction_digest, fold_old_media, project_for_compaction, repair_tool_exchanges,
    ProjectOptions,
};
use crate::dynamic_tools::strip_dynamic_tool_context;
use crate::token_counting::TokenCounter;
use crate::usage::usage_location;

/// Fraction of the model context window that triggers auto-compaction.
pub const DEFAULT_TRIGGER_RATIO: f64 = 0.85;
/// Fraction that blocks the turn until compaction finishes.
pub const DEFAULT_BLOCK_RATIO: f64 = 0.85;
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: u64 = 20_000;
pub const COMPACT_USER_MESSAGE_HEAD_TOKENS: u64 = 2_000;
pub const MAX_OVERFLOW_COMPACTION_ATTEMPTS: u32 = 3;
pub const MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS: u32 = 3;
const OVERFLOW_CONTEXT_SAFETY_RATIO: f64 = 0.85;
const COMPACTION_OVERFLOW_SHRINK_RATIOS: [f64; 3] = [0.7, 0.5, 0.35];

const SUMMARY_PREFIX: &str = "The conversation so far has been compacted to free up context. What follows is your own working summary of this task — use it to continue your train of thought rather than starting over. Treat it as notes, not proof: where it says a step was done, tests passed, or a fix worked, verify that yourself before relying on it. Any user messages earlier in this context are preserved verbatim from the compacted conversation; where a system-reminder note among them marks an omitted middle section, the user messages it replaced are covered by this summary.";

const COMPACTION_INSTRUCTION: &str = r#"You are about to run out of context. Write a first-person handoff note to yourself so you can seamlessly continue this task after the earlier conversation is cleared.

--- This message is a direct task, not part of the above conversation ---

Write the note as your own continuing train of thought — first person, present tense. Do not write a third-party report. Write the note in the same language the conversation has been using.

Make the note self-sufficient: the next turn will see only your most recent user messages and this note — every assistant message, tool call, and tool result above will be gone. Preserve:

- What the latest request is actually asking for
- Instructions and constraints currently in force
- What has actually been done (exact commands, paths, results)
- What you still don't know
- The forward plan and next concrete step

Be concise and proportional to the task. Respond with text only. Do not call any tools."#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStrategy {
    /// kimi-style: LLM/local summary + retained user messages (default).
    KeepUsers,
    /// Legacy: keep last N messages + digest.
    KeepTail,
    /// Drop vacuous noise then keep tail.
    VacuousFold,
    /// Aggressive handoff of user notes only.
    Handoff,
}

impl CompactionStrategy {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "vacuous" | "vacuous_fold" => Self::VacuousFold,
            "handoff" => Self::Handoff,
            "tail" | "keep_tail" => Self::KeepTail,
            _ => Self::KeepUsers,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CompactionPolicy {
    pub trigger_ratio: f64,
    pub block_ratio: f64,
    pub reserved_context_size: u64,
    pub max_overflow_compaction_attempts: u32,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            trigger_ratio: DEFAULT_TRIGGER_RATIO,
            block_ratio: DEFAULT_BLOCK_RATIO,
            reserved_context_size: 50_000,
            max_overflow_compaction_attempts: MAX_OVERFLOW_COMPACTION_ATTEMPTS,
        }
    }
}

impl CompactionPolicy {
    pub fn from_loop_control(lc: &kkagent_config::LoopControlConfig) -> Self {
        Self {
            trigger_ratio: lc.compact_trigger_ratio.unwrap_or(DEFAULT_TRIGGER_RATIO),
            block_ratio: lc.compact_block_ratio.unwrap_or(DEFAULT_BLOCK_RATIO),
            reserved_context_size: lc.reserved_context_size,
            max_overflow_compaction_attempts: lc
                .compact_max_overflow_attempts
                .unwrap_or(MAX_OVERFLOW_COMPACTION_ATTEMPTS),
        }
    }

    pub fn should_compact(self, max_context: u64, used: u64) -> bool {
        if max_context == 0 {
            return false;
        }
        let ratio_hit = used as f64 >= max_context as f64 * self.trigger_ratio;
        let reserved = self.reserved_context_size;
        let reserved_hit = if reserved == 0 {
            false
        } else if reserved >= max_context {
            // No usable budget left — always compact (legacy kkagent behavior).
            true
        } else {
            used.saturating_add(reserved) >= max_context
        };
        ratio_hit || reserved_hit
    }

    pub fn should_block(self, max_context: u64, used: u64) -> bool {
        if max_context == 0 {
            return false;
        }
        let ratio_hit = used as f64 >= max_context as f64 * self.block_ratio;
        let reserved = self.reserved_context_size;
        let reserved_hit = if reserved == 0 {
            false
        } else if reserved >= max_context {
            true
        } else {
            used.saturating_add(reserved) >= max_context
        };
        ratio_hit || reserved_hit
    }
}

#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub dropped: usize,
    pub strategy: CompactionStrategy,
    pub kept_user_message_count: usize,
    pub kept_head_user_message_count: Option<usize>,
    pub summarizer_dropped_count: usize,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct CompactionUserSelection {
    pub head: Vec<ChatMessage>,
    pub tail: Vec<ChatMessage>,
    pub elided: bool,
    pub omitted_tokens: u64,
}

fn message_text(msg: &ChatMessage) -> String {
    msg.content
        .iter()
        .filter_map(|c| match c {
            ChatContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn is_tool_result_only(msg: &ChatMessage) -> bool {
    msg.role == "user"
        && !msg.content.is_empty()
        && msg
            .content
            .iter()
            .all(|c| matches!(c, ChatContent::ToolResult { .. }))
}

fn is_compaction_summary_message(msg: &ChatMessage) -> bool {
    let text = message_text(msg);
    text.contains("has been compacted to free up context")
        || text.contains("<compaction-handoff>")
        || (text.contains("<system-reminder>") && text.contains("Conversation compacted"))
}

fn is_elision_marker(msg: &ChatMessage) -> bool {
    let text = message_text(msg);
    text.contains("were omitted here during compaction")
}

/// Genuine user prompts kept verbatim after compaction (not harness / tool results).
pub fn is_real_user_input(msg: &ChatMessage) -> bool {
    if msg.role != "user" || is_tool_result_only(msg) {
        return false;
    }
    if is_compaction_summary_message(msg) || is_elision_marker(msg) {
        return false;
    }
    let text = message_text(msg);
    if text.trim().is_empty() {
        // Allow image-only user turns.
        return msg
            .content
            .iter()
            .any(|c| matches!(c, ChatContent::Image { .. } | ChatContent::Video { .. }));
    }
    !kkagent_protocol::is_harness_only_user_text(&text)
}

pub fn collect_compactable_user_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|m| is_real_user_input(m))
        .cloned()
        .collect()
}

fn truncate_text_to_tokens(text: &str, max_tokens: u64) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    let mut end = 0usize;
    for ch in text.chars() {
        if (ch as u32) <= 127 {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
        if ascii.div_ceil(4) + non_ascii > max_tokens {
            break;
        }
        end += ch.len_utf8();
    }
    text[..end].to_string()
}

fn truncate_text_to_tokens_from_end(text: &str, max_tokens: u64) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    let chars: Vec<char> = text.chars().collect();
    let mut start_idx = chars.len();
    for i in (0..chars.len()).rev() {
        let ch = chars[i];
        if (ch as u32) <= 127 {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
        if ascii.div_ceil(4) + non_ascii > max_tokens {
            break;
        }
        start_idx = i;
    }
    chars[start_idx..].iter().collect()
}

fn replace_message_text(msg: &ChatMessage, text: String) -> ChatMessage {
    ChatMessage {
        role: msg.role.clone(),
        content: vec![ChatContent::Text { text }],
        tools: None,
    }
}

fn truncate_user_message(msg: &ChatMessage, max_tokens: u64) -> ChatMessage {
    replace_message_text(msg, truncate_text_to_tokens(&message_text(msg), max_tokens))
}

/// Head (oldest) + tail (newest) user retention with elision when over budget.
pub fn select_compaction_user_messages(
    messages: &[ChatMessage],
    max_tokens: u64,
    head_tokens: u64,
) -> CompactionUserSelection {
    let total: u64 = messages.iter().map(TokenCounter::estimate_message).sum();
    if total <= max_tokens {
        return CompactionUserSelection {
            head: Vec::new(),
            tail: messages.to_vec(),
            elided: false,
            omitted_tokens: 0,
        };
    }

    let head_budget = head_tokens.min(max_tokens);
    let tail_budget = max_tokens.saturating_sub(head_budget);

    let mut tail = Vec::new();
    let mut tail_remaining = tail_budget;
    let mut head_end_exclusive = messages.len();
    let mut boundary_prefix: Option<ChatMessage> = None;

    for i in (0..messages.len()).rev() {
        if tail_remaining == 0 {
            break;
        }
        let message = &messages[i];
        let tokens = TokenCounter::estimate_message(message);
        if tokens <= tail_remaining {
            tail.push(message.clone());
            tail_remaining -= tokens;
            head_end_exclusive = i;
            continue;
        }
        let full = message_text(message);
        let kept_suffix = truncate_text_to_tokens_from_end(&full, tail_remaining);
        tail.push(replace_message_text(message, kept_suffix.clone()));
        head_end_exclusive = i;
        let dropped_len = full.len().saturating_sub(kept_suffix.len());
        if dropped_len > 0 {
            boundary_prefix = Some(replace_message_text(
                message,
                full[..dropped_len].to_string(),
            ));
        }
        break;
    }
    tail.reverse();

    let mut head_candidates: Vec<ChatMessage> = messages[..head_end_exclusive].to_vec();
    if let Some(prefix) = boundary_prefix {
        head_candidates.push(prefix);
    }

    let mut head = Vec::new();
    let mut head_remaining = head_budget;
    for message in head_candidates {
        if head_remaining == 0 {
            break;
        }
        let tokens = TokenCounter::estimate_message(&message);
        if tokens <= head_remaining {
            head.push(message);
            head_remaining -= tokens;
        } else {
            head.push(truncate_user_message(&message, head_remaining));
            break;
        }
    }

    let kept: u64 = head
        .iter()
        .chain(tail.iter())
        .map(TokenCounter::estimate_message)
        .sum();
    CompactionUserSelection {
        head,
        tail,
        elided: true,
        omitted_tokens: total.saturating_sub(kept),
    }
}

pub fn build_compaction_elision_text(omitted_tokens: u64) -> String {
    format!(
        "<system-reminder>\n\
Some of this conversation's user messages were omitted here during compaction: \
the messages above this note are the oldest user input, the messages below are the most recent, \
and roughly {omitted_tokens} tokens in between were dropped. The omitted content is covered by \
the compaction summary at the end of the conversation.\n\
</system-reminder>"
    )
}

pub fn build_compaction_summary_text(summary: &str) -> String {
    let body = summary.trim();
    format!(
        "{SUMMARY_PREFIX}\n{}",
        if body.is_empty() {
            "(no summary available)"
        } else {
            body
        }
    )
}

/// Rebuild live context as kept user messages + summary (no assistant/tool left).
/// This is the primary toolcall-400 protection: unpaired tool_use/tool_result
/// cannot survive into the next provider request.
pub fn apply_compaction(messages: &mut Vec<ChatMessage>, raw_summary: &str) -> CompactionResult {
    let before = messages.len();
    let compactable = collect_compactable_user_messages(messages);
    let selection = select_compaction_user_messages(
        &compactable,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        COMPACT_USER_MESSAGE_HEAD_TOKENS,
    );

    let mut kept = Vec::new();
    kept.extend(selection.head.iter().cloned());
    let kept_head = if selection.elided {
        Some(selection.head.len())
    } else {
        None
    };
    if selection.elided {
        kept.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: build_compaction_elision_text(selection.omitted_tokens),
            }],
            tools: None,
        });
    }
    kept.extend(selection.tail.iter().cloned());
    let kept_user_count = selection.head.len() + selection.tail.len();

    let summary_text = build_compaction_summary_text(raw_summary);
    kept.push(ChatMessage {
        role: "user".into(),
        content: vec![ChatContent::Text {
            text: summary_text.clone(),
        }],
        tools: None,
    });

    messages.clear();
    messages.extend(kept);

    CompactionResult {
        dropped: before.saturating_sub(messages.len().saturating_sub(1)),
        strategy: CompactionStrategy::KeepUsers,
        kept_user_message_count: kept_user_count,
        kept_head_user_message_count: kept_head,
        summarizer_dropped_count: 0,
        summary: summary_text,
    }
}

/// Drop leading tool-result-only user messages (Anthropic history shape).
pub fn drop_leading_tool_result_only(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    while messages.first().is_some_and(is_tool_result_only) {
        messages.remove(0);
    }
    messages
}

pub fn shrink_history_for_summarizer(messages: &[ChatMessage], attempt: u32) -> Vec<ChatMessage> {
    if messages.len() <= 1 {
        return messages.to_vec();
    }
    let idx = (attempt.saturating_sub(1) as usize).min(COMPACTION_OVERFLOW_SHRINK_RATIOS.len() - 1);
    let ratio = COMPACTION_OVERFLOW_SHRINK_RATIOS[idx];
    let budget = ((TokenCounter::estimate_messages(messages) as f64) * ratio).floor() as u64;
    take_recent_within_budget(messages, budget.max(1))
}

fn take_recent_within_budget(messages: &[ChatMessage], token_budget: u64) -> Vec<ChatMessage> {
    let mut start = messages.len();
    let mut tokens = 0u64;
    for i in (0..messages.len()).rev() {
        let t = TokenCounter::estimate_message(&messages[i]);
        if tokens.saturating_add(t) > token_budget {
            break;
        }
        tokens += t;
        start = i;
    }
    if start == 0 {
        start = 1;
    }
    drop_leading_tool_result_only(messages[start..].to_vec())
}

pub fn drop_oldest_and_leading_tool_results(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    if messages.len() <= 1 {
        return messages.to_vec();
    }
    drop_leading_tool_result_only(messages[1..].to_vec())
}

/// Lower observed max context after a provider overflow (kimi safety ratio).
pub fn observe_context_overflow(estimated_request_tokens: u64, configured_max: u64) -> u64 {
    if estimated_request_tokens == 0 {
        return configured_max;
    }
    let observed = ((estimated_request_tokens as f64) * OVERFLOW_CONTEXT_SAFETY_RATIO)
        .floor()
        .max(1.0) as u64;
    if configured_max == 0 {
        observed
    } else {
        configured_max.min(observed)
    }
}

fn build_summarizer_messages(
    history: &[ChatMessage],
    custom_instruction: Option<&str>,
) -> Vec<ChatMessage> {
    let stripped = strip_dynamic_tool_context(history);
    let mut projected = project_for_compaction(&stripped, &ProjectOptions::default());
    repair_tool_exchanges(&mut projected, true);
    let mut instruction = COMPACTION_INSTRUCTION.to_string();
    if let Some(extra) = custom_instruction.map(str::trim).filter(|s| !s.is_empty()) {
        instruction.push_str("\n\nOptional user instruction:\n");
        instruction.push_str(extra);
    }
    projected.push(ChatMessage {
        role: "user".into(),
        content: vec![ChatContent::Text { text: instruction }],
        tools: None,
    });
    projected
}

/// Resolve the model alias used for compaction summaries.
///
/// Priority: `compaction_model` > `secondary_model` > session model alias >
/// `default_model`. Empty strings are ignored.
pub fn resolve_compaction_model_alias(
    config: &AppConfig,
    session_model_alias: Option<&str>,
) -> Option<String> {
    config
        .compaction_model
        .clone()
        .filter(|m| !m.is_empty())
        .or_else(|| config.secondary_model.clone().filter(|m| !m.is_empty()))
        .or_else(|| {
            session_model_alias.and_then(|s| {
                // Only honor aliases that still resolve; stale session aliases
                // fall through to the validated global default.
                if !s.is_empty() && config.resolve_model(s).is_some() {
                    Some(s.to_string())
                } else {
                    None
                }
            })
        })
        .or_else(|| config.default_model_alias().map(|s| s.to_string()))
}

/// LLM summary with overflow shrink + empty/truncated retry. Never panics.
/// Falls back to a local digest when the model is unavailable. The extra
/// return values are the dropped-message count and, when an LLM call
/// succeeded, the `(model alias, token usage)` of the summarizer call.
pub async fn summarize_history_with_llm(
    config: Arc<AppConfig>,
    history: &[ChatMessage],
    custom_instruction: Option<&str>,
    session_model_alias: Option<&str>,
) -> (String, usize, Option<(String, TokenUsage)>) {
    let mut history_for_model = history.to_vec();
    let mut dropped_count = 0usize;
    let mut media_strip_attempted = false;
    let mut overflow_shrink = 0u32;
    let mut empty_shrink = 0u32;

    let alias = resolve_compaction_model_alias(&config, session_model_alias);
    let Some(alias) = alias else {
        return (local_digest_summary(history), 0, None);
    };
    let Some((model_cfg, provider_cfg)) = config.resolve_model(&alias) else {
        return (local_digest_summary(history), 0, None);
    };

    loop {
        let messages = build_summarizer_messages(&history_for_model, custom_instruction);
        match stream_summary(provider_cfg, model_cfg, messages).await {
            Ok((text, usage)) if !text.trim().is_empty() => {
                return (text, dropped_count, Some((alias, usage)));
            }
            Ok(_) => {
                empty_shrink += 1;
                if empty_shrink > 5 || history_for_model.len() <= 1 {
                    break;
                }
                let before = history_for_model.len();
                history_for_model = drop_oldest_and_leading_tool_results(&history_for_model);
                dropped_count += before.saturating_sub(history_for_model.len());
            }
            Err(err) => {
                let lower = err.to_ascii_lowercase();
                let too_large = lower.contains("413")
                    || lower.contains("too large")
                    || lower.contains("context")
                    || lower.contains("overflow");
                if too_large && !media_strip_attempted {
                    media_strip_attempted = true;
                    let folded = fold_old_media(&mut history_for_model, 0);
                    if folded > 0 {
                        continue;
                    }
                }
                if too_large && history_for_model.len() > 1 {
                    overflow_shrink += 1;
                    if overflow_shrink > MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS {
                        break;
                    }
                    let before = history_for_model.len();
                    history_for_model =
                        shrink_history_for_summarizer(&history_for_model, overflow_shrink);
                    dropped_count += before.saturating_sub(history_for_model.len());
                    continue;
                }
                tracing::warn!("compaction LLM summary failed: {err}");
                break;
            }
        }
    }

    (local_digest_summary(history), dropped_count, None)
}

fn local_digest_summary(history: &[ChatMessage]) -> String {
    if history.is_empty() {
        return "No earlier turns.".into();
    }
    build_compaction_digest(history)
}

async fn stream_summary(
    provider_cfg: &kkagent_config::ProviderConfig,
    model_cfg: &kkagent_config::ModelConfig,
    messages: Vec<ChatMessage>,
) -> Result<(String, TokenUsage), String> {
    let provider = create_provider(provider_cfg, model_cfg).map_err(|e| e.to_string())?;
    let (tx, mut rx) = mpsc::channel(64);
    let request = LlmRequest {
        model: model_cfg.model.clone(),
        messages,
        tools: Vec::<ToolDef>::new(),
        max_tokens: Some(4096),
        system: Some(
            "You compress conversation history into a concise factual handoff note.".into(),
        ),
        thinking: None,
        prompt_cache_key: None,
        first_token_timeout: kkagent_config::resolve_first_token_timeout(model_cfg, provider_cfg),
    };
    let handle = tokio::spawn(async move {
        if let Err(error) = provider.stream_chat(request, tx.clone()).await {
            let _ = tx.send(kkagent_llm::stream_error_event(&error)).await;
        }
    });
    let mut out = String::new();
    let mut usage = TokenUsage {
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        input_includes_cache: None,
    };
    let mut complete = false;
    let collected = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(t) => out.push_str(&t),
                StreamEvent::MessageEnd {
                    usage: step_usage, ..
                } => {
                    usage = TokenUsage {
                        input_tokens: step_usage.input_tokens,
                        output_tokens: step_usage.output_tokens,
                        cache_creation_input_tokens: step_usage.cache_creation_input_tokens,
                        cache_read_input_tokens: step_usage.cache_read_input_tokens,
                        input_includes_cache: step_usage.input_includes_cache,
                    };
                    complete = true;
                    break;
                }
                StreamEvent::Error(error) => return Err(error),
                StreamEvent::RateLimited { message, .. } => return Err(message),
                _ => {}
            }
        }
        Ok(())
    })
    .await;
    handle.abort();
    match collected {
        Ok(Ok(())) if complete && !out.trim().is_empty() => Ok((out, usage)),
        Ok(Ok(())) => Err("empty or incomplete compaction summary".into()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("compaction summary timed out".into()),
    }
}

/// Run full KeepUsers compaction (LLM when possible). When `usage` is given,
/// the summarizer call's tokens are attributed to `(alias, "compaction")` in
/// that usage tracker.
pub async fn compact_full_async(
    config: Arc<AppConfig>,
    messages: &mut Vec<ChatMessage>,
    custom_instruction: Option<&str>,
    session_model_alias: Option<&str>,
    usage: Option<&mut crate::usage::UsageService>,
) -> CompactionResult {
    if messages.is_empty() {
        return CompactionResult {
            dropped: 0,
            strategy: CompactionStrategy::KeepUsers,
            kept_user_message_count: 0,
            kept_head_user_message_count: None,
            summarizer_dropped_count: 0,
            summary: String::new(),
        };
    }
    let (raw, summarizer_dropped, summarizer_usage) =
        summarize_history_with_llm(config, messages, custom_instruction, session_model_alias).await;
    if let (Some(usage), Some((alias, step))) = (usage, summarizer_usage) {
        usage.record_labeled(&step, &alias, usage_location::COMPACTION);
    }
    let mut result = apply_compaction(messages, &raw);
    result.summarizer_dropped_count = summarizer_dropped;
    result
}

/// Sync fallback used by auto-compact when an async LLM call is unavailable.
pub fn compact_full(
    messages: &mut Vec<ChatMessage>,
    keep_last: usize,
    strategy: CompactionStrategy,
) -> CompactionResult {
    match strategy {
        CompactionStrategy::KeepUsers => {
            let digest = if messages.is_empty() {
                "No earlier turns.".into()
            } else {
                build_compaction_digest(messages)
            };
            apply_compaction(messages, &digest)
        }
        CompactionStrategy::KeepTail => {
            use crate::context_projector::{compact_cut_index, compact_messages};
            let cut = compact_cut_index(messages, keep_last);
            let digest = if cut == 0 {
                "No earlier turns.".into()
            } else {
                build_compaction_digest(&messages[..cut])
            };
            let dropped = compact_messages(messages, keep_last, &digest);
            CompactionResult {
                dropped,
                strategy,
                kept_user_message_count: 0,
                kept_head_user_message_count: None,
                summarizer_dropped_count: 0,
                summary: digest,
            }
        }
        CompactionStrategy::VacuousFold => {
            let before = messages.len();
            messages.retain(|m| !is_vacuous(m));
            let mut result = compact_full(messages, keep_last, CompactionStrategy::KeepTail);
            result.dropped += before.saturating_sub(messages.len());
            result.strategy = CompactionStrategy::VacuousFold;
            result
        }
        CompactionStrategy::Handoff => {
            let mut kept = Vec::new();
            for m in messages.iter() {
                if is_real_user_input(m) {
                    if let Some(text) = first_text(m) {
                        kept.push(ChatMessage {
                            role: "user".into(),
                            content: vec![ChatContent::Text {
                                text: text.chars().take(500).collect(),
                            }],
                            tools: None,
                        });
                    }
                }
            }
            let digest = format!(
                "Handoff summary of prior work ({} user notes):\n{}",
                kept.len(),
                kept.iter()
                    .filter_map(first_text)
                    .take(12)
                    .collect::<Vec<_>>()
                    .join("\n—\n")
            );
            apply_compaction(messages, &digest)
        }
    }
}

fn is_vacuous(m: &ChatMessage) -> bool {
    match m.content.as_slice() {
        [ChatContent::Text { text }] => {
            let t = text.trim();
            t.is_empty() || t == "ok" || t == "OK" || t.starts_with("Skipped: duplicate tool call")
        }
        [ChatContent::ToolResult {
            content, is_error, ..
        }] => !*is_error && (content.is_empty() || content == "(no output)"),
        _ => false,
    }
}

fn first_text(m: &ChatMessage) -> Option<&str> {
    m.content.iter().find_map(|c| match c {
        ChatContent::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_trigger_at_ratio() {
        let p = CompactionPolicy {
            trigger_ratio: 0.85,
            block_ratio: 0.85,
            reserved_context_size: 0,
            max_overflow_compaction_attempts: 3,
        };
        assert!(!p.should_compact(1000, 800));
        assert!(p.should_compact(1000, 850));
        assert!(p.should_compact(1000, 900));
    }

    #[test]
    fn reserved_triggers_early() {
        let p = CompactionPolicy {
            trigger_ratio: 0.99,
            block_ratio: 0.99,
            reserved_context_size: 200,
            max_overflow_compaction_attempts: 3,
        };
        assert!(p.should_compact(1000, 850));
        assert!(!p.should_compact(1000, 700));
    }

    #[test]
    fn apply_keeps_users_drops_tools() {
        let mut msgs = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "please read a.rs".into(),
                }],
                tools: None,
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                }],
                tools: None,
            },
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "fn main() {}".into(),
                    is_error: false,
                }],
                tools: None,
            },
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "continue".into(),
                }],
                tools: None,
            },
        ];
        let result = apply_compaction(&mut msgs, "read a.rs then continue");
        assert_eq!(result.kept_user_message_count, 2);
        assert!(msgs.iter().all(|m| {
            !m.content.iter().any(|c| {
                matches!(
                    c,
                    ChatContent::ToolUse { .. } | ChatContent::ToolResult { .. }
                )
            })
        }));
        assert!(msgs.iter().any(is_compaction_summary_message));
        assert!(msgs.iter().any(|m| message_text(m).contains("please read")));
        assert!(msgs.iter().any(|m| message_text(m).contains("continue")));
    }

    #[test]
    fn head_tail_elision_when_over_budget() {
        let msgs: Vec<_> = (0..40)
            .map(|i| ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: format!("user message number {i} {}", "x".repeat(800)),
                }],
                tools: None,
            })
            .collect();
        let sel = select_compaction_user_messages(&msgs, 4_000, 800);
        assert!(sel.elided);
        assert!(!sel.head.is_empty());
        assert!(!sel.tail.is_empty());
        assert!(message_text(&sel.head[0]).contains("number 0"));
        assert!(message_text(sel.tail.last().unwrap()).contains("number 39"));
    }

    #[test]
    fn harness_messages_not_kept_as_user() {
        let msg = ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: "<system-reminder>\nplan mode\n</system-reminder>".into(),
            }],
            tools: None,
        };
        assert!(!is_real_user_input(&msg));
    }

    fn test_config() -> kkagent_config::AppConfig {
        use kkagent_config::{AppConfig, ModelConfig, ProviderConfig};
        use std::collections::HashMap;
        let mut config = AppConfig::default();
        let provider = ProviderConfig {
            provider_type: "openai".into(),
            api_key: None,
            api_key_env: None,
            base_url: Some("https://example.test".into()),
            custom_headers: HashMap::new(),
            oauth: None,
            first_token_timeout_ms: None,
            extra_fields: Default::default(),
        };
        config.providers.insert("test".into(), provider);
        for alias in ["default", "session", "secondary", "compaction"] {
            config.models.insert(
                alias.into(),
                ModelConfig {
                    provider: "test".into(),
                    model: format!("upstream-{alias}"),
                    max_context_size: Some(100_000),
                    max_output_size: Some(4_096),
                    capabilities: vec!["tool_use".into()],
                    display_name: None,
                    support_efforts: Vec::new(),
                    default_effort: None,
                    pricing: None,
                    experimental_adaptive_thinking: false,
                    experimental_vision_proxy: false,
                    experimental_visible_empty_retries: 0,
                    experimental_bad_toolcall_auto_retries: 0,
                    first_token_timeout_ms: None,
                },
            );
        }
        config.default_model = Some("default".into());
        config
    }

    #[test]
    fn compaction_model_priority() {
        let mut config = test_config();

        // No dedicated models configured: session alias wins over default.
        assert_eq!(
            resolve_compaction_model_alias(&config, Some("session")),
            Some("session".into())
        );
        // No session alias: global default.
        assert_eq!(
            resolve_compaction_model_alias(&config, None),
            Some("default".into())
        );
        // Stale session alias that no longer resolves: falls back to default.
        assert_eq!(
            resolve_compaction_model_alias(&config, Some("ghost")),
            Some("default".into())
        );
        // Empty strings are ignored at every level.
        let mut empty = test_config();
        empty.default_model = None;
        assert_eq!(resolve_compaction_model_alias(&empty, Some("")), None);

        // secondary_model outranks session alias and default.
        config.secondary_model = Some("secondary".into());
        assert_eq!(
            resolve_compaction_model_alias(&config, Some("session")),
            Some("secondary".into())
        );

        // compaction_model is the highest priority.
        config.compaction_model = Some("compaction".into());
        assert_eq!(
            resolve_compaction_model_alias(&config, Some("session")),
            Some("compaction".into())
        );
    }
}
