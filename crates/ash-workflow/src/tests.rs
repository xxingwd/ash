use ash_agent::{Agent, Profile, Runtime};
use ash_collab::{
    AgentArgs, AgentControl, Blueprint, Definition, GroupArgs, ListArgs, MessageArgs, WaitArgs,
};
use ash_core::{
    CancellationToken, ModelClient, ModelEvent, ModelId, ModelRequest, ModelStream, ProtocolError,
    SessionId, SessionIdentity, StopReason, ToolCallId, ToolContext,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Model {
    requests: Mutex<Vec<ModelRequest>>,
    responses: Mutex<VecDeque<Vec<ModelEvent>>>,
}
impl ModelClient for Model {
    fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProtocolError> {
        self.requests.lock().unwrap().push(request);
        let events = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                vec![
                    ModelEvent::Text("done".into()),
                    ModelEvent::Stop(StopReason::EndTurn),
                ]
            });
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}
fn context(identity: SessionIdentity) -> ToolContext {
    ToolContext {
        identity,
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}
fn setup(
    responses: Vec<Vec<ModelEvent>>,
) -> (tempfile::TempDir, AgentControl, Arc<Model>, SessionIdentity) {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(Model {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(responses.into()),
    });
    let runtime = Runtime::new(model.clone()).with_session_directory(directory.path());
    let tools = ["read", "glob", "grep", "bash", "edit", "write", "webfetch"]
        .into_iter()
        .map(|name| ash_core::define_tool(name, name, |_, ()| async { Ok("ok") }).unwrap())
        .collect();
    let base = Agent::new(ModelId::new("test"), tools);
    let mut definitions = Profile::builtins()
        .unwrap()
        .into_iter()
        .map(|profile| Definition {
            agent: base.clone().with_profile(profile).unwrap(),
            capabilities: None,
        })
        .collect::<Vec<_>>();
    definitions.push(super::definition(base, &[]).unwrap());
    (
        directory,
        AgentControl::new(runtime, definitions).unwrap(),
        model,
        SessionIdentity::root(SessionId::new()),
    )
}
async fn manager(
    control: &AgentControl,
    root: SessionIdentity,
    prompt: Option<String>,
) -> SessionIdentity {
    let result: Value = serde_json::from_str(
        &control
            .create(
                context(root),
                AgentArgs {
                    profile: "workflow".into(),
                    prompt,
                },
            )
            .await
            .unwrap(),
    )
    .unwrap();
    SessionIdentity::try_from_parts(
        serde_json::from_value(result["agent_id"].clone()).unwrap(),
        root.id(),
        Some(root.id()),
    )
    .unwrap()
}
fn blueprint(value: Value) -> Blueprint {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn skill_loading_does_not_change_manager_tools_or_system_prompt() {
    let call = |name: &str, args: Value| {
        vec![
            ModelEvent::ToolCall {
                id: ToolCallId::new(),
                name: name.into(),
                arguments: args,
            },
            ModelEvent::Stop(StopReason::EndTurn),
        ]
    };
    let (_directory, control, model, root) = setup(vec![
        call("skill", json!({"name":"workflow"})),
        call(
            "workflow",
            json!({"blueprint":{"agents":[{"name":"explorer","profile":"explore"}]}}),
        ),
    ]);
    let manager = manager(
        &control,
        root,
        Some("inspect this task and build one research helper".into()),
    )
    .await;
    control
        .wait(
            context(root),
            WaitArgs {
                agent_id: Some(manager.id()),
                group_id: None,
            },
        )
        .await
        .unwrap();
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].tools.iter().any(|tool| tool.name == "workflow"));
    assert!(!requests[0]
        .tools
        .iter()
        .any(|tool| matches!(tool.name.as_str(), "agent" | "group")));
    assert!(requests[0]
        .system
        .as_ref()
        .unwrap()
        .contains("Available agent profiles"));
    assert!(requests[0].system.as_ref().unwrap().contains("- review:"));
    assert!(requests[0]
        .system
        .as_ref()
        .unwrap()
        .contains("workflow manager"));
    assert!(!requests[0]
        .system
        .as_ref()
        .unwrap()
        .contains("Example arguments:"));
    assert_eq!(requests[0].system, requests[1].system);
    assert_eq!(
        serde_json::to_value(&requests[0].tools).unwrap(),
        serde_json::to_value(&requests[1].tools).unwrap()
    );
    assert!(requests[1]
        .context
        .current()
        .unwrap()
        .1
        .iter()
        .flat_map(|step| step.tool_calls())
        .any(|call| call
            .result
            .as_ref()
            .is_ok_and(|output| output.text.contains("Example arguments:"))));
    drop(requests);
    let listing: Value = serde_json::from_str(
        &control
            .list(context(manager), ListArgs { group_id: None })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listing["agents"].as_array().unwrap().len(), 1);
    assert_eq!(listing["agents"][0]["started"], false);
    let parent = control
        .install_root(Agent::new(ModelId::new("test"), Vec::new()))
        .unwrap();
    assert!(!parent.tools().iter().any(|tool| tool.name() == "workflow"));
    assert!(parent.system_prompt().unwrap().contains("- workflow:"));
}

#[tokio::test]
async fn blueprint_builds_the_entire_parent_tree_without_running_members() {
    let (_directory, control, model, root) = setup(Vec::new());
    let manager = manager(&control, root, None).await;
    let output:Value=serde_json::from_str(&control.assemble(context(manager),blueprint(json!({"agents":[{"name":"worker"},{"name":"reviewer","profile":"review"},{"name":"helper","parent":"worker","profile":"explore"}],"groups":[{"name":"line","members":["worker","reviewer"],"prompt":"shared background"}]}))).await.unwrap()).unwrap();
    assert!(model.requests.lock().unwrap().is_empty());
    let worker: SessionId = serde_json::from_value(output["agents"]["worker"].clone()).unwrap();
    let helper: SessionId = serde_json::from_value(output["agents"]["helper"].clone()).unwrap();
    let worker_identity =
        SessionIdentity::try_from_parts(worker, root.id(), Some(manager.id())).unwrap();
    let listing: Value = serde_json::from_str(
        &control
            .list(context(worker_identity), ListArgs { group_id: None })
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listing["agents"][0]["agent_id"], json!(helper));
    assert_eq!(listing["agents"][0]["parent_id"], json!(worker));
    assert!(control
        .create(
            context(worker_identity),
            AgentArgs {
                profile: "default".into(),
                prompt: None
            }
        )
        .await
        .is_err());
    control
        .message(
            context(worker_identity),
            MessageArgs {
                agent_id: helper,
                message: "inspect".into(),
            },
        )
        .await
        .unwrap();
    control
        .wait(
            context(worker_identity),
            WaitArgs {
                agent_id: Some(helper),
                group_id: None,
            },
        )
        .await
        .unwrap();
    assert!(control
        .message(
            context(root),
            MessageArgs {
                agent_id: worker,
                message: "bypass manager".into()
            }
        )
        .await
        .is_err());
    assert_eq!(
        std::fs::read_to_string(output["groups"]["line"]["prompt_path"].as_str().unwrap()).unwrap(),
        "shared background"
    );
}

#[tokio::test]
async fn invalid_blueprints_publish_no_partial_organization() {
    let (_directory, control, model, root) = setup(Vec::new());
    let manager = manager(&control, root, None).await;
    let cases = [
        json!({"agents":[{"name":"a","parent":"b"},{"name":"b","parent":"a"}]}),
        json!({"agents":[{"name":"a"},{"name":"a"}]}),
        json!({"agents":[{"name":"a","profile":"unknown"}]}),
        json!({"agents":[{"name":"a"}],"groups":[{"name":"line","members":["unknown"]}]}),
        json!({"agents":[{"name":"a"},{"name":"b","parent":"a"}],"groups":[{"name":"line","members":["a","b"]}]}),
        json!({"agents":[{"name":"a"}],"groups":[{"name":"first","members":["a"]},{"name":"second","members":["a"]}]}),
    ];
    for case in cases {
        assert!(control
            .assemble(context(manager), blueprint(case))
            .await
            .is_err());
    }
    assert!(model.requests.lock().unwrap().is_empty());
    let listing: Value = serde_json::from_str(
        &control
            .list(context(manager), ListArgs { group_id: None })
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(listing["agents"].as_array().unwrap().is_empty());
    assert!(listing["groups"].as_array().unwrap().is_empty());
    assert!(control
        .assemble(context(root), blueprint(json!({"agents":[]})))
        .await
        .is_err());
    assert!(control
        .group(
            context(manager),
            GroupArgs {
                group_id: "owned".into(),
                agent_id: None
            }
        )
        .await
        .is_err());
    assert!(control
        .create(
            context(manager),
            AgentArgs {
                profile: "default".into(),
                prompt: None
            }
        )
        .await
        .is_err());
    control
        .assemble(
            context(manager),
            blueprint(json!({"agents":[],"groups":[{"name":"owned","members":[]}]})),
        )
        .await
        .unwrap();
    assert!(control
        .assemble(
            context(manager),
            blueprint(json!({"agents":[],"groups":[{"name":"owned","members":[]}]}))
        )
        .await
        .is_err());
}
