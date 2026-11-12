//! Small, stable tool schemas; skill bodies enter context only when requested.
use std::fmt::Write as _;
use std::sync::Arc;

use makedo_tools::registry::{Registry, RegistryError};
use makedo_tools::{Tool, ToolCtx, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::Skill;

pub fn register(registry: &mut Registry, skills: Vec<Skill>) -> Result<(), RegistryError> {
    let skills = Arc::new(skills);
    registry.register(Search(Arc::clone(&skills)))?;
    registry.register(Load(skills))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Keywords describing the task, or part of a skill name.
    pub query: String,
}

#[derive(Debug)]
struct Search(Arc<Vec<Skill>>);

impl Tool for Search {
    type Args = SearchArgs;
    fn name(&self) -> &'static str {
        "skill_search"
    }
    fn description(&self) -> &'static str {
        "Search embedded and project skills by name/description. Returns up to 8 matches, without loading their instructions. Use skill_load with a returned name."
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn run(&self, _ctx: &ToolCtx, args: SearchArgs) -> Result<String, ToolError> {
        let query = args.query.to_lowercase();
        let words: Vec<_> = query.split_whitespace().collect();
        if words.is_empty() {
            return Err(ToolError::InvalidArgs("query must not be empty".into()));
        }
        let mut hits: Vec<_> = self
            .0
            .iter()
            .filter_map(|skill| {
                let name = skill.name.to_lowercase();
                let description = skill.description.to_lowercase();
                let score: usize = words
                    .iter()
                    .map(|word| {
                        usize::from(name.contains(word)) * 4
                            + usize::from(description.contains(word))
                    })
                    .sum();
                (score > 0).then_some((score, skill))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        let mut out = format!("{} matches ({} available):\n", hits.len(), self.0.len());
        for (_, skill) in hits.into_iter().take(8) {
            let description: String = skill.description.chars().take(500).collect();
            let _ = writeln!(out, "{}: {description}", skill.name);
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadArgs {
    pub name: String,
}

#[derive(Debug)]
struct Load(Arc<Vec<Skill>>);

impl Tool for Load {
    type Args = LoadArgs;
    fn name(&self) -> &'static str {
        "skill_load"
    }
    fn description(&self) -> &'static str {
        "Load the instructions of one available skill. Follow its instructions and resolve relative references from its source file."
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn run(&self, _ctx: &ToolCtx, args: LoadArgs) -> Result<String, ToolError> {
        let skill = self.0.iter().find(|s| s.name == args.name).ok_or_else(|| {
            ToolError::InvalidArgs(format!("unknown skill {}; use skill_search", args.name))
        })?;
        let source = skill
            .source
            .as_ref()
            .map_or_else(|| "embedded in MAKEDO".into(), |p| p.display().to_string());
        Ok(format!(
            "Skill: {}\nSource: {source}\n\n{}",
            skill.name, skill.body
        ))
    }
}
