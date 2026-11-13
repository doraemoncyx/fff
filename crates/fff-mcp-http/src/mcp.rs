use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};

use crate::grep_params::{GrepParams, MultiGrepParams};
use crate::params::Params;
use crate::registry::Registry;

const INSTRUCTIONS: &str = "Find indexed files by path fragment, glob, or regex, and search file contents with grep or multi_grep. \
Every call names a tardir (absolute path: a directory, or a single file): the server indexes that directory on demand, keeps \
indexes for parent and child directories and merges their results for find_files. A file tardir searches only that file; when \
the file is not in the Index (ignored, binary, empty, over the size cap) the results say so instead of guessing. grep and \
multi_grep run on one Index covering the tardir, so their results are content matches with one-based line numbers, never JSON. \
Results carry absolute paths and paths relative to tardir. Content search has no index of file contents: the first call on a new \
tardir waits for the scan, and a still-scanning Index answers with a partial result that says so.";

// Stateless MCP surface exposing the same tools as `fff-mcp`.
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

    #[tool(
        name = "grep",
        description = "Search source code with line numbers and surrounding context. For content questions, search directly with a module scope in path. Returned source verifies locations when it contains the required evidence.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::grep::grep(&self.registry, params).map_err(to_error_data)?;
        Ok(CallToolResult::success(vec![Content::text(result.text)]))
    }

    #[tool(
        name = "multi_grep",
        description = "Search for any of several literal identifiers in one call. Set constraints to the directory, module, filename, or glob from the request. Set definitions_only=true for declarations. Results contain source lines and line numbers.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn multi_grep(
        &self,
        Parameters(params): Parameters<MultiGrepParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = crate::grep::multi_grep(&self.registry, params).map_err(to_error_data)?;
        Ok(CallToolResult::success(vec![Content::text(result.text)]))
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
