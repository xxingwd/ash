use crate::{access::Access, *};
use ash_agent::{Agent, Profile, Runtime};
use ash_core::{
    CancellationToken, ModelClient, ModelEvent, ModelId, ModelRequest, ModelStream, ProtocolError,
    SessionId, SessionIdentity, StopReason, ToolContext, ToolOutput,
};
use futures::{stream, StreamExt};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

type Request = (ModelRequest, oneshot::Sender<Vec<ModelEvent>>);
struct Model {
    requests: mpsc::UnboundedSender<Request>,
}
impl ModelClient for Model {
    fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProtocolError> {
        let (send, receive) = oneshot::channel();
        self.requests.send((request, send)).unwrap();
        Ok(Box::pin(
            stream::once(async move { receive.await.unwrap_or_default().into_iter().map(Ok) })
                .flat_map(stream::iter),
        ))
    }
}
struct Harness {
    runtime: Runtime,
    control: AgentControl,
    root: SessionIdentity,
    requests: mpsc::UnboundedReceiver<Request>,
    _directory: tempfile::TempDir,
}
impl Harness {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (send, requests) = mpsc::unbounded_channel();
        let runtime = Runtime::new(Arc::new(Model { requests: send }))
            .with_session_directory(directory.path());
        let mut definitions = Profile::builtins()
            .unwrap()
            .into_iter()
            .map(|profile| {
                let tools = ["read", "glob", "grep", "bash", "edit", "write", "webfetch"]
                    .into_iter()
                    .map(|name| {
                        ash_core::define_tool(name, name, |_, ()| async { Ok("ok") }).unwrap()
                    })
                    .collect();
                Definition {
                    agent: Agent::new(ModelId::new("test"), tools)
                        .with_profile(profile)
                        .unwrap(),
                    capabilities: None,
                }
            })
            .collect::<Vec<_>>();
        definitions.push(Definition {
            agent: Agent::new(ModelId::new("test"), Vec::new())
                .with_profile(Profile::parse("coordinator", "---\ndescription: Test blueprint coordinator\n---\nCoordinate prebuilt work.").unwrap()).unwrap(),
            capabilities: Some(|_| Ok(Vec::new())),
        });
        Self {
            runtime: runtime.clone(),
            control: AgentControl::new(runtime, definitions).unwrap(),
            root: SessionIdentity::root(SessionId::new()),
            requests,
            _directory: directory,
        }
    }
    async fn agent(&self, profile: &str) -> SessionId {
        let result = self
            .control
            .create(
                context(self.root),
                AgentArgs {
                    profile: profile.into(),
                    prompt: None,
                },
            )
            .await
            .unwrap();
        serde_json::from_value(value(&result)["agent_id"].clone()).unwrap()
    }
    async fn group(&self, name: &str, members: &[SessionId]) -> String {
        let mut id = name.to_string();
        for member in members {
            let result = self
                .control
                .group(
                    context(self.root),
                    GroupArgs {
                        group_id: id,
                        agent_id: Some(*member),
                    },
                )
                .await
                .unwrap();
            id = value(&result)["group_id"].as_str().unwrap().into();
        }
        id
    }
    async fn next(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn send(&self, agent: SessionId, message: &str) -> Result<String, ash_core::ToolError> {
        self.control
            .message(
                context(self.root),
                MessageArgs {
                    agent_id: agent,
                    message: message.into(),
                },
            )
            .await
    }
    async fn wait(&self, args: WaitArgs) -> String {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.control.wait(context(self.root), args),
        )
        .await
        .unwrap()
        .unwrap()
    }
    async fn receive(&self, args: WaitArgs) -> Value {
        let tool = self
            .control
            .tools(Access::Root)
            .unwrap()
            .into_iter()
            .find(|tool| tool.name() == "wait")
            .unwrap();
        let arguments = serde_json::to_value(&args).unwrap();
        let output = tool
            .execute(context(self.root), arguments.clone())
            .await
            .unwrap();
        tool.committed(context(self.root), &arguments, &output)
            .await
            .unwrap();
        value(&output.text)
    }
}
fn context(identity: SessionIdentity) -> ToolContext {
    ToolContext {
        identity,
        cancellation: CancellationToken::new(),
        deadline: None,
    }
}
fn value(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}
fn agent_wait(id: SessionId) -> WaitArgs {
    WaitArgs {
        agent_id: Some(id),
        group_id: None,
    }
}
fn group_wait(id: &str) -> WaitArgs {
    WaitArgs {
        agent_id: None,
        group_id: Some(id.into()),
    }
}
fn complete(send: oneshot::Sender<Vec<ModelEvent>>, message: &str) {
    let _ = send.send(vec![
        ModelEvent::Text(message.into()),
        ModelEvent::Stop(StopReason::EndTurn),
    ]);
}

#[test]
fn completion_selects_last_text_and_preserves_execution_diagnostics() {
    use ash_core::{Input, Item, Step, ToolCall, ToolCallId, Turn, TurnId, TurnResult};
    let sender = SessionId::new();
    let mut turn = Turn {
        id: TurnId::new(),
        input: Input::user("task"),
        steps: vec![Arc::new(Step {
            items: vec![
                Item::Text("progress".into()),
                Item::ToolCall(ToolCall {
                    id: ToolCallId::new(),
                    name: "read".into(),
                    arguments: json!({}),
                    result: Ok(ToolOutput::from("private tool output")),
                }),
                Item::Text("final answer".into()),
                Item::Thought {
                    text: "private thought".into(),
                    elapsed_seconds: 0,
                },
            ],
        })],
        result: TurnResult::Stopped(StopReason::EndTurn),
        stats: Default::default(),
    };
    let result = Completion::from_turn(sender, Ok(&turn));
    assert_eq!(result.message, "final answer");
    assert_eq!(result.source, MessageSource::Agent);
    for (end, expected) in [
        (
            TurnResult::Failed("failure".into()),
            ExecutionStatus::Failed,
        ),
        (TurnResult::Cancelled, ExecutionStatus::Cancelled),
        (TurnResult::Truncated, ExecutionStatus::Truncated),
        (
            TurnResult::Stopped(StopReason::MaxTokens),
            ExecutionStatus::Truncated,
        ),
    ] {
        turn.result = end;
        let result = Completion::from_turn(sender, Ok(&turn));
        assert_eq!(result.status, expected);
        assert_eq!(result.source, MessageSource::Runtime);
        assert!(result.message.contains("final answer"));
        assert!(!result.message.contains("private"));
    }
    turn.steps.clear();
    turn.result = TurnResult::Stopped(StopReason::EndTurn);
    assert_eq!(
        Completion::from_turn(sender, Ok(&turn)).source,
        MessageSource::Runtime
    );
}

#[tokio::test]
async fn independent_groups_run_in_parallel_and_snapshots_keep_one_row_per_group() {
    let mut harness = Harness::new();
    let first = harness.agent("default").await;
    let second = harness.agent("review").await;
    let first_group = harness.group("first", &[first]).await;
    let second_group = harness.group("second", &[second]).await;
    harness.send(first, "first work").await.unwrap();
    let (_, first_response) = harness.next().await;
    harness.send(second, "second work").await.unwrap();
    let (_, second_response) = harness.next().await;
    let snapshots = harness.control.snapshots(harness.root.id()).await.unwrap();
    assert_eq!(snapshots.len(), 2);
    assert!(snapshots.iter().all(|event| event.group_id.is_some()
        && event.kind == SubagentEventKind::StateChanged(SubagentState::Running)));
    let _ = first_response.send(vec![
        ModelEvent::Text("partial".into()),
        ModelEvent::Stop(StopReason::MaxTokens),
    ]);
    let result = harness.receive(group_wait(&first_group)).await;
    assert_eq!(result["result"]["status"], "truncated");
    let listing = value(
        &harness
            .control
            .list(
                context(harness.root),
                ListArgs {
                    group_id: Some(second_group.clone()),
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(listing["state"], "running");
    complete(second_response, "second complete");
    assert_eq!(
        harness.receive(group_wait(&second_group)).await["result"]["message"],
        "second complete"
    );
    let snapshots = harness.control.snapshots(harness.root.id()).await.unwrap();
    assert_eq!(snapshots.len(), 2);
    assert!(snapshots
        .iter()
        .all(|event| event.kind == SubagentEventKind::StateChanged(SubagentState::Idle)));
}

#[tokio::test]
async fn stopped_or_replaced_coordinator_can_collect_its_running_descendant() {
    for replace in [false, true] {
        let mut harness = Harness::new();
        let coordinator = harness.agent("coordinator").await;
        let identity = SessionIdentity::try_from_parts(
            coordinator,
            harness.root.id(),
            Some(harness.root.id()),
        )
        .unwrap();
        let child = value(
            &harness
                .control
                .assemble(
                    context(identity),
                    Blueprint {
                        agents: vec![BlueprintAgent {
                            name: "child".into(),
                            profile: "default".into(),
                            parent: None,
                        }],
                        groups: Vec::new(),
                    },
                )
                .await
                .unwrap(),
        );
        let child_id: SessionId = serde_json::from_value(child["agents"]["child"].clone()).unwrap();
        harness.send(coordinator, "coordinate").await.unwrap();
        let (_, coordinator_response) = harness.next().await;
        harness
            .control
            .message(
                context(identity),
                MessageArgs {
                    agent_id: child_id,
                    message: "long task".into(),
                },
            )
            .await
            .unwrap();
        let (_, child_response) = harness.next().await;
        let resumed_response = if replace {
            harness
                .send(coordinator, "take over the existing child")
                .await
                .unwrap();
            drop(coordinator_response);
            harness.next().await.1
        } else {
            complete(coordinator_response, "delegated");
            harness.receive(agent_wait(coordinator)).await;
            harness
                .send(coordinator, "collect the existing child")
                .await
                .unwrap();
            harness.next().await.1
        };
        let listing = value(
            &harness
                .control
                .list(context(identity), ListArgs { group_id: None })
                .await
                .unwrap(),
        );
        assert_eq!(listing["agents"][0]["agent_id"], json!(child_id));
        assert_eq!(listing["agents"][0]["state"], "running");
        assert_eq!(
            harness
                .control
                .snapshots(harness.root.id())
                .await
                .unwrap()
                .len(),
            2
        );
        let _ = resumed_response.send(vec![
            ModelEvent::ToolCall {
                id: ash_core::ToolCallId::new(),
                name: "wait".into(),
                arguments: json!({"agent_id":child_id}),
            },
            ModelEvent::Stop(StopReason::EndTurn),
        ]);
        complete(child_response, "child complete");
        let (request, response) = harness.next().await;
        assert!(request
            .context
            .current()
            .unwrap()
            .1
            .iter()
            .flat_map(|step| step.tool_calls())
            .any(|call| call.name == "wait"
                && call
                    .result
                    .as_ref()
                    .is_ok_and(|output| output.text.contains("child complete"))));
        complete(response, "collected existing child");
        let (_, auto_response) = harness.next().await;
        complete(auto_response, "automatic child completion");
        assert_eq!(
            harness.receive(agent_wait(coordinator)).await["result"]["message"],
            "collected existing child"
        );
        assert!(harness.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn bounded_history_and_wait_preserve_full_chat_records() {
    let mut harness = Harness::new();
    let member = harness.agent("default").await;
    let group = harness.group("large", &[member]).await;
    let message = "\"\\\n中文".repeat(20_000);
    harness.send(member, "task").await.unwrap();
    complete(harness.next().await.1, &message);
    let result = harness.wait(group_wait(&group)).await;
    assert!(result.len() < 64 * 1024);
    assert_eq!(value(&result)["truncated"], true);
    let history = harness
        .control
        .history(
            context(harness.root),
            HistoryArgs {
                group_id: Some(group.clone()),
                before: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(history.len() < 64 * 1024);
    let entries = crate::store::history(harness._directory.path(), harness.root.id(), &group)
        .await
        .unwrap();
    assert_eq!(entries.last().unwrap().message, message);
    assert!(harness.send(member, "not received yet").await.is_err());
}

#[tokio::test]
async fn unstarted_creation_receipt_reports_success_without_ambiguous_acceptance() {
    let harness = Harness::new();
    let receipt = value(
        &harness
            .control
            .create(
                context(harness.root),
                AgentArgs {
                    profile: "review".into(),
                    prompt: None,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(receipt["created"], true);
    assert_eq!(receipt["started"], false);
    assert!(receipt.get("accepted").is_none());
    let listing = value(
        &harness
            .control
            .list(context(harness.root), ListArgs { group_id: None })
            .await
            .unwrap(),
    );
    assert_eq!(listing["agents"].as_array().unwrap().len(), 1);
    assert_eq!(listing["agents"][0]["agent_id"], receipt["agent_id"]);
}

#[tokio::test]
async fn six_tools_and_unstarted_children_do_not_inherit_parent_role() {
    let mut harness = Harness::new();
    let mut names = harness
        .control
        .tools(Access::Root)
        .unwrap()
        .iter()
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        ["agent", "group", "history", "list", "message", "wait"]
    );
    let agent = harness.agent("review").await;
    assert!(value(&harness.wait(agent_wait(agent)).await)["result"].is_null());
    assert!(harness.requests.try_recv().is_err());
    harness.send(agent, "review").await.unwrap();
    let (request, send) = harness.next().await;
    assert!(request
        .system
        .unwrap()
        .contains(Profile::builtin("review").unwrap().instructions()));
    assert!(!request
        .tools
        .iter()
        .any(|tool| matches!(tool.name.as_str(), "edit" | "write")));
    complete(send, "reviewed");
    assert_eq!(
        harness.receive(agent_wait(agent)).await["result"]["message"],
        "reviewed"
    );
}

#[tokio::test]
async fn wait_result_is_received_only_after_tool_result_commit() {
    let mut harness = Harness::new();
    let agent = harness.agent("default").await;
    harness.send(agent, "first").await.unwrap();
    complete(harness.next().await.1, "first result");
    let original = harness.wait(agent_wait(agent)).await;
    assert!(harness
        .send(agent, "second")
        .await
        .unwrap_err()
        .to_string()
        .contains("wait"));
    harness
        .control
        .list(context(harness.root), ListArgs { group_id: None })
        .await
        .unwrap();
    assert!(harness.send(agent, "second").await.is_err());
    let received = harness.receive(agent_wait(agent)).await;
    assert_eq!(received, value(&original));
    assert_eq!(harness.receive(agent_wait(agent)).await, received);
    harness.send(agent, "second").await.unwrap();
    complete(harness.next().await.1, "second result");
    assert_eq!(
        harness.receive(agent_wait(agent)).await["result"]["message"],
        "second result"
    );
}

#[tokio::test]
async fn running_replacement_keeps_wait_on_the_continuous_work_line() {
    let mut harness = Harness::new();
    let agent = harness.agent("default").await;
    harness.send(agent, "old").await.unwrap();
    let (_, old) = harness.next().await;
    let control = harness.control.clone();
    let root = harness.root;
    let waiting = tokio::spawn(async move {
        control
            .wait(context(root), agent_wait(agent))
            .await
            .unwrap()
    });
    harness.send(agent, "replacement").await.unwrap();
    let (request, replacement) = harness.next().await;
    assert!(request
        .context
        .current()
        .unwrap()
        .0
        .text()
        .contains("replacement"));
    assert!(!waiting.is_finished());
    complete(old, "late old result");
    complete(replacement, "new result");
    let result = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value(&result)["result"]["message"], "new result");
}

#[tokio::test]
async fn group_handoff_is_serial_and_wait_returns_the_last_member() {
    let mut harness = Harness::new();
    let first = harness.agent("default").await;
    let second = harness.agent("review").await;
    let group = harness.group("line", &[first, second]).await;
    harness.send(first, "implement").await.unwrap();
    let (request, send) = harness.next().await;
    assert!(!request
        .tools
        .iter()
        .any(|tool| matches!(tool.name.as_str(), "agent" | "group" | "workflow")));
    assert!(harness
        .control
        .wait(context(harness.root), agent_wait(first))
        .await
        .unwrap_err()
        .to_string()
        .contains(&group));
    let identity =
        SessionIdentity::try_from_parts(first, harness.root.id(), Some(harness.root.id())).unwrap();
    harness
        .control
        .message(
            context(identity),
            MessageArgs {
                agent_id: second,
                message: "review this".into(),
            },
        )
        .await
        .unwrap();
    assert!(harness.requests.try_recv().is_err());
    assert!(harness
        .control
        .message(
            context(identity),
            MessageArgs {
                agent_id: second,
                message: "another".into()
            }
        )
        .await
        .is_err());
    assert!(harness
        .control
        .wait(context(identity), group_wait(&group))
        .await
        .is_err());
    assert!(harness
        .control
        .create(
            context(identity),
            AgentArgs {
                profile: "default".into(),
                prompt: None
            }
        )
        .await
        .is_err());
    complete(send, "implementation ready");
    let (_, send) = harness.next().await;
    complete(send, "review complete");
    assert_eq!(
        harness.receive(group_wait(&group)).await["result"]["message"],
        "review complete"
    );
    let history = harness
        .control
        .history(
            context(identity),
            HistoryArgs {
                group_id: None,
                before: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(history.contains("implementation ready"));
    assert!(history.contains("review complete"));
}

#[tokio::test]
async fn parent_group_replacement_discards_the_old_successor() {
    let mut harness = Harness::new();
    let first = harness.agent("default").await;
    let second = harness.agent("default").await;
    let group = harness.group("line", &[first, second]).await;
    harness.send(first, "original").await.unwrap();
    let (_, old) = harness.next().await;
    let identity =
        SessionIdentity::try_from_parts(first, harness.root.id(), Some(harness.root.id())).unwrap();
    harness
        .control
        .message(
            context(identity),
            MessageArgs {
                agent_id: second,
                message: "stale handoff".into(),
            },
        )
        .await
        .unwrap();
    harness.send(first, "new direction").await.unwrap();
    let (_, new) = harness.next().await;
    complete(old, "late");
    complete(new, "new direction done");
    assert_eq!(
        harness.receive(group_wait(&group)).await["result"]["message"],
        "new direction done"
    );
    assert!(harness.requests.try_recv().is_err());
}

#[tokio::test]
async fn group_unread_gate_cannot_be_bypassed_by_changing_recipient() {
    let mut harness = Harness::new();
    let first = harness.agent("default").await;
    let second = harness.agent("default").await;
    let group = harness.group("line", &[first, second]).await;
    harness.send(first, "task").await.unwrap();
    complete(harness.next().await.1, "help needed");
    harness.wait(group_wait(&group)).await;
    assert!(harness
        .send(second, "continue")
        .await
        .unwrap_err()
        .to_string()
        .contains(&group));
    harness
        .control
        .history(
            context(harness.root),
            HistoryArgs {
                group_id: Some(group.clone()),
                before: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(harness.send(second, "continue").await.is_err());
    harness.receive(group_wait(&group)).await;
    harness.send(second, "continue").await.unwrap();
    complete(harness.next().await.1, "continued");
    harness.receive(group_wait(&group)).await;
}

#[tokio::test]
async fn cancelling_wait_does_not_cancel_the_target() {
    let mut harness = Harness::new();
    let agent = harness.agent("default").await;
    harness.send(agent, "task").await.unwrap();
    let (_, send) = harness.next().await;
    let ctx = context(harness.root);
    ctx.cancellation.cancel();
    assert!(harness.control.wait(ctx, agent_wait(agent)).await.is_err());
    complete(send, "still completed");
    assert_eq!(
        harness.receive(agent_wait(agent)).await["result"]["message"],
        "still completed"
    );
}

#[tokio::test]
async fn old_delivery_commit_cannot_clear_a_new_unread_result() {
    let mut harness = Harness::new();
    let agent = harness.agent("default").await;
    harness.send(agent, "first").await.unwrap();
    complete(harness.next().await.1, "first");
    let old = harness.receive(agent_wait(agent)).await;
    harness.send(agent, "second").await.unwrap();
    complete(harness.next().await.1, "second");
    harness.wait(agent_wait(agent)).await;
    let tool = harness
        .control
        .tools(Access::Root)
        .unwrap()
        .into_iter()
        .find(|tool| tool.name() == "wait")
        .unwrap();
    tool.committed(
        context(harness.root),
        &serde_json::to_value(agent_wait(agent)).unwrap(),
        &ToolOutput::from(old.to_string()),
    )
    .await
    .unwrap();
    assert!(harness.send(agent, "third").await.is_err());
    harness.receive(agent_wait(agent)).await;
}

#[tokio::test]
async fn empty_groups_are_idempotent_and_owner_scoped() {
    let harness = Harness::new();
    let first = harness
        .control
        .group(
            context(harness.root),
            GroupArgs {
                group_id: "../same/name".into(),
                agent_id: None,
            },
        )
        .await
        .unwrap();
    let second = harness
        .control
        .group(
            context(harness.root),
            GroupArgs {
                group_id: "../same/name".into(),
                agent_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(first, second);
    let group = value(&first)["group_id"].as_str().unwrap().to_string();
    assert!(value(&harness.wait(group_wait(&group)).await)["result"].is_null());
    assert!(!value(&first)["prompt_path"]
        .as_str()
        .unwrap()
        .contains("../"));
    let other = SessionIdentity::root(SessionId::new());
    assert!(harness
        .control
        .history(
            context(other),
            HistoryArgs {
                group_id: Some(group),
                before: None,
                limit: None
            }
        )
        .await
        .is_err());
}

#[tokio::test]
async fn group_prompt_failure_stops_with_a_runtime_diagnostic() {
    let mut harness = Harness::new();
    let agent = harness.agent("default").await;
    let group = harness.group("line", &[agent]).await;
    let listing = harness
        .control
        .list(
            context(harness.root),
            ListArgs {
                group_id: Some(group.clone()),
            },
        )
        .await
        .unwrap();
    tokio::fs::remove_file(value(&listing)["prompt_path"].as_str().unwrap())
        .await
        .unwrap();
    harness.send(agent, "task").await.unwrap();
    let result = harness.receive(group_wait(&group)).await;
    assert_eq!(result["result"]["status"], "failed");
    assert_eq!(result["result"]["source"], "runtime");
    assert!(harness.requests.try_recv().is_err());
}

#[tokio::test]
async fn history_uses_exclusive_anchors_and_does_not_repeat_pages() {
    let mut harness = Harness::new();
    let agent = harness.agent("default").await;
    let group = harness.group("line", &[agent]).await;
    for index in 0..6 {
        harness.send(agent, &format!("task {index}")).await.unwrap();
        complete(harness.next().await.1, &format!("result {index}"));
        harness.receive(group_wait(&group)).await;
    }
    let page = value(
        &harness
            .control
            .history(
                context(harness.root),
                HistoryArgs {
                    group_id: Some(group.clone()),
                    before: None,
                    limit: None,
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(page["messages"].as_array().unwrap().len(), 10);
    let anchor = page["next_before"].as_str().unwrap().to_string();
    harness.send(agent, "new tail").await.unwrap();
    complete(harness.next().await.1, "new tail result");
    harness.receive(group_wait(&group)).await;
    let older = value(
        &harness
            .control
            .history(
                context(harness.root),
                HistoryArgs {
                    group_id: Some(group.clone()),
                    before: Some(anchor),
                    limit: None,
                },
            )
            .await
            .unwrap(),
    );
    for entry in older["messages"].as_array().unwrap() {
        assert!(!page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|other| other["message_id"] == entry["message_id"]));
    }
    assert!(harness
        .control
        .history(
            context(harness.root),
            HistoryArgs {
                group_id: Some(group),
                before: Some("unknown".into()),
                limit: None
            }
        )
        .await
        .is_err());
}

#[test]
fn optional_agent_ids_are_inline_nullable_strings_in_tool_schemas() {
    for schema in [
        serde_json::to_value(schemars::schema_for!(WaitArgs)).unwrap(),
        serde_json::to_value(schemars::schema_for!(GroupArgs)).unwrap(),
    ] {
        assert_eq!(
            schema["properties"]["agent_id"]["type"],
            json!(["string", "null"]),
            "{schema}"
        );
        assert!(schema.get("$defs").is_none());
    }
    let message = serde_json::to_value(schemars::schema_for!(MessageArgs)).unwrap();
    assert_eq!(message["properties"]["agent_id"]["type"], "string");
    assert!(message.get("$defs").is_none());
}

#[tokio::test]
async fn ordinary_children_have_no_collaboration_tools_or_creation_authority() {
    let mut harness = Harness::new();
    for profile in ["default", "review", "explore"] {
        let agent = harness.agent(profile).await;
        let identity =
            SessionIdentity::try_from_parts(agent, harness.root.id(), Some(harness.root.id()))
                .unwrap();
        harness
            .send(agent, "complete a bounded task")
            .await
            .unwrap();
        let (request, response) = harness.next().await;
        assert!(!request
            .tools
            .iter()
            .any(|tool| crate::access::COLLAB_TOOLS.contains(&tool.name.as_str())));
        let system = request.system.unwrap();
        assert!(!system.contains("Available agent profiles"));
        assert!(!system.contains("# Coordinate existing work"));
        assert!(!system.contains("# Work with group members"));
        assert!(harness
            .control
            .create(
                context(identity),
                AgentArgs {
                    profile: "default".into(),
                    prompt: None
                }
            )
            .await
            .is_err());
        assert!(harness
            .control
            .group(
                context(identity),
                GroupArgs {
                    group_id: "forbidden".into(),
                    agent_id: None
                }
            )
            .await
            .is_err());
        assert!(harness
            .control
            .assemble(
                context(identity),
                Blueprint {
                    agents: Vec::new(),
                    groups: Vec::new()
                }
            )
            .await
            .is_err());
        complete(response, "done");
        harness.receive(agent_wait(agent)).await;
    }
}

#[test]
fn collaboration_tools_follow_organizational_access() {
    let cases: &[(Access, &[&str])] = &[
        (
            Access::Worker {
                member: false,
                children: false,
                groups: false,
            },
            &[],
        ),
        (
            Access::Worker {
                member: true,
                children: false,
                groups: false,
            },
            &["history", "list", "message"],
        ),
        (
            Access::Worker {
                member: false,
                children: true,
                groups: false,
            },
            &["list", "message", "wait"],
        ),
        (
            Access::Worker {
                member: true,
                children: true,
                groups: false,
            },
            &["history", "list", "message", "wait"],
        ),
        (
            Access::Worker {
                member: false,
                children: true,
                groups: true,
            },
            &["history", "list", "message", "wait"],
        ),
        (Access::Manager, &["history", "list", "message", "wait"]),
    ];
    let harness = Harness::new();
    for (access, expected) in cases {
        let mut names = harness
            .control
            .tools(*access)
            .unwrap()
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, *expected, "{access:?}");
    }
}

#[tokio::test]
async fn pending_reports_running_and_unread_without_receiving_or_cancelling() {
    let mut harness = Harness::new();
    let child = harness.agent("default").await;
    let member = harness.agent("review").await;
    let group = harness.group("line", &[member]).await;
    let _unstarted = harness.agent("default").await;
    harness.send(child, "work").await.unwrap();
    let (_, child_response) = harness.next().await;
    harness.send(member, "review").await.unwrap();
    let (member_request, member_response) = harness.next().await;
    assert!(!member_request.tools.iter().any(|tool| tool.name == "wait"));
    assert!(member_request
        .system
        .as_ref()
        .unwrap()
        .contains("# Work with group members"));
    assert!(!member_request
        .system
        .as_ref()
        .unwrap()
        .contains("# Coordinate existing work"));
    let pending = harness.control.pending(harness.root).await.unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending
        .iter()
        .all(|work| work.state == PendingState::Running));
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        harness
            .control
            .wait(context(harness.root), group_wait(&group))
    )
    .await
    .is_err());
    assert_eq!(
        pending,
        harness.control.pending(harness.root).await.unwrap()
    );
    complete(child_response, "child result");
    complete(member_response, "group result");
    harness.wait(agent_wait(child)).await;
    harness.wait(group_wait(&group)).await;
    assert!(harness
        .control
        .pending(harness.root)
        .await
        .unwrap()
        .iter()
        .all(|work| work.state == PendingState::Unread));
    harness.receive(agent_wait(child)).await;
    harness.receive(group_wait(&group)).await;
    assert!(harness
        .control
        .pending(harness.root)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn stopped_group_reports_running_prebuilt_descendants_without_waiting_for_them() {
    let mut harness = Harness::new();
    let coordinator = harness.agent("coordinator").await;
    let owner =
        SessionIdentity::try_from_parts(coordinator, harness.root.id(), Some(harness.root.id()))
            .unwrap();
    let assembly = value(
        &harness
            .control
            .assemble(
                context(owner),
                serde_json::from_value(json!({
                    "agents":[{"name":"worker"},{"name":"helper","parent":"worker"}],
                    "groups":[{"name":"line","members":["worker"]}]
                }))
                .unwrap(),
            )
            .await
            .unwrap(),
    );
    let worker: SessionId = serde_json::from_value(assembly["agents"]["worker"].clone()).unwrap();
    let helper: SessionId = serde_json::from_value(assembly["agents"]["helper"].clone()).unwrap();
    let group = assembly["groups"]["line"]["group_id"].as_str().unwrap();
    let worker_identity =
        SessionIdentity::try_from_parts(worker, harness.root.id(), Some(coordinator)).unwrap();
    harness
        .control
        .message(
            context(owner),
            MessageArgs {
                agent_id: worker,
                message: "coordinate helper".into(),
            },
        )
        .await
        .unwrap();
    let (request, worker_response) = harness.next().await;
    assert!(request.tools.iter().any(|tool| tool.name == "wait"));
    assert!(!request
        .tools
        .iter()
        .any(|tool| matches!(tool.name.as_str(), "agent" | "group" | "workflow")));
    harness
        .control
        .message(
            context(worker_identity),
            MessageArgs {
                agent_id: helper,
                message: "long work".into(),
            },
        )
        .await
        .unwrap();
    let (request, helper_response) = harness.next().await;
    assert!(!request
        .tools
        .iter()
        .any(|tool| crate::access::COLLAB_TOOLS.contains(&tool.name.as_str())));
    complete(worker_response, "need parent help");
    let result = value(
        &tokio::time::timeout(
            Duration::from_secs(2),
            harness.control.wait(context(owner), group_wait(group)),
        )
        .await
        .unwrap()
        .unwrap(),
    );
    assert_eq!(result["result"]["message"], "need parent help");
    assert_eq!(
        result["pending"],
        json!([{"owner_id":worker,"kind":"agent","agent_id":helper,"state":"running"}])
    );
    assert!(
        pending_notice(&harness.control.pending(worker_identity).await.unwrap())
            .unwrap()
            .contains(&helper.to_string())
    );
    complete(helper_response, "helper completed");
    harness
        .control
        .wait(context(worker_identity), agent_wait(helper))
        .await
        .unwrap();
    let result = value(
        &harness
            .control
            .wait(context(owner), group_wait(group))
            .await
            .unwrap(),
    );
    assert_eq!(result["pending"][0]["state"], "unread");
}

#[test]
fn agent_schema_rejects_old_names_and_invalid_profiles() {
    assert!(serde_json::from_value::<AgentArgs>(json!({"name":"old","message":"old"})).is_err());
    assert!(serde_json::from_value::<AgentArgs>(json!({"profile":null})).is_err());
    assert_eq!(
        serde_json::from_value::<AgentArgs>(json!({}))
            .unwrap()
            .profile,
        "default"
    );
    let schema = serde_json::to_value(schemars::schema_for!(MessageArgs)).unwrap();
    assert!(schema["properties"].get("agent_id").is_some());
    assert!(schema["properties"].get("name").is_none());
}

#[tokio::test]
async fn concurrent_successors_and_replacements_have_one_winner() {
    let mut harness = Harness::new();
    let first = harness.agent("default").await;
    let second = harness.agent("default").await;
    let third = harness.agent("default").await;
    let group = harness.group("line", &[first, second, third]).await;
    harness.send(first, "start").await.unwrap();
    let (_, old) = harness.next().await;
    let identity =
        SessionIdentity::try_from_parts(first, harness.root.id(), Some(harness.root.id())).unwrap();
    let (left, right) = tokio::join!(
        harness.control.message(
            context(identity),
            MessageArgs {
                agent_id: second,
                message: "left".into()
            }
        ),
        harness.control.message(
            context(identity),
            MessageArgs {
                agent_id: third,
                message: "right".into()
            }
        )
    );
    assert_ne!(left.is_ok(), right.is_ok());
    let (left, right) = tokio::join!(
        harness.send(first, "replace left"),
        harness.send(second, "replace right")
    );
    assert_ne!(left.is_ok(), right.is_ok());
    complete(old, "old");
    let (_, replacement) = harness.next().await;
    complete(replacement, "replacement done");
    assert_eq!(
        harness.receive(group_wait(&group)).await["result"]["message"],
        "replacement done"
    );
    assert!(harness.requests.try_recv().is_err());
}

#[tokio::test]
async fn failed_replacement_commit_keeps_the_original_execution() {
    let mut harness = Harness::new();
    let agent = harness.agent("default").await;
    harness.send(agent, "original").await.unwrap();
    let (_, original) = harness.next().await;
    let temporary = harness
        ._directory
        .path()
        .join("collab")
        .join(harness.root.id().to_string())
        .join("state.tmp");
    tokio::fs::create_dir(&temporary).await.unwrap();
    assert!(harness.send(agent, "not accepted").await.is_err());
    tokio::fs::remove_dir(&temporary).await.unwrap();
    complete(original, "original completed");
    assert_eq!(
        harness.receive(agent_wait(agent)).await["result"]["message"],
        "original completed"
    );
    assert!(harness.requests.try_recv().is_err());
}

fn copy_directory(source: &std::path::Path, destination: &std::path::Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test]
async fn recovery_reports_interrupted_work_without_replaying_it() {
    let mut original = Harness::new();
    let agent = original.agent("default").await;
    let group = original.group("line", &[agent]).await;
    original
        .send(agent, "task with possible side effects")
        .await
        .unwrap();
    let (_, pending) = original.next().await;
    let mut recovered = Harness::new();
    recovered.root = original.root;
    copy_directory(original._directory.path(), recovered._directory.path());
    let path = recovered
        ._directory
        .path()
        .join("collab")
        .join(recovered.root.id().to_string())
        .join("groups")
        .join(&group)
        .join("chat.jsonl");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    recovered.control.restore(recovered.root).await.unwrap();
    let result = recovered.receive(group_wait(&group)).await;
    assert_eq!(result["result"]["status"], "interrupted");
    assert_eq!(result["result"]["source"], "runtime");
    assert!(recovered.requests.try_recv().is_err());
    let history = recovered
        .control
        .history(
            context(recovered.root),
            HistoryArgs {
                group_id: Some(group.clone()),
                before: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(history.contains("interrupted"));
    assert!(!history.contains("partial"));
    recovered
        .send(agent, "inspect before continuing")
        .await
        .unwrap();
    complete(recovered.next().await.1, "checked");
    recovered.receive(group_wait(&group)).await;
    original.control.close(original.root.id()).await;
    complete(pending, "late");
}

#[tokio::test]
async fn wait_and_message_in_one_batch_do_not_bypass_durable_receipt_gate() {
    let mut harness = Harness::new();
    let agent = harness.agent("default").await;
    harness.send(agent, "child task").await.unwrap();
    complete(harness.next().await.1, "child result");
    harness.wait(agent_wait(agent)).await;
    let parent = Agent::new(
        ModelId::new("test"),
        harness.control.tools(Access::Root).unwrap(),
    );
    let session = harness.runtime.start_at(&parent, harness.root);
    let turn = session.submit("receive the child").await.unwrap();
    let (_, reply) = harness.next().await;
    reply
        .send(vec![
            ModelEvent::ToolCall {
                id: ash_core::ToolCallId::new(),
                name: "wait".into(),
                arguments: serde_json::to_value(agent_wait(agent)).unwrap(),
            },
            ModelEvent::ToolCall {
                id: ash_core::ToolCallId::new(),
                name: "message".into(),
                arguments: json!({"agent_id":agent,"message":"too early"}),
            },
            ModelEvent::Stop(StopReason::EndTurn),
        ])
        .unwrap();
    let (request, reply) = harness.next().await;
    let calls = request
        .context
        .current()
        .unwrap()
        .1
        .iter()
        .flat_map(|step| step.tool_calls())
        .collect::<Vec<_>>();
    assert!(calls[0].result.is_ok());
    assert!(calls[1].result.is_err());
    complete(reply, "received");
    turn.wait().await.unwrap();
    let listing = value(
        &harness
            .control
            .list(context(harness.root), ListArgs { group_id: None })
            .await
            .unwrap(),
    );
    assert_eq!(listing["agents"][0]["unread"], false);
    let mut recovered = Harness::new();
    recovered.root = harness.root;
    copy_directory(harness._directory.path(), recovered._directory.path());
    recovered.control.restore(recovered.root).await.unwrap();
    let listing = value(
        &recovered
            .control
            .list(context(recovered.root), ListArgs { group_id: None })
            .await
            .unwrap(),
    );
    assert_eq!(listing["agents"][0]["unread"], false);
    assert!(recovered.requests.try_recv().is_err());
}

#[tokio::test]
async fn missing_completed_child_history_is_not_silently_recreated() {
    let mut original = Harness::new();
    let agent = original.agent("default").await;
    original.send(agent, "task").await.unwrap();
    complete(original.next().await.1, "completed");
    original.receive(agent_wait(agent)).await;
    let mut recovered = Harness::new();
    recovered.root = original.root;
    copy_directory(original._directory.path(), recovered._directory.path());
    std::fs::remove_file(recovered._directory.path().join(format!("{agent}.jsonl"))).unwrap();
    assert!(recovered
        .control
        .restore(recovered.root)
        .await
        .unwrap_err()
        .to_string()
        .contains("session record is missing"));
    assert!(recovered.requests.try_recv().is_err());
}
