//! `todo_write`: модель ведёт план сессии (полная замена списка — стиль
//! Claude Code/opencode). Session перехватывает результат по имени тула и
//! эмитит `TodoUpdated` для inline-панели TUI.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Tool, ToolCtx, ToolError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TodoItem {
    /// Short imperative task description
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoArgs {
    /// The FULL updated list (replaces the previous one entirely)
    pub todos: Vec<TodoItem>,
}

#[derive(Debug)]
pub struct TodoTool;

pub const TODO_TOOL_NAME: &str = "todo_write";
pub const MAX_TODOS: usize = 30;

impl Tool for TodoTool {
    type Args = TodoArgs;

    fn name(&self) -> &'static str {
        TODO_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Maintain the session task list. Pass the FULL list every time (it replaces \
         the previous one). Use for multi-step work: mark the current step in_progress, \
         completed steps completed. Keep at most one in_progress."
    }

    fn is_read_only(&self) -> bool {
        true // не трогает файлы; permissions не нужны
    }

    async fn run(&self, _ctx: &ToolCtx, args: TodoArgs) -> Result<String, ToolError> {
        if args.todos.len() > MAX_TODOS {
            return Err(ToolError::InvalidArgs(format!(
                "too many todos ({}); keep the list under {MAX_TODOS}",
                args.todos.len()
            )));
        }
        let done = args
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        Ok(format!(
            "Todo list updated: {}/{} done",
            done,
            args.todos.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn counts_progress_and_caps_size() {
        let ctx = ToolCtx::new(std::env::temp_dir(), std::env::temp_dir());
        let out = TodoTool
            .run(
                &ctx,
                TodoArgs {
                    todos: vec![
                        TodoItem {
                            content: "a".into(),
                            status: TodoStatus::Completed,
                        },
                        TodoItem {
                            content: "b".into(),
                            status: TodoStatus::InProgress,
                        },
                    ],
                },
            )
            .await
            .unwrap();
        assert_eq!(out, "Todo list updated: 1/2 done");
        let too_many = (0..40)
            .map(|i| TodoItem {
                content: format!("t{i}"),
                status: TodoStatus::Pending,
            })
            .collect();
        assert!(
            TodoTool
                .run(&ctx, TodoArgs { todos: too_many })
                .await
                .is_err()
        );
    }
}
