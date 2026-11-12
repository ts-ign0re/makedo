//! Встроенные тулы M0. `task`/`lsp`/`websearch` — отложены (PLAN §6).

mod file_store;
pub mod fs;
pub mod patch;
pub mod search;
pub mod shell;
pub mod todo;

use crate::registry::{Registry, RegistryError};

/// Регистрирует все встроенные тулы M0.
pub fn register_all(r: &mut Registry) -> Result<(), RegistryError> {
    r.register(fs::ReadTool)?;
    r.register(fs::WriteTool)?;
    r.register(fs::EditTool)?;
    r.register(patch::ApplyPatchTool)?;
    r.register(search::GrepTool)?;
    r.register(search::GlobTool)?;
    r.register(shell::BashTool)?;
    r.register(todo::TodoTool)?;
    Ok(())
}
