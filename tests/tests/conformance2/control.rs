use super::*;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::*,
    service::RequestContext,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, tower::StreamableHttpService,
    },
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
pub struct SpawnObservation {
    pub agent_id: protocol::AgentId,
    pub parent_agent_id: protocol::AgentId,
    pub name: String,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub origin: protocol::AgentOrigin,
}

pub struct Child {
    pub launch: SpawnObservation,
    pub events: Vec<ChatEvent>,
    pub requests: Vec<ModelRequestTokenUsage>,
    pub idle: bool,
    pub failure: Option<String>,
    commands: mpsc::UnboundedSender<ChildCommand>,
}

enum ChildCommand {
    Send(String, oneshot::Sender<Result<(), String>>),
    Shutdown(oneshot::Sender<()>),
}

pub struct ControlService<B: Backend> {
    pub root_id: protocol::AgentId,
    pub children: Mutex<Vec<Child>>,
    config: BackendSpawnConfig,
    base_url: String,
    changed: watch::Sender<u64>,
    backend_type: std::marker::PhantomData<B>,
}

struct Endpoint<B: Backend> {
    service: Arc<ControlService<B>>,
    await_only: bool,
}

impl<B: Backend> ControlService<B> {
    pub async fn start(config: BackendSpawnConfig) -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind agent-control fixture");
        let (changed, _) = watch::channel(0);
        let service = Arc::new(Self {
            root_id: protocol::AgentId(uuid::Uuid::new_v4().to_string()),
            children: Mutex::new(Vec::new()),
            config,
            base_url: format!("http://{}", listener.local_addr().expect("fixture address")),
            changed,
            backend_type: std::marker::PhantomData,
        });
        let endpoint = |await_only| {
            let service = service.clone();
            StreamableHttpService::new(
                move || {
                    Ok(Endpoint {
                        service: service.clone(),
                        await_only,
                    })
                },
                Arc::new(LocalSessionManager::default()),
                Default::default(),
            )
        };
        let router = axum::Router::new()
            .nest_service("/control", endpoint(false))
            .nest_service("/await", endpoint(true));
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve real MCP transport");
        });
        (service, task)
    }

    pub fn configure(&self, caller: &protocol::AgentId, config: &mut BackendSpawnConfig) {
        for (name, path) in [
            ("tyde-agent-control", "control"),
            ("tyde-agent-await", "await"),
        ] {
            config
                .startup_mcp_servers
                .push(server::backend::StartupMcpServer {
                    name: name.to_owned(),
                    supports_parallel_tool_calls: true,
                    transport: server::backend::StartupMcpTransport::Http {
                        url: format!("{}/{path}", self.base_url),
                        headers: HashMap::from([("x-tyde-agent-id".to_owned(), caller.0.clone())]),
                        bearer_token_env_var: None,
                    },
                });
        }
    }

    fn changed(&self) {
        self.changed.send_modify(|revision| *revision += 1);
    }

    fn caller(&self, context: &RequestContext<RoleServer>) -> Result<protocol::AgentId, String> {
        let parts = context
            .extensions
            .get::<axum::http::request::Parts>()
            .ok_or("missing MCP request headers")?;
        let caller = protocol::AgentId(
            parts
                .headers
                .get("x-tyde-agent-id")
                .and_then(|value| value.to_str().ok())
                .ok_or("missing caller")?
                .to_owned(),
        );
        if caller != self.root_id
            && !self
                .children
                .lock()
                .expect("children")
                .iter()
                .any(|child| child.launch.agent_id == caller)
        {
            return Err("unknown caller".to_owned());
        }
        Ok(caller)
    }

    fn child_commands(
        &self,
        caller: &protocol::AgentId,
        id: &protocol::AgentId,
    ) -> Result<mpsc::UnboundedSender<ChildCommand>, String> {
        self.children
            .lock()
            .expect("children")
            .iter()
            .find(|child| &child.launch.agent_id == id && &child.launch.parent_agent_id == caller)
            .map(|child| child.commands.clone())
            .ok_or_else(|| format!("{id} is not a direct child of {caller}"))
    }

    async fn spawn(
        self: &Arc<Self>,
        caller: protocol::AgentId,
        args: Value,
    ) -> Result<Value, String> {
        let roots: Vec<String> = serde_json::from_value(
            args.get("workspace_roots")
                .cloned()
                .ok_or("missing workspace_roots")?,
        )
        .map_err(|error| error.to_string())?;
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or("missing child prompt")?
            .to_owned();
        let kind: BackendKind = serde_json::from_value(
            args.get("backend_kind")
                .cloned()
                .ok_or("missing backend_kind")?,
        )
        .map_err(|error| error.to_string())?;
        if kind != B::session_settings_schema().backend_kind {
            return Err("child backend differs from the conformance provider".to_owned());
        }
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("child")
            .to_owned();
        let id = protocol::AgentId(uuid::Uuid::new_v4().to_string());
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let index = {
            let mut children = self.children.lock().expect("children");
            let index = children.len();
            children.push(Child {
                launch: SpawnObservation {
                    agent_id: id.clone(),
                    parent_agent_id: caller,
                    name: name.clone(),
                    backend_kind: kind,
                    workspace_roots: roots.clone(),
                    origin: protocol::AgentOrigin::AgentControl,
                },
                events: Vec::new(),
                requests: Vec::new(),
                idle: false,
                failure: None,
                commands,
            });
            index
        };
        let service = self.clone();
        let mut config = self.config.clone();
        self.configure(&id, &mut config);
        tokio::spawn(async move {
            let (backend, mut events) = match B::spawn(
                roots,
                config,
                SendMessagePayload {
                    message: prompt,
                    images: None,
                    origin: None,
                    tool_response: None,
                },
            )
            .await
            {
                Ok(started) => started,
                Err(error) => {
                    service.children.lock().expect("children")[index].failure = Some(error);
                    service.changed();
                    return;
                }
            };
            let shutdown = loop {
                tokio::select! {
                    event = events.recv_backend() => {
                        let Some(event) = event else { service.children.lock().expect("children")[index].failure = Some("child backend closed".to_owned()); service.changed(); break None; };
                        let mut children = service.children.lock().expect("children");
                        let child = &mut children[index];
                        match event {
                            BackendEvent::Chat(event) => {
                                if let ChatEvent::TypingStatusChanged(active) = event { child.idle = !active; }
                                child.events.push(event);
                            }
                            BackendEvent::ModelRequestTokenUsage(usage) => child.requests.push(usage),
                            BackendEvent::Compaction(_) => {}
                        }
                        drop(children);
                        service.changed();
                    }
                    command = receiver.recv() => match command {
                        Some(ChildCommand::Shutdown(reply)) => break Some(reply),
                        Some(ChildCommand::Send(message, reply)) => {
                            let outcome = backend.send_with_outcome(AgentInput::SendMessage(SendMessagePayload { message, images: None, origin: None, tool_response: None })).await;
                            let result = match outcome { SendOutcome::Accepted => { service.children.lock().expect("children")[index].idle = false; Ok(()) }, other => Err(format!("child rejected input: {other:?}")) };
                            service.changed();
                            let _ = reply.send(result);
                        }
                        None => break None,
                    }
                }
            };
            backend.shutdown().await;
            while let Some(event) = events.recv_backend().await {
                if let BackendEvent::Chat(event) = event {
                    service.children.lock().expect("children")[index]
                        .events
                        .push(event);
                }
            }
            if let Some(reply) = shutdown {
                let _ = reply.send(());
            }
        });
        Ok(json!({"agent_id": id.0, "name": name, "status": "thinking"}))
    }

    async fn await_children(
        &self,
        caller: protocol::AgentId,
        args: Value,
        context: RequestContext<RoleServer>,
    ) -> Result<Value, String> {
        let ids: Vec<protocol::AgentId> =
            serde_json::from_value(args.get("agent_ids").cloned().ok_or("missing agent_ids")?)
                .map_err(|error| error.to_string())?;
        if ids.is_empty() {
            return Err("agent_ids must not be empty".to_owned());
        }
        for id in &ids {
            self.child_commands(&caller, id)?;
        }
        let mut changed = self.changed.subscribe();
        let progress_token = context.meta.get_progress_token();
        let mut progress = 0.0;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let (ready, thinking) = {
                let children = self.children.lock().expect("children");
                let mut ready = Vec::new();
                let mut thinking = Vec::new();
                for id in &ids {
                    let child = children
                        .iter()
                        .find(|child| &child.launch.agent_id == id)
                        .expect("registered child");
                    let status = if child.failure.is_some() {
                        "failed"
                    } else if child.idle {
                        "idle"
                    } else {
                        "thinking"
                    };
                    let entry = json!({"agent_id": id.0, "status": status});
                    if status == "thinking" {
                        thinking.push(entry);
                    } else {
                        ready.push(entry);
                    }
                }
                (ready, thinking)
            };
            if !ready.is_empty() || thinking.is_empty() {
                return Ok(json!({"ready": ready, "still_thinking": thinking}));
            }
            tokio::select! {
                _ = context.ct.cancelled() => return Err("agent await request cancelled".to_owned()),
                result = changed.changed() => result.map_err(|error| error.to_string())?,
                _ = heartbeat.tick(), if progress_token.is_some() => {
                    progress += 1.0;
                    let _ = context.peer.notify_progress(ProgressNotificationParam {
                        progress_token: progress_token.clone().expect("progress token checked"),
                        progress,
                        total: None,
                        message: Some(format!("Waiting for {} Tyde agent(s)", thinking.len())),
                    }).await;
                }
            }
        }
    }

    pub async fn shutdown(&self) {
        let commands: Vec<_> = self
            .children
            .lock()
            .expect("children")
            .iter()
            .map(|child| child.commands.clone())
            .collect();
        for command in commands {
            let (reply, done) = oneshot::channel();
            if command.send(ChildCommand::Shutdown(reply)).is_ok() {
                done.await.expect("child shutdown acknowledgement");
            }
        }
    }
}

impl<B: Backend> ServerHandler for Endpoint<B> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let definitions = if self.await_only {
            vec![(
                "tyde_await_agents",
                "Wait for a direct child to stop thinking.",
                json!({"agent_ids":{"type":"array","items":{"type":"string"}}}),
                vec!["agent_ids"],
            )]
        } else {
            vec![
                (
                    "tyde_spawn_agent",
                    "Start a real child backend and return its agent_id immediately.",
                    json!({"workspace_roots":{"type":"array","items":{"type":"string"}},"prompt":{"type":"string"},"name":{"type":"string"},"backend_kind":{"type":"string"},"cost_hint":{"type":"string"}}),
                    vec!["workspace_roots", "prompt", "backend_kind"],
                ),
                (
                    "tyde_send_agent_message",
                    "Send a message to a direct child.",
                    json!({"agent_id":{"type":"string"},"message":{"type":"string"}}),
                    vec!["agent_id", "message"],
                ),
            ]
        };
        let tools = definitions
            .into_iter()
            .map(|(name, description, properties, required)| {
                Tool::new(
                    name,
                    description,
                    json!({"type":"object","properties":properties,"required":required})
                        .as_object()
                        .expect("tool schema")
                        .clone(),
                )
            })
            .collect();
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let result: Result<Value, String> = async {
            let caller = self.service.caller(&context)?;
            let args = Value::Object(request.arguments.unwrap_or_default());
            match request.name.as_ref() {
                "tyde_spawn_agent" if !self.await_only => self.service.spawn(caller, args).await,
                "tyde_await_agents" if self.await_only => {
                    self.service.await_children(caller, args, context).await
                }
                "tyde_send_agent_message" if !self.await_only => {
                    let id = protocol::AgentId(
                        args.get("agent_id")
                            .and_then(Value::as_str)
                            .ok_or("missing agent_id")?
                            .to_owned(),
                    );
                    let message = args
                        .get("message")
                        .and_then(Value::as_str)
                        .ok_or("missing message")?
                        .to_owned();
                    let command = self.service.child_commands(&caller, &id)?;
                    let (reply, done) = oneshot::channel();
                    command
                        .send(ChildCommand::Send(message, reply))
                        .map_err(|_| "child closed")?;
                    done.await
                        .map_err(|_| "child did not acknowledge input")??;
                    Ok(json!({"agent_id":id.0,"queued":false}))
                }
                _ => Err("unknown tool on this MCP endpoint".to_owned()),
            }
        }
        .await;
        match result {
            Ok(value) => Ok(CallToolResult::success(vec![Content::json(value)?])),
            Err(error) => Ok(CallToolResult::error(vec![Content::text(error)])),
        }
    }
}
