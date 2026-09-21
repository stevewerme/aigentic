//! MCP client: a server's tools join the registry as `mcp.<server>.<tool>`
//! and go through policy like any other tool. Transports are stdio (a
//! child process) and streamable HTTP. Descriptions and schemas from a
//! server are untrusted text; they are passed to the model unchanged and
//! the REPL shows them to the human at connect time.

use aigentic_core::{BoxFuture, RiskClass, Tool, ToolError, ToolOutput};
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{Peer, ServiceExt};
use serde::{Deserialize, Serialize};

use crate::truncate::{DEFAULT_OUTPUT_CAP, truncate_output};

/// How to reach a server. Serialises externally tagged, so `aigentic.toml`
/// writes `transport = { stdio = { command = "npx", args = [...] } }` or
/// `transport = { http = { url = "..." } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Http {
        url: String,
    },
}

/// One `[[mcp_servers]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    /// Risk class of every tool the server offers. `network` until a human
    /// downgrades it.
    #[serde(default = "default_class")]
    pub class: RiskClass,
}

fn default_class() -> RiskClass {
    RiskClass::Network
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("mcp server `{server}`: invalid name (use letters, digits, `-` and `_`)")]
    Name { server: String },
    #[error("mcp server `{server}`: failed to start `{command}`: {source}")]
    Spawn {
        server: String,
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("mcp server `{server}`: connect failed: {message}")]
    Connect { server: String, message: String },
    #[error("mcp server `{server}`: {message}")]
    Protocol { server: String, message: String },
    #[error(
        "mcp server `{server}`: tool `{tool}` has a schema this harness cannot represent: {message}"
    )]
    Schema {
        server: String,
        tool: String,
        message: String,
    },
}

/// A connected server. Dropping it ends the session; the registry keeps
/// it alive for as long as its tools are registered.
pub struct McpServer {
    name: String,
    class: RiskClass,
    peer: Peer<RoleClient>,
    service: Option<RunningService<RoleClient, ()>>,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("name", &self.name)
            .field("class", &self.class)
            .finish()
    }
}

impl McpServer {
    /// Connect over the configured transport and complete the MCP
    /// handshake.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        check_name(&config.name)?;
        match &config.transport {
            McpTransport::Stdio { command, args } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                let transport = TokioChildProcess::new(cmd).map_err(|source| McpError::Spawn {
                    server: config.name.clone(),
                    command: command.clone(),
                    source,
                })?;
                Self::connect_transport(&config.name, config.class, transport).await
            }
            McpTransport::Http { url } => {
                let transport = StreamableHttpClientTransport::from_uri(url.as_str());
                Self::connect_transport(&config.name, config.class, transport).await
            }
        }
    }

    /// Connect over any rmcp transport; tests use an in-process duplex.
    pub async fn connect_transport<T, E, A>(
        name: &str,
        class: RiskClass,
        transport: T,
    ) -> Result<Self, McpError>
    where
        T: rmcp::transport::IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        check_name(name)?;
        let service = ().serve(transport).await.map_err(|e| McpError::Connect {
            server: name.to_owned(),
            message: e.to_string(),
        })?;
        Ok(Self {
            name: name.to_owned(),
            class,
            peer: service.peer().clone(),
            service: Some(service),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn class(&self) -> RiskClass {
        self.class
    }

    /// The server's tool list, unchanged.
    pub async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>, McpError> {
        self.peer
            .list_all_tools()
            .await
            .map_err(|e| McpError::Protocol {
                server: self.name.clone(),
                message: e.to_string(),
            })
    }

    /// Wrap one of the server's tools as a registry tool named
    /// `mcp.<server>.<tool>` with the server's class.
    pub fn tool(&self, remote: &rmcp::model::Tool) -> Result<McpTool, McpError> {
        let schema_json = serde_json::Value::Object((*remote.input_schema).clone());
        let schema: schemars::schema::RootSchema =
            serde_json::from_value(schema_json).map_err(|e| McpError::Schema {
                server: self.name.clone(),
                tool: remote.name.to_string(),
                message: e.to_string(),
            })?;
        Ok(McpTool {
            name: format!("mcp.{}.{}", self.name, remote.name),
            remote_name: remote.name.to_string(),
            description: remote
                .description
                .as_deref()
                .unwrap_or("(no description from server)")
                .to_owned(),
            schema,
            class: self.class,
            peer: self.peer.clone(),
            server: self.name.clone(),
            output_cap: DEFAULT_OUTPUT_CAP,
        })
    }

    /// Every tool the server offers, wrapped.
    pub async fn tools(&self) -> Result<Vec<McpTool>, McpError> {
        self.list_tools()
            .await?
            .iter()
            .map(|t| self.tool(t))
            .collect()
    }

    /// End the session; a child process is shut down.
    pub async fn close(&mut self) {
        if let Some(mut service) = self.service.take() {
            let _ = service.close().await;
        }
    }
}

fn check_name(name: &str) -> Result<(), McpError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(McpError::Name {
            server: name.to_owned(),
        })
    }
}

/// A server tool as the registry sees it.
pub struct McpTool {
    name: String,
    remote_name: String,
    description: String,
    schema: schemars::schema::RootSchema,
    class: RiskClass,
    peer: Peer<RoleClient>,
    server: String,
    output_cap: usize,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.name)
            .field("class", &self.class)
            .finish()
    }
}

impl McpTool {
    pub fn with_output_cap(mut self, cap: usize) -> Self {
        self.output_cap = cap;
        self
    }

    pub fn server(&self) -> &str {
        &self.server
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        self.schema.clone()
    }

    fn risk_class(&self) -> RiskClass {
        self.class
    }

    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let arguments = match args {
                serde_json::Value::Object(map) => map,
                serde_json::Value::Null => serde_json::Map::new(),
                other => {
                    return Err(ToolError::InvalidArgs(format!(
                        "arguments must be an object, got {other}"
                    )));
                }
            };
            let params =
                CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments);
            let result =
                self.peer.call_tool(params).await.map_err(|e| {
                    ToolError::Execution(format!("mcp server `{}`: {e}", self.server))
                })?;
            let mut parts: Vec<String> = result
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text(t) => t.text.clone(),
                    ContentBlock::Image(i) => format!("[image {}]", i.mime_type),
                    ContentBlock::Audio(a) => format!("[audio {}]", a.mime_type),
                    ContentBlock::Resource(_) => "[embedded resource]".to_owned(),
                    ContentBlock::ResourceLink(r) => format!("[resource {}]", r.uri),
                    _ => "[unsupported content]".to_owned(),
                })
                .collect();
            if parts.is_empty()
                && let Some(structured) = &result.structured_content
            {
                parts.push(structured.to_string());
            }
            Ok(ToolOutput {
                content: truncate_output(&parts.join("\n"), self.output_cap),
                is_error: result.is_error.unwrap_or(false),
            })
        })
    }
}

/// A tools-only MCP server for tests and for trying the client by hand:
/// `echo` returns its text, `add` returns a sum, `fail` returns an error
/// result. `cargo run -p aigentic-tools --example mcp_echo_server` serves
/// it over stdio.
pub mod test_server {
    use rmcp::handler::server::tool::ToolRouter;
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::model::{CallToolResult, ContentBlock};
    use rmcp::{schemars, tool, tool_router};
    use serde::Deserialize;

    #[derive(Debug, Deserialize, schemars::JsonSchema)]
    pub struct EchoArgs {
        /// Text to echo back.
        pub text: String,
    }

    #[derive(Debug, Deserialize, schemars::JsonSchema)]
    pub struct AddArgs {
        pub left: i64,
        pub right: i64,
    }

    #[derive(Debug, Clone)]
    pub struct EchoServer {
        // Read by the `#[tool_handler]` impl the macro emits; dead-code
        // analysis does not see through it.
        #[allow(dead_code)]
        tool_router: ToolRouter<Self>,
    }

    impl Default for EchoServer {
        fn default() -> Self {
            Self::new()
        }
    }

    #[tool_router(server_handler)]
    impl EchoServer {
        pub fn new() -> Self {
            Self {
                tool_router: Self::tool_router(),
            }
        }

        #[tool(name = "echo", description = "Echo the given text back.")]
        fn echo(&self, Parameters(EchoArgs { text }): Parameters<EchoArgs>) -> CallToolResult {
            CallToolResult::success(vec![ContentBlock::text(format!("echo: {text}"))])
        }

        #[tool(name = "add", description = "Add two integers.")]
        fn add(&self, Parameters(AddArgs { left, right }): Parameters<AddArgs>) -> CallToolResult {
            CallToolResult::success(vec![ContentBlock::text((left + right).to_string())])
        }

        #[tool(name = "fail", description = "Always returns an error result.")]
        fn fail(&self) -> CallToolResult {
            CallToolResult::error(vec![ContentBlock::text("it failed, as asked")])
        }
    }
}
