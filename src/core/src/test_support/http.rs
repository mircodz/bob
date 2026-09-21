use crate::core::types::{
    ContentBlock, GenerateOptions, Message, Role, ShellOutput, ShellStream, StreamEvent, ToolSpec,
};
use crate::providers::provider::{Provider, StreamTimeouts};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub(crate) async fn read_request(socket: &mut TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                return;
            }
        }
    }
}

pub(crate) async fn serve(body: &str, hold_open: bool) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = body.as_bytes().to_vec();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let length = body.len() + if hold_open { 1000 } else { 0 };
        write_response(&mut socket, "text/event-stream", &body, Some(length)).await;
        if hold_open {
            let mut byte = [0];
            let read = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
                .await
                .expect("provider left its HTTP producer running");
            assert!(matches!(read, Ok(0) | Err(_)));
        }
    });
    (format!("http://{address}"), server)
}

pub(crate) async fn collect(provider: Arc<dyn Provider>) -> Vec<StreamEvent> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut rx = provider.stream(GenerateOptions::default()).await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("stream did not terminate")
}

pub(crate) fn deadlines() -> StreamTimeouts {
    StreamTimeouts {
        startup: Duration::from_secs(2),
        idle: Duration::from_millis(100),
    }
}

pub(crate) fn assert_error(events: &[StreamEvent]) {
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error { .. })),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::MessageStop { .. })),
        "{events:?}"
    );
}

pub(crate) async fn lifecycle_checks(
    make: impl Fn(String, StreamTimeouts) -> Arc<dyn Provider>,
    delta: &str,
    terminal: &str,
) {
    for body in ["", delta, "data: {broken}\n\n"] {
        let (base, server) = serve(body, false).await;
        assert_error(&collect(make(base, deadlines())).await);
        server.await.unwrap();
    }
    let (base, server) = serve(terminal, true).await;
    let events = collect(make(base, deadlines())).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::MessageStop { .. }))
            .count(),
        1,
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Error { .. })),
        "{events:?}"
    );
    server.await.unwrap();

    let (base, server) = serve(delta, true).await;
    assert_error(&collect(make(base, deadlines())).await);
    server.await.unwrap();

    let (base, server) = serve(delta, true).await;
    let provider = make(base, StreamTimeouts::default());
    let mut rx = provider.stream(GenerateOptions::default()).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    drop(rx);
    server.await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let mut byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)));
    });
    let mut timeouts = deadlines();
    timeouts.startup = Duration::from_millis(100);
    assert_error(&collect(make(base, timeouts)).await);
    server.await.unwrap();
}

async fn write_response(
    socket: &mut TcpStream,
    content_type: &str,
    body: &[u8],
    length: Option<usize>,
) {
    let length = length
        .map(|length| format!("Content-Length: {length}\r\n"))
        .unwrap_or_default();
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{length}Connection: close\r\n\r\n"
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    socket.write_all(body).await.unwrap();
}

pub(crate) async fn response(
    content_type: &str,
    body: &str,
    hold_open: bool,
) -> (reqwest::Response, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let content_type = content_type.to_string();
    let body = body.as_bytes().to_vec();
    let (release, released) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let length = if hold_open { None } else { Some(body.len()) };
        write_response(&mut socket, &content_type, &body, length).await;
        if hold_open {
            let _ = released.await;
        }
    });
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{address}"))
        .send()
        .await
        .unwrap();
    (response, release)
}

pub(crate) fn workflow_spec() -> ToolSpec {
    use crate::agent::env::AgentEnv;
    use crate::agent::team::AgentRegistry;
    use crate::core::events::EventBus;
    use crate::providers::mock::MockProvider;
    use crate::tools::jobs::JobRegistry;
    use crate::tools::registry::{Tool, ToolRegistry};
    use crate::tools::workflow_tool::WorkflowTool;
    use std::sync::atomic::AtomicBool;

    WorkflowTool {
        env: AgentEnv {
            provider: Arc::new(MockProvider::new(vec![])),
            subagent_tools: ToolRegistry::new(None),
            bus: EventBus::new(),
            cwd: String::new(),
            subagent_system: None,
            jobs: JobRegistry::new(),
            team: AgentRegistry::new(),
            lsp: None,
            parent_cancel: Arc::new(AtomicBool::new(false)),
            definitions: Default::default(),
            hooks: Default::default(),
            memory: None,
        },
    }
    .spec()
}

pub(crate) fn assert_workflow_tool(tool: &Value, schema_key: &str, spec: &ToolSpec) {
    assert_eq!(tool["name"], "workflow");
    assert_eq!(tool["description"], spec.description);
    let schema = &tool[schema_key];
    assert_eq!(schema, &spec.input_schema);
    assert_eq!(schema["type"], "object");
    for combiner in ["oneOf", "anyOf", "allOf"] {
        assert!(schema.get(combiner).is_none(), "root contains {combiner}");
    }
    assert_eq!(schema["properties"]["steps"]["$ref"], "#/$defs/steps");
    assert_eq!(schema["properties"]["read_only"]["default"], false);
    assert!(!schema["required"]
        .as_array()
        .is_some_and(|required| required.iter().any(|field| field == "read_only")));
    assert_eq!(
        schema["$defs"]["step"]["oneOf"].as_array().unwrap().len(),
        4
    );
    let examples: Vec<Value> = serde_json::from_str(
        tool["description"]
            .as_str()
            .unwrap()
            .split_once("Examples:\n")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(examples.len(), 3);
    for example in examples {
        crate::workflow::input::validate(&example).unwrap();
    }
}

pub(crate) fn tool_output_fixture() -> (Message, ShellOutput) {
    let shell = ShellOutput {
        exit_code: Some(7),
        stdout: ShellStream {
            text: "日本語\npartial stdout".into(),
            total_bytes: 8192,
            truncated: true,
            output_id: Some("output-1".into()),
        },
        stderr: ShellStream {
            text: "command failed\n".into(),
            total_bytes: 15,
            ..Default::default()
        },
        ..Default::default()
    };
    let message = Message {
        role: Role::Tool,
        content: vec![
            ContentBlock::ToolResult {
                tool_use_id: "shell-call".into(),
                content: shell.clone().into(),
                is_error: Some(true),
            },
            ContentBlock::ToolResult {
                tool_use_id: "text-call".into(),
                content: "plain \"text\"\n日本語".into(),
                is_error: Some(false),
            },
        ],
    };
    (message, shell)
}

pub(crate) fn all_content_blocks() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Text { text: "hi".into() },
        ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read_file".into(),
            input: json!({"path": "a.rs"}),
        },
        ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "ok".into(),
            is_error: Some(false),
        },
        ContentBlock::Thinking {
            thinking: "hmm".into(),
            signature: "sig".into(),
        },
        ContentBlock::RedactedThinking { data: "xxx".into() },
        ContentBlock::ReasoningItem {
            item: json!({"id": "r1"}),
        },
    ]
}
