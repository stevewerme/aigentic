//! MCP client against the in-process echo server: over a duplex pipe for
//! the protocol, and over stdio by spawning the `mcp_echo_server` example
//! for the child-process transport. No network.

use std::path::PathBuf;

use aigentic_core::{RiskClass, Tool};
use aigentic_tools::mcp::test_server::EchoServer;
use aigentic_tools::{
    DEFAULT_TIMEOUT, McpError, McpServer, McpServerConfig, McpTransport, ToolRegistry,
};
use rmcp::ServiceExt;
use serde_json::json;

/// Serve the echo server in-process and connect a client to it.
async fn duplex_server(name: &str, class: RiskClass) -> McpServer {
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_side);
    tokio::spawn(async move {
        let service = EchoServer::new()
            .serve((server_read, server_write))
            .await
            .expect("server serves");
        let _ = service.waiting().await;
    });
    let (client_read, client_write) = tokio::io::split(client_side);
    McpServer::connect_transport(name, class, (client_read, client_write))
        .await
        .expect("client connects")
}

fn example_binary() -> PathBuf {
    // target/debug/deps/mcp-<hash> -> target/debug/examples/mcp_echo_server
    let exe = std::env::current_exe().unwrap();
    let debug = exe.parent().unwrap().parent().unwrap();
    let path = debug.join("examples").join("mcp_echo_server");
    assert!(
        path.exists(),
        "{} missing; cargo builds examples before tests",
        path.display()
    );
    path
}

#[tokio::test]
async fn connect_list_and_call_over_duplex() {
    let server = duplex_server("echo", RiskClass::Network).await;
    let tools = server.list_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort();
    assert_eq!(names, vec!["add", "echo", "fail"]);

    let echo = server
        .tool(tools.iter().find(|t| t.name == "echo").unwrap())
        .unwrap();
    assert_eq!(echo.name(), "mcp.echo.echo");
    assert_eq!(echo.description(), "Echo the given text back.");
    assert_eq!(echo.risk_class(), RiskClass::Network, "network by default");
    let schema = serde_json::to_value(echo.schema()).unwrap();
    assert!(schema["properties"]["text"].is_object(), "{schema}");
    assert_eq!(schema["required"], json!(["text"]));

    let out = echo.call(json!({"text": "hi"})).await.unwrap();
    assert_eq!((out.content.as_str(), out.is_error), ("echo: hi", false));

    let add = server
        .tool(tools.iter().find(|t| t.name == "add").unwrap())
        .unwrap();
    let out = add.call(json!({"left": 2, "right": 40})).await.unwrap();
    assert_eq!(out.content, "42");

    let fail = server
        .tool(tools.iter().find(|t| t.name == "fail").unwrap())
        .unwrap();
    let out = fail.call(json!({})).await.unwrap();
    assert_eq!(
        (out.content.as_str(), out.is_error),
        ("it failed, as asked", true)
    );

    assert!(matches!(
        echo.call(json!("not an object")).await.unwrap_err(),
        aigentic_core::ToolError::InvalidArgs(_)
    ));
    // A schema violation is reported by the server as an error, never a crash.
    match echo.call(json!({"wrong": 1})).await {
        Ok(out) => assert!(out.is_error),
        Err(aigentic_core::ToolError::Execution(_)) => {}
        Err(other) => panic!("{other}"),
    }
}

#[tokio::test]
async fn registry_registers_server_tools_with_the_configured_class() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry =
        ToolRegistry::builtin(aigentic_tools::Workdir::new(dir.path()), DEFAULT_TIMEOUT);
    let server = duplex_server("docs", RiskClass::Read).await;
    let specs = registry.register_mcp(server).await.unwrap();
    assert_eq!(
        specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["mcp.docs.add", "mcp.docs.echo", "mcp.docs.fail"]
    );
    assert_eq!(registry.len(), 9);
    assert_eq!(registry.mcp_servers(), vec!["docs"]);
    let tool = registry.get("mcp.docs.echo").unwrap();
    assert_eq!(tool.risk_class(), RiskClass::Read, "a human downgraded it");
    let out = tool.call(json!({"text": "via registry"})).await.unwrap();
    assert_eq!(out.content, "echo: via registry");

    // The model's view: MCP specs sorted in among the built-ins.
    let names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
    assert_eq!(names, registry.names());
    registry.close_mcp().await;
}

#[tokio::test]
async fn stdio_child_process_transport() {
    let config = McpServerConfig {
        name: "child".into(),
        transport: McpTransport::Stdio {
            command: example_binary().to_string_lossy().into_owned(),
            args: vec![],
        },
        class: RiskClass::Network,
    };
    let mut registry = ToolRegistry::empty();
    let specs = registry.connect_mcp(&config).await.unwrap();
    assert_eq!(specs.len(), 3);
    let out = registry
        .get("mcp.child.echo")
        .unwrap()
        .call(json!({"text": "over stdio"}))
        .await
        .unwrap();
    assert_eq!(out.content, "echo: over stdio");

    let err = registry.connect_mcp(&config).await.unwrap_err();
    assert!(err.to_string().contains("already connected"), "{err}");
    registry.close_mcp().await;
}

#[tokio::test]
async fn a_server_that_cannot_start_is_an_error_not_a_panic() {
    let config = McpServerConfig {
        name: "nope".into(),
        transport: McpTransport::Stdio {
            command: "/nonexistent/mcp-server".into(),
            args: vec![],
        },
        class: RiskClass::Network,
    };
    let err = McpServer::connect(&config).await.unwrap_err();
    assert!(
        matches!(err, McpError::Spawn { .. } | McpError::Connect { .. }),
        "{err}"
    );
    let bad = McpServerConfig {
        name: "has space".into(),
        ..config
    };
    assert!(matches!(
        McpServer::connect(&bad).await.unwrap_err(),
        McpError::Name { .. }
    ));
}

#[test]
fn config_deserialises_from_the_plan_shape() {
    let toml = r#"
name = "docs"
transport = { stdio = { command = "npx", args = ["-y", "@example/docs-mcp"] } }
class = "read"
"#;
    let c: McpServerConfig = toml::from_str(toml).unwrap();
    assert_eq!(c.name, "docs");
    assert_eq!(
        c.transport,
        McpTransport::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "@example/docs-mcp".into()]
        }
    );
    assert_eq!(c.class, RiskClass::Read);

    let c: McpServerConfig = toml::from_str(
        "name = \"h\"\ntransport = { http = { url = \"http://127.0.0.1:1/mcp\" } }\n",
    )
    .unwrap();
    assert_eq!(c.class, RiskClass::Network, "network by default");
    assert!(
        toml::from_str::<McpServerConfig>(
            "name = \"x\"\ntransport = { stdio = { command = \"a\" } }\nbogus = 1\n"
        )
        .is_err()
    );
}
