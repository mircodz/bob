mod lsp;
mod mcp;
mod memory;
mod web;

pub use mcp::{McpState, McpStatus, McpTransportKind};

use anyhow::{ensure, Context, Result};
use bob_core::core::config::BobConfig;
use bob_core::core::memory::{MemoryEntry, MemoryRepository, MemoryScope};
use bob_core::tools::web::{WebClient, WebRequest};
use bob_kernel::{
    Invocation, PreparedTool, ToolBody, ToolCapabilities, ToolDescriptor, ToolOutcome,
    ToolPresentation, ToolResultPresentation,
};
use memory::MemoryRequest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const INPUT_LIMIT: usize = 1024 * 1024;
const RESULT_LIMIT: usize = 16 * 1024 * 1024;

pub struct Services {
    workspace: PathBuf,
    project_scope: MemoryScope,
    memories: Arc<dyn MemoryRepository>,
    web: WebClient,
    mcp: mcp::McpService,
    lsp: lsp::LspService,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundService {
    version: u32,
    invocation_sha256: String,
    action_sha256: String,
    action: ServiceAction,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "service",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum ServiceAction {
    Web(WebRequest),
    Memory(MemoryRequest),
    Mcp(Value),
    Lsp(Value),
}

impl Services {
    pub async fn create(
        config: &BobConfig,
        workspace: &Path,
        memories: Arc<dyn MemoryRepository>,
    ) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace).context("resolve services workspace")?;
        let project_scope = MemoryScope::project(&workspace)?;
        let web = WebClient::new()?;
        let lsp = lsp::LspService::start(&config.lsp_servers, &workspace);
        let mcp = mcp::McpService::start(&config.mcp_servers, &workspace).await;
        Ok(Self {
            workspace,
            project_scope,
            memories,
            web,
            mcp,
            lsp,
        })
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut descriptors = web::descriptors();
        descriptors.push(memory::descriptor());
        descriptors.extend(self.lsp.descriptors());
        descriptors.extend(self.mcp.descriptors());
        descriptors
    }

    pub fn contains(&self, name: &str) -> bool {
        matches!(
            name,
            "web_fetch" | "web_search" | "memory" | "lsp" | "rename_symbol" | "code_action"
        ) || self.mcp.contains(name)
    }

    pub fn tool_capabilities(&self, name: &str) -> ToolCapabilities {
        match name {
            "memory" => ToolCapabilities {
                plan_safe: true,
                ..Default::default()
            },
            "web_fetch" | "web_search" => ToolCapabilities {
                external_access: true,
                plan_safe: true,
                ..Default::default()
            },
            "lsp" => ToolCapabilities {
                plan_safe: true,
                ..Default::default()
            },
            "rename_symbol" | "code_action" => ToolCapabilities {
                mutates_workspace: true,
                ..Default::default()
            },
            _ => ToolCapabilities {
                external_access: true,
                mutates_workspace: true,
                executes_process: true,
                ..Default::default()
            },
        }
    }

    pub fn prepared_capabilities(&self, tool: &PreparedTool) -> Result<ToolCapabilities> {
        self.check_workspace(&tool.invocation)?;
        let bound: BoundService = serde_json::from_value(tool.specification.clone())?;
        bound.validate(&tool.invocation)?;
        Ok(
            if (tool.invocation.name == "memory" && tool.invocation.input["action"] == "list")
                || (tool.invocation.name == "code_action"
                    && tool.invocation.input.get("apply").is_none())
            {
                ToolCapabilities {
                    plan_safe: true,
                    ..Default::default()
                }
            } else {
                self.tool_capabilities(&tool.invocation.name)
            },
        )
    }

    pub fn prepared_authorization_requirements(
        &self,
        tool: &PreparedTool,
    ) -> Result<Vec<bob_kernel::PreparedAuthorizationRequirement>> {
        self.check_workspace(&tool.invocation)?;
        let bound: BoundService = serde_json::from_value(tool.specification.clone())?;
        bound.validate(&tool.invocation)?;
        match &bound.action {
            ServiceAction::Lsp(spec) => Ok(lsp::LspService::prepared_targets(
                &tool.invocation.name,
                &tool.invocation.input,
                spec,
                &self.workspace,
            )?
            .into_iter()
            .map(|path| bob_kernel::PreparedAuthorizationRequirement::EditFile { path })
            .collect()),
            _ => Ok(Vec::new()),
        }
    }

    pub async fn system_context(&self, workspace: &str, base: &str) -> Result<String> {
        ensure!(
            std::fs::canonicalize(workspace)? == self.workspace,
            "context workspace does not match services"
        );
        bob_core::core::memory::with_memory_context(
            base.to_owned(),
            workspace.to_owned(),
            Some(self.memories.clone()),
        )
        .await
    }

    pub fn project_scope(&self) -> MemoryScope {
        self.project_scope.clone()
    }

    pub fn memory_entries(&self, scope: &MemoryScope, offset: usize) -> Result<Vec<MemoryEntry>> {
        ensure!(
            *scope == MemoryScope::Global || *scope == self.project_scope,
            "memory scope is outside this service workspace"
        );
        self.memories.list_page(scope, offset, 50)
    }

    pub fn mcp_statuses(&self) -> Vec<McpStatus> {
        self.mcp.statuses()
    }

    pub fn lsp_statuses(&self) -> Vec<(String, bob_core::lsp::Health)> {
        self.lsp.statuses()
    }

    pub fn notify_changed(&self, path: &Path) {
        self.lsp.notify_changed(path);
    }

    pub async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        let (_sender, cancel) = watch::channel(false);
        self.prepare_with_cancel(invocation, cancel).await
    }

    pub async fn prepare_with_cancel(
        &self,
        invocation: Invocation,
        cancel: watch::Receiver<bool>,
    ) -> Result<PreparedTool> {
        ensure!(
            !*cancel.borrow() && cancel.has_changed().is_ok(),
            "service preparation cancelled"
        );
        self.check_workspace(&invocation)?;
        ensure!(
            serde_json::to_vec(&invocation.input)?.len() <= INPUT_LIMIT,
            "service input exceeds {INPUT_LIMIT} bytes"
        );
        let name = invocation.name.as_str();
        let action = match name {
            "web_fetch" | "web_search" => {
                ServiceAction::Web(WebRequest::prepare(name, &invocation.input)?)
            }
            "memory" => ServiceAction::Memory(MemoryRequest::prepare(
                &invocation.input,
                &self.project_scope,
            )?),
            "lsp" | "rename_symbol" | "code_action" => ServiceAction::Lsp(
                self.lsp
                    .prepare_with_cancel(name, &invocation.input, cancel)
                    .await?,
            ),
            _ if self.mcp.contains(name) => {
                ServiceAction::Mcp(self.mcp.prepare(name, &invocation.input)?)
            }
            _ => anyhow::bail!("unknown integration tool: {name}"),
        };
        let preview = match &action {
            ServiceAction::Web(WebRequest::Fetch { url, .. }) => format!("Fetch {url}"),
            ServiceAction::Web(WebRequest::Search { query, .. }) => format!("Search web: {query}"),
            ServiceAction::Memory(request) => request.summary(),
            ServiceAction::Lsp(spec) => lsp::LspService::prepared_preview(spec)?,
            ServiceAction::Mcp(_) => format!("MCP {name}: {}", invocation.input),
        };
        let preview = text_preview(&preview, 4096);
        let presentation = Some(ToolPresentation {
            title: name.into(),
            target: None,
            body: ToolBody::Text {
                text: preview.clone(),
            },
            details: vec![format!("Workspace: {}", self.workspace.display())],
        });
        let bound = BoundService {
            version: 1,
            invocation_sha256: invocation_digest(&invocation)?,
            action_sha256: action_digest(&invocation, &action)?,
            action,
        };
        Ok(PreparedTool {
            invocation,
            specification: serde_json::to_value(bound)?,
            preview,
            presentation,
        })
    }

    pub async fn execute(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        self.check_workspace(&tool.invocation)?;
        let bound: BoundService = serde_json::from_value(tool.specification)?;
        bound.validate(&tool.invocation)?;
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Ok(interrupted());
        }
        let name = tool.invocation.name.as_str();
        match &bound.action {
            ServiceAction::Lsp(spec) => {
                lsp::LspService::validate_prepared(name, &tool.invocation.input, spec)?
            }
            ServiceAction::Mcp(spec) => ensure!(
                *spec == self.mcp.prepare(name, &tool.invocation.input)?,
                "prepared MCP request changed"
            ),
            _ => {}
        }
        let mut result = match bound.action {
            ServiceAction::Memory(request) => {
                ensure!(
                    name == "memory"
                        && request
                            == MemoryRequest::prepare(&tool.invocation.input, &self.project_scope)?,
                    "prepared memory request changed"
                );
                memory::execute(self.memories.clone(), request, cancel).await?
            }
            ServiceAction::Lsp(spec) => self.lsp.execute(name, &spec, &cancel).await?,
            action => {
                let work = async {
                    match action {
                        ServiceAction::Web(request) => {
                            ensure!(
                                request == WebRequest::prepare(name, &tool.invocation.input)?,
                                "prepared web request changed"
                            );
                            web::execute(&self.web, &request).await
                        }
                        ServiceAction::Mcp(spec) => self.mcp.execute(name, &spec).await,
                        ServiceAction::Memory(_) | ServiceAction::Lsp(_) => unreachable!(),
                    }
                };
                let cancellation = cancel.clone();
                tokio::select! {
                    biased;
                    _ = cancelled(cancellation) => return Ok(interrupted()),
                    result = tokio::time::timeout(REQUEST_TIMEOUT, work) => result.context("integration request timed out; external effect may be uncertain")??,
                }
            }
        };
        ensure!(
            result.text.len() <= RESULT_LIMIT,
            "integration result exceeds {RESULT_LIMIT} bytes"
        );
        if result.presentation.is_none() {
            result.presentation = Some(ToolResultPresentation {
                body_preview: Some(text_preview(&result.text, 4096)),
                ..Default::default()
            });
        }
        Ok(result)
    }

    fn check_workspace(&self, invocation: &Invocation) -> Result<()> {
        ensure!(
            std::fs::canonicalize(&invocation.workspace)? == self.workspace,
            "service workspace does not match invocation"
        );
        Ok(())
    }
}

impl BoundService {
    fn validate(&self, invocation: &Invocation) -> Result<()> {
        ensure!(
            self.version == 1 && self.invocation_sha256 == invocation_digest(invocation)?,
            "prepared service invocation changed"
        );
        ensure!(
            self.action_sha256 == action_digest(invocation, &self.action)?,
            "prepared service action changed"
        );
        Ok(())
    }
}

fn action_digest(invocation: &Invocation, action: &ServiceAction) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&serde_json::to_value((
            invocation, action
        ))?)?)
    ))
}

fn invocation_digest(invocation: &Invocation) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(invocation)?)
    ))
}

async fn cancelled(mut cancel: watch::Receiver<bool>) {
    let _ = cancel.wait_for(|cancelled| *cancelled).await;
}

fn text_preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.into();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n…[preview truncated; full result retained]",
        &text[..end]
    )
}

fn outcome(text: String) -> ToolOutcome {
    ToolOutcome {
        presentation: Some(ToolResultPresentation {
            body_preview: Some(text_preview(&text, 4096)),
            ..Default::default()
        }),
        text,
        is_error: false,
        interrupted: false,
    }
}

#[cfg(test)]
mod tests;

fn interrupted() -> ToolOutcome {
    ToolOutcome {
        text:
            "Integration request cancelled; an already dispatched external effect may be uncertain."
                .into(),
        is_error: true,
        interrupted: true,
        presentation: None,
    }
}
