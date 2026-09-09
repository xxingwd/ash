use std::str::FromStr;

use rquickjs::{
    prelude::Async, AsyncContext, AsyncRuntime, CatchResultExt, Function, Promise, Value,
};
use serde::Serialize;
use thiserror::Error;

use crate::{AgentResult, Workflow, WorkflowError};
use ash_core::SessionId;

#[derive(Debug, Error)]
pub enum ScriptError {
    #[error("workflow was cancelled")]
    Cancelled,
    #[error("javascript runtime: {0}")]
    JavaScript(String),
    #[error("workflow: {0}")]
    Workflow(#[from] WorkflowError),
}

#[derive(Serialize)]
struct AgentValue {
    id: String,
    output: String,
}

pub async fn run(workflow: Workflow, script: String) -> Result<serde_json::Value, ScriptError> {
    let executor = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || executor.block_on(evaluate(workflow, script)))
        .await
        .map_err(|error| ScriptError::JavaScript(format!("script task failed: {error}")))?
}

async fn evaluate(workflow: Workflow, script: String) -> Result<serde_json::Value, ScriptError> {
    let cancellation = workflow.cancellation.clone();
    let runtime =
        AsyncRuntime::new().map_err(|error| ScriptError::JavaScript(error.to_string()))?;
    let interrupt = cancellation.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || interrupt.is_cancelled())))
        .await;
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|error| ScriptError::JavaScript(error.to_string()))?;
    let execution = context.async_with(async move |context| {
        let globals = context.globals();
        globals
            .set("root_id", workflow.root_id().to_string())
            .map_err(|error| ScriptError::JavaScript(error.to_string()))?;

        let callback = {
            let workflow = workflow.clone();
            move |prompt: String, parent_id: Option<String>| {
                let workflow = workflow.clone();
                async move {
                    let parent = parent_id
                        .as_deref()
                        .map(SessionId::from_str)
                        .transpose()
                        .map_err(js_error)?;
                    let label = prompt.lines().next().unwrap_or("agent").to_string();
                    let handle = workflow
                        .spawn_agent(parent, label, prompt)
                        .await
                        .map_err(js_error)?;
                    let id = handle.id().to_string();
                    let output = match handle.wait().await {
                        AgentResult::Completed(turn) => Ok(turn.visible_text().unwrap_or_default()),
                        AgentResult::Cancelled => Err(js_error("agent cancelled")),
                        AgentResult::Failed(error) => Err(js_error(error)),
                    }?;
                    serde_json::to_string(&AgentValue { id, output }).map_err(js_error)
                }
            }
        };
        globals
            .set(
                "__agent",
                Function::new(context.clone(), Async(callback))
                    .map_err(|error| ScriptError::JavaScript(error.to_string()))?,
            )
            .map_err(|error| ScriptError::JavaScript(error.to_string()))?;
        context
            .eval::<(), _>(
                "globalThis.agent = (prompt, parent) => __agent(prompt, parent).then(JSON.parse);",
            )
            .map_err(|error| ScriptError::JavaScript(error.to_string()))?;

        let source = format!("(async () => {{\n{script}\n}})()", script = script);
        let promise: Promise = context
            .eval(source)
            .map_err(|error| ScriptError::JavaScript(error.to_string()))?;
        let value: Value = promise
            .into_future()
            .await
            .catch(&context)
            .map_err(|error| ScriptError::JavaScript(error.to_string()))?;
        rquickjs_serde::from_value_strict(value)
            .map_err(|error| ScriptError::JavaScript(error.to_string()))
    });
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ScriptError::Cancelled),
        result = execution => result,
    };
    if result.is_ok() {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ScriptError::Cancelled),
            () = runtime.idle() => {}
        }
    }
    if cancellation.is_cancelled() {
        Err(ScriptError::Cancelled)
    } else {
        result
    }
}

fn js_error(error: impl std::fmt::Display) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("workflow", "operation", error.to_string())
}
