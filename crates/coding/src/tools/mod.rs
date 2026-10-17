mod bash;
mod edit;
mod read;
mod write;

use std::sync::Arc;

pub use bash::BashTool;
pub use edit::EditTool;
pub use read::ReadTool;
pub use write::WriteTool;

use agent::Tool;

pub const NAMES: [&str; 4] = ["read", "write", "edit", "bash"];

pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadTool),
        Arc::new(WriteTool),
        Arc::new(EditTool),
        Arc::new(BashTool),
    ]
}

/// The four tools, optionally restricted to a subset (`--tools read,bash`).
pub fn specified(names: &[String]) -> Result<Vec<Arc<dyn Tool>>, String> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for name in names {
        match name.as_str() {
            "read" => tools.push(Arc::new(ReadTool)),
            "write" => tools.push(Arc::new(WriteTool)),
            "edit" => tools.push(Arc::new(EditTool)),
            "bash" => tools.push(Arc::new(BashTool)),
            other => {
                return Err(format!(
                    "unknown tool: {other} (available: {})",
                    NAMES.join(", ")
                ))
            }
        }
    }
    Ok(tools)
}
