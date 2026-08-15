use kkagent_protocol::{ApprovalResponse, Frame, PermissionMode};
use kkagent_rpc::{RpcClient, RpcConnectionState};
use tokio::sync::mpsc;

pub struct KkagentClient {
    rpc: RpcClient,
    pub event_rx: mpsc::Receiver<Frame>,
}

/// Cloneable request-only handle for RPC work that should not block the UI loop.
#[derive(Clone)]
pub struct KkagentRequester {
    rpc: RpcClient,
}

impl KkagentRequester {
    pub async fn rpc_call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        self.rpc
            .call(method, params)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}

impl KkagentClient {
    pub fn new(rpc: RpcClient, event_rx: mpsc::Receiver<Frame>) -> Self {
        Self { rpc, event_rx }
    }

    pub fn requester(&self) -> KkagentRequester {
        KkagentRequester {
            rpc: self.rpc.clone(),
        }
    }

    pub fn connection_state(&self) -> RpcConnectionState {
        self.rpc.connection_state()
    }

    pub async fn create_session(
        &self,
        workspace: Option<&str>,
        permission: Option<PermissionMode>,
    ) -> anyhow::Result<String> {
        let params = serde_json::json!({
            "workspace": workspace,
            "permission_mode": permission,
        });
        let result = self
            .rpc
            .call("sessions.create", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let session_id = result
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("sessions.create response has no session_id"))?
            .to_string();
        Ok(session_id)
    }

    pub async fn list_sessions(&self, limit: usize) -> anyhow::Result<Vec<serde_json::Value>> {
        let params = serde_json::json!({"limit": limit});
        let result = self
            .rpc
            .call("sessions.list", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let sessions = result
            .get("sessions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(sessions)
    }

    pub async fn send_prompt(&self, session_id: &str, text: &str) -> anyhow::Result<()> {
        self.send_prompt_with_images(session_id, text, &[]).await
    }

    pub async fn send_prompt_with_images(
        &self,
        session_id: &str,
        text: &str,
        images: &[(String, String)],
    ) -> anyhow::Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "text": text,
            "images": images.iter().map(|(media_type, data)| serde_json::json!({
                "media_type": media_type,
                "data": data,
            })).collect::<Vec<_>>(),
        });
        self.rpc
            .call("session.prompt", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn steer(
        &self,
        session_id: &str,
        text: &str,
        images: &[(String, String)],
    ) -> anyhow::Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "text": text,
            "images": images.iter().map(|(media_type, data)| serde_json::json!({
                "media_type": media_type,
                "data": data,
            })).collect::<Vec<_>>(),
        });
        self.rpc
            .call("session.steer", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn interrupt(&self, session_id: &str) -> anyhow::Result<()> {
        let params = serde_json::json!({"session_id": session_id});
        self.rpc
            .call("session.interrupt", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    /// Side-question (`/btw`) — streams `BtwDelta` / `BtwEnd` without touching main transcript.
    pub async fn start_btw(&self, session_id: &str, question: &str) -> anyhow::Result<String> {
        let params = serde_json::json!({
            "session_id": session_id,
            "text": question,
        });
        let result = self
            .rpc
            .call("session.btw", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(result
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn cancel_btw(&self, session_id: &str) -> anyhow::Result<()> {
        let params = serde_json::json!({"session_id": session_id});
        self.rpc
            .call("session.btw_cancel", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn delete_btw(&self, session_id: &str) -> anyhow::Result<()> {
        let params = serde_json::json!({"session_id": session_id});
        self.rpc
            .call("session.btw_delete", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn fork_session(
        &self,
        session_id: &str,
        title: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let mut params = serde_json::json!({"session_id": session_id});
        if let Some(title) = title {
            params["title"] = serde_json::json!(title);
        }
        self.rpc
            .call("sessions.fork", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    pub async fn set_permission_mode(
        &self,
        session_id: &str,
        mode: PermissionMode,
    ) -> anyhow::Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "mode": mode,
        });
        self.rpc
            .call("session.set_permission_mode", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn set_plan_mode(&self, session_id: &str, enabled: bool) -> anyhow::Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "enabled": enabled,
        });
        self.rpc
            .call("session.set_plan_mode", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn respond_approval(
        &self,
        session_id: &str,
        response: ApprovalResponse,
    ) -> anyhow::Result<()> {
        let mut params = serde_json::to_value(&response)?;
        if let Some(obj) = params.as_object_mut() {
            obj.insert("session_id".into(), serde_json::json!(session_id));
        }
        self.rpc
            .call("approval.respond", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn respond_question(
        &self,
        session_id: &str,
        response: kkagent_protocol::QuestionResponse,
    ) -> anyhow::Result<()> {
        let mut params = serde_json::to_value(&response)?;
        if let Some(obj) = params.as_object_mut() {
            obj.insert("session_id".into(), serde_json::json!(session_id));
        }
        self.rpc
            .call("question.respond", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn set_model(&self, session_id: &str, model: &str) -> anyhow::Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "model": model,
        });
        self.rpc
            .call("session.set_model", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn set_fallback_model(
        &self,
        session_id: &str,
        mode: &str,
        model: Option<&str>,
    ) -> anyhow::Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "mode": mode,
            "model": model,
        });
        self.rpc
            .call("session.set_fallback_model", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    pub async fn set_prompt_queue_json(
        &self,
        session_id: &str,
        selected: usize,
        items: Vec<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "selected": selected,
            "items": items,
        });
        self.rpc
            .call("session.set_prompt_queue", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(())
    }

    /// Whether the connected server currently has any in-flight agent turns.
    pub async fn has_active_turns(&self) -> anyhow::Result<bool> {
        let result = self
            .rpc
            .call("runtime.has_active_turns", None)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(result
            .get("active")
            .and_then(|value| value.as_bool())
            .unwrap_or(false))
    }

    pub async fn rpc_call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        self.rpc
            .call(method, params)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}
