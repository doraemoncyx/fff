use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};

use crate::params::Params;
use crate::registry::Registry;

const INSTRUCTIONS: &str = "Find indexed files by path fragment, glob, or regex. Every call names a tardir: \
the server indexes it on demand, keeps indexes for parent and child directories and merges their results. \
Results carry absolute paths and paths relative to tardir.";

/// Stateless MCP surface exposing the same `find_files` tool as `fff-mcp`.
#[derive(Clone)]
pub struct FindFilesServer {
    registry: Arc<Registry>,
}

impl FindFilesServer {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[tool_router]
impl FindFilesServer {
    #[tool(
        name = "find_files",
        description = "Find indexed files by path fragment, glob, or regex. Fragments treat hyphens and underscores alike before fuzzy suggestions. Put a known directory in path. An empty query lists files in scope.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn find_files(
        &self,
        Parameters(params): Parameters<Params>,
    ) -> Result<CallToolResult, ErrorData> {
        let response = self.registry.query(&params).map_err(to_error_data)?;
        let value = serde_json::to_value(&response)
            .map_err(|e| ErrorData::internal_error(format!("serialize response: {e}"), None))?;
        Ok(CallToolResult::structured(value))
    }
}

#[tool_handler]
impl ServerHandler for FindFilesServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "fff-mcp-http",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

fn to_error_data(error: crate::ApiError) -> ErrorData {
    match error.code() {
        "INVALID_PARAMS" => ErrorData::invalid_params(error.message().to_string(), None),
        _ => {
            tracing::warn!(%error, "tool call failed");
            ErrorData::internal_error(error.message().to_string(), None)
        }
    }
}
