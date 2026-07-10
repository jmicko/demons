use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::control::{self, CaptureView, ControlRequest, ControlResponse, InstanceInfo};

#[derive(Clone)]
struct DemonsMcpServer {
    scope_id: String,
    config_path: PathBuf,
    #[expect(dead_code, reason = "tool_handler macro accesses this router field")]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InstanceSelection {
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PaneRequest {
    /// Pane identifier returned by list_panes.
    pane_id: String,
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadOutputRequest {
    /// Pane identifier returned by list_panes.
    pane_id: String,
    /// Opaque line cursor returned by a previous read. Omit to read the newest lines.
    cursor: Option<String>,
    /// Maximum physical terminal lines to return. Defaults to 200 and is capped at 1000.
    max_lines: Option<u32>,
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchOutputRequest {
    /// Pane identifier returned by list_panes.
    pane_id: String,
    /// Literal, case-insensitive text to find.
    query: String,
    /// Maximum matches to return. Defaults to 50 and is capped at 200.
    max_results: Option<u32>,
    /// Lines of context before and after each match. Defaults to 2 and is capped at 20.
    context_lines: Option<u32>,
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitOutputRequest {
    /// Pane identifier returned by list_panes.
    pane_id: String,
    /// Optional literal, case-insensitive output text to wait for.
    query: Option<String>,
    /// Optional status to wait for: running, exited, failed, stopped, or another list_panes status.
    status: Option<String>,
    /// Search only output at or after this cursor. Omit to wait for new output.
    after_cursor: Option<String>,
    /// Timeout in milliseconds. Defaults to 30000 and is capped at 60000.
    timeout_ms: Option<u64>,
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendInputRequest {
    /// Pane identifier returned by list_panes.
    pane_id: String,
    /// Text to send verbatim to the pane.
    text: String,
    /// Append Enter after the text. Defaults to false.
    submit: Option<bool>,
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RunCommandRequest {
    /// Shell command to run in a new visible, agent-owned pane.
    command: String,
    /// Working directory. Relative paths resolve from the Demons project root.
    cwd: Option<PathBuf>,
    /// Optional short pane name; Demons makes it unique.
    name: Option<String>,
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitCommandRequest {
    /// Agent-owned command pane identifier.
    pane_id: String,
    /// Timeout in milliseconds. Defaults to 30000 and is capped at 60000.
    timeout_ms: Option<u64>,
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CaptureRequest {
    /// workspace shows the underlying panes; full includes current menus and dialogs.
    view: Option<CaptureView>,
    /// Required only when more than one running Demons instance uses this project scope.
    instance_id: Option<String>,
}

#[tool_router]
impl DemonsMcpServer {
    fn new(scope_id: String, config_path: PathBuf) -> Self {
        Self {
            scope_id,
            config_path,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List running Demons instances bound to this configured project scope. Unrelated projects are never returned."
    )]
    async fn list_instances(&self) -> Result<CallToolResult, McpError> {
        let scope = self.scope_id.clone();
        let config_path = self.config_path.clone();
        let instances =
            tokio::task::spawn_blocking(move || control::discover_instances(&scope, &config_path))
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        json_result(&instances)
    }

    #[tool(
        description = "List task, terminal, and agent command panes with status and output cursor metadata."
    )]
    async fn list_panes(
        &self,
        Parameters(request): Parameters<InstanceSelection>,
    ) -> Result<CallToolResult, McpError> {
        self.call(request.instance_id, ControlRequest::ListPanes)
            .await
    }

    #[tool(
        description = "Read bounded plain-text process history from one pane. This returns process output, not composed TUI decoration."
    )]
    async fn read_output(
        &self,
        Parameters(request): Parameters<ReadOutputRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::ReadOutput {
                pane_id: request.pane_id,
                cursor: request.cursor,
                max_lines: request.max_lines.unwrap_or(200),
            },
        )
        .await
    }

    #[tool(
        description = "Search one pane's plain-text history for a literal case-insensitive string."
    )]
    async fn search_output(
        &self,
        Parameters(request): Parameters<SearchOutputRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::SearchOutput {
                pane_id: request.pane_id,
                query: request.query,
                max_results: request.max_results.unwrap_or(50),
                context_lines: request.context_lines.unwrap_or(2),
            },
        )
        .await
    }

    #[tool(
        description = "Wait for new literal output or a pane status change without polling repeatedly."
    )]
    async fn wait_for_output(
        &self,
        Parameters(request): Parameters<WaitOutputRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::WaitForOutput {
                pane_id: request.pane_id,
                query: request.query,
                status: request.status,
                after_cursor: request.after_cursor,
                timeout_ms: request.timeout_ms.unwrap_or(30_000),
            },
        )
        .await
    }

    #[tool(description = "Restart one configured task and its configured dependents.")]
    async fn restart_task(
        &self,
        Parameters(request): Parameters<PaneRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::RestartTask {
                pane_id: request.pane_id,
            },
        )
        .await
    }

    #[tool(description = "Restart all configured task panes using dependency order.")]
    async fn restart_all(
        &self,
        Parameters(request): Parameters<InstanceSelection>,
    ) -> Result<CallToolResult, McpError> {
        self.call(request.instance_id, ControlRequest::RestartAll)
            .await
    }

    #[tool(description = "Send SIGINT to the process group running in one pane.")]
    async fn interrupt_pane(
        &self,
        Parameters(request): Parameters<PaneRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::InterruptPane {
                pane_id: request.pane_id,
            },
        )
        .await
    }

    #[tool(
        description = "Send explicit text to an interactive pane, optionally followed by Enter."
    )]
    async fn send_input(
        &self,
        Parameters(request): Parameters<SendInputRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::SendInput {
                pane_id: request.pane_id,
                text: request.text,
                submit: request.submit.unwrap_or(false),
            },
        )
        .await
    }

    #[tool(
        description = "Run a shell command in a new visible, agent-owned Demons pane and return its pane identifier immediately."
    )]
    async fn run_command(
        &self,
        Parameters(request): Parameters<RunCommandRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::RunCommand {
                command: request.command,
                cwd: request.cwd,
                name: request.name,
            },
        )
        .await
    }

    #[tool(description = "Wait for an agent-owned command pane to finish.")]
    async fn wait_for_command(
        &self,
        Parameters(request): Parameters<WaitCommandRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::WaitForCommand {
                pane_id: request.pane_id,
                timeout_ms: request.timeout_ms.unwrap_or(30_000),
            },
        )
        .await
    }

    #[tool(
        description = "Close an agent-owned command pane, terminating its process first when needed."
    )]
    async fn close_command(
        &self,
        Parameters(request): Parameters<PaneRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            request.instance_id,
            ControlRequest::CloseCommand {
                pane_id: request.pane_id,
            },
        )
        .await
    }

    #[tool(
        description = "Render the current Demons terminal grid as a PNG for visual layout diagnosis. Use read_output for process text."
    )]
    async fn capture_tui(
        &self,
        Parameters(request): Parameters<CaptureRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.call_capture(request.instance_id, request.view.unwrap_or_default())
            .await
    }

    async fn call(
        &self,
        instance_id: Option<String>,
        request: ControlRequest,
    ) -> Result<CallToolResult, McpError> {
        let instance = self.select_instance(instance_id).await?;
        let response = tokio::task::spawn_blocking(move || control::request(&instance, &request))
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        response_result(response)
    }

    async fn call_capture(
        &self,
        instance_id: Option<String>,
        view: CaptureView,
    ) -> Result<CallToolResult, McpError> {
        let instance = self.select_instance(instance_id).await?;
        let response = tokio::task::spawn_blocking(move || {
            control::request(&instance, &ControlRequest::CaptureTui { view })
        })
        .await
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        match response {
            ControlResponse::Capture { capture } => {
                let metadata = serde_json::json!({
                    "view": capture.view,
                    "columns": capture.columns,
                    "rows": capture.rows,
                    "width": capture.width,
                    "height": capture.height,
                    "font": capture.font,
                    "missing_glyphs": capture.missing_glyphs,
                });
                Ok(CallToolResult::success(vec![
                    ContentBlock::text(metadata.to_string()),
                    ContentBlock::image(BASE64.encode(capture.png), "image/png"),
                ]))
            }
            ControlResponse::Error { code, message } => {
                Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "{code}: {message}"
                ))]))
            }
            other => Err(McpError::internal_error(
                format!("unexpected capture response: {other:?}"),
                None,
            )),
        }
    }

    async fn select_instance(&self, instance_id: Option<String>) -> Result<InstanceInfo, McpError> {
        let scope = self.scope_id.clone();
        let config_path = self.config_path.clone();
        let instances =
            tokio::task::spawn_blocking(move || control::discover_instances(&scope, &config_path))
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        match instance_id {
            Some(instance_id) => instances
                .into_iter()
                .find(|instance| instance.instance_id == instance_id)
                .ok_or_else(|| {
                    McpError::invalid_params(
                        "the requested Demons instance is not running in this project scope",
                        None,
                    )
                }),
            None if instances.len() == 1 => Ok(instances.into_iter().next().unwrap()),
            None if instances.is_empty() => Err(McpError::invalid_params(
                "no MCP-enabled Demons instance is running for this project scope",
                None,
            )),
            None => Err(McpError::invalid_params(
                "multiple Demons instances use this project scope; pass instance_id from list_instances",
                None,
            )),
        }
    }
}

#[tool_handler]
impl ServerHandler for DemonsMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "This server is bound to one Demons project scope. Use read_output for process text. Use capture_tui only when visual TUI layout matters. Mutation tools require Full access in Demons settings.",
        )
    }
}

fn json_result(value: &impl serde::Serialize) -> Result<CallToolResult, McpError> {
    serde_json::to_value(value)
        .map(CallToolResult::structured)
        .map_err(|error| McpError::internal_error(error.to_string(), None))
}

fn response_result(response: ControlResponse) -> Result<CallToolResult, McpError> {
    if let ControlResponse::Error { code, message } = response {
        return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "{code}: {message}"
        ))]));
    }
    json_result(&response)
}

pub fn serve(scope_id: String, config_path: PathBuf) -> Result<()> {
    uuid::Uuid::parse_str(&scope_id).context("invalid MCP project scope ID")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to initialize MCP runtime")?;
    runtime.block_on(async move {
        let service = DemonsMcpServer::new(scope_id, config_path)
            .serve(rmcp::transport::stdio())
            .await
            .context("failed to start MCP stdio server")?;
        service.waiting().await.context("MCP server failed")?;
        Ok(())
    })
}
