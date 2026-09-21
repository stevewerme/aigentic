//! The test MCP server over stdio. The registry tests spawn this binary to
//! exercise the child-process transport; it also serves for trying the
//! client by hand: `cargo run -p aigentic-tools --example mcp_echo_server`.

use aigentic_tools::mcp::test_server::EchoServer;
use rmcp::ServiceExt;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let service = EchoServer::new()
        .serve(rmcp::transport::stdio())
        .await
        .expect("serve over stdio");
    let _ = service.waiting().await;
}
