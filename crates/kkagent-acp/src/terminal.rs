use kkagent_kaos::KaosHandle;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::AcpSessionStore;

type TerminalJoinOutput = (i32, String, String, Option<PathBuf>);

pub struct TerminalSlot {
    pub info: Value,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub join: Option<tokio::task::JoinHandle<TerminalJoinOutput>>,
}

fn session_id(params: &Value) -> String {
    params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub async fn create(
    store: &AcpSessionStore,
    kaos: &KaosHandle,
    params: &Value,
) -> Result<Value, String> {
    let cmd = params
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("echo kkagent-acp-terminal")
        .to_string();
    let sid = session_id(params);
    let cwd = if let Some(explicit) = params.get("cwd").and_then(|v| v.as_str()) {
        Some(PathBuf::from(explicit))
    } else {
        store.session_cwd(&sid).await
    };
    let tid = uuid::Uuid::new_v4().to_string();
    let kaos = kaos.clone();
    let cmd_for_exec = cmd.clone();
    let join = tokio::spawn(async move {
        match kaos.exec(&cmd_for_exec, cwd.as_deref()).await {
            Ok(result) => (result.status, result.stdout, result.stderr, result.cwd),
            Err(e) => (-1, String::new(), e.to_string(), None),
        }
    });
    let info = json!({
        "terminalId": tid,
        "command": cmd,
        "status": "running",
        "sessionId": sid,
    });
    store.terminals.lock().await.insert(
        tid.clone(),
        TerminalSlot {
            info: info.clone(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            join: Some(join),
        },
    );
    Ok(info)
}

async fn settle(slot: &mut TerminalSlot) {
    if slot.exit_code.is_some() {
        return;
    }
    if let Some(join) = slot.join.take() {
        if join.is_finished() {
            if let Ok((code, stdout, stderr, _cwd)) = join.await {
                slot.exit_code = Some(code);
                slot.stdout = stdout;
                slot.stderr = stderr;
                slot.info["status"] = json!("exited");
                slot.info["exitCode"] = json!(code);
            }
        } else {
            // Still running — put the handle back.
            slot.join = Some(join);
        }
    }
}

pub async fn output(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let tid = params
        .get("terminalId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut map = store.terminals.lock().await;
    let slot = map
        .get_mut(tid)
        .ok_or_else(|| "terminal not found".to_string())?;
    settle(slot).await;
    Ok(json!({
        "terminalId": tid,
        "stdout": slot.stdout,
        "stderr": slot.stderr,
        "exitCode": slot.exit_code,
        "info": slot.info,
    }))
}

pub async fn wait_for_exit(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let tid = params
        .get("terminalId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let join = {
        let mut map = store.terminals.lock().await;
        let slot = map
            .get_mut(&tid)
            .ok_or_else(|| "terminal not found".to_string())?;
        slot.join.take()
    };
    let (code, stdout, stderr) = if let Some(join) = join {
        match join.await {
            Ok((code, stdout, stderr, _)) => (code, stdout, stderr),
            Err(e) => (-1, String::new(), e.to_string()),
        }
    } else {
        let map = store.terminals.lock().await;
        let slot = map
            .get(&tid)
            .ok_or_else(|| "terminal not found".to_string())?;
        (
            slot.exit_code.unwrap_or(-1),
            slot.stdout.clone(),
            slot.stderr.clone(),
        )
    };
    let mut map = store.terminals.lock().await;
    if let Some(slot) = map.get_mut(&tid) {
        slot.exit_code = Some(code);
        slot.stdout = stdout.clone();
        slot.stderr = stderr.clone();
        slot.info["status"] = json!("exited");
        slot.info["exitCode"] = json!(code);
    }
    Ok(json!({
        "terminalId": tid,
        "exitCode": code,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

pub async fn kill(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let tid = params
        .get("terminalId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut map = store.terminals.lock().await;
    if let Some(mut slot) = map.remove(tid) {
        if let Some(join) = slot.join.take() {
            join.abort();
        }
        Ok(json!({"ok": true}))
    } else {
        Err("terminal not found".into())
    }
}
