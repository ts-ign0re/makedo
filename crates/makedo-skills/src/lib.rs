//! Skills: claude-совместимый формат `SKILL.md` (frontmatter + markdown).
//! Built-ins live in the binary; project-local `.makedo/skills` may override by name.
//! Only always-active instructions enter the system prompt; other bodies load on demand.

pub mod tools;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use makedo_config::{Doctrine, PonytailLevel};

/// Ponytail вшит в бинарь (MIT, THIRD-PARTY-NOTICES.md) и активен по умолчанию.
const PONYTAIL: &str = include_str!("../embedded/ponytail/SKILL.md");
/// Доктрины проектирования — вшиты в бинарь, активируются `[skills] doctrine`.
const DOCTRINE_DDD: &str = include_str!("../embedded/doctrine-ddd/SKILL.md");
const DOCTRINE_HEXAGONAL: &str = include_str!("../embedded/doctrine-hexagonal/SKILL.md");

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: Option<PathBuf>,
    pub active: bool,
}

fn parse(text: &str) -> Option<Skill> {
    let (fields, body) = makedo_types::frontmatter(text)?;
    Some(Skill {
        name: fields.get("name")?.clone(),
        description: fields.get("description").cloned().unwrap_or_default(),
        body: body.to_owned(),
        source: None,
        active: false,
    })
}

fn load_dir(dir: &Path, skills: &mut BTreeMap<String, Skill>) {
    fn walk(
        dir: &Path,
        depth: usize,
        seen: &mut BTreeSet<PathBuf>,
        skills: &mut BTreeMap<String, Skill>,
    ) {
        let Ok(real) = dir.canonicalize() else { return };
        if !seen.insert(real) {
            return;
        }
        let source = dir.join("SKILL.md");
        if source.metadata().is_ok_and(|m| m.len() <= 128 * 1024) {
            if let Ok(text) = std::fs::read_to_string(&source)
                && let Some(mut skill) = parse(&text.replace("\r\n", "\n"))
            {
                skill.source = Some(source);
                skills.insert(skill.name.clone(), skill);
            }
            return;
        }
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        paths.sort();
        for path in paths {
            walk(&path, depth - 1, seen, skills);
        }
    }
    walk(dir, 8, &mut BTreeSet::new(), skills);
}

/// Каталог embedded и проектных скиллов (проектные побеждают по имени).
#[must_use]
pub fn load(
    global_dir: Option<&Path>,
    project_root: &Path,
    ponytail: PonytailLevel,
    doctrine: Doctrine,
) -> Vec<Skill> {
    use std::fmt::Write as _;
    let mut skills = BTreeMap::new();
    if ponytail != PonytailLevel::Off
        && let Some(p) = parse(PONYTAIL)
    {
        skills.insert(p.name.clone(), p);
    }
    // доктрина проектирования (embedded), если выбрана
    let doctrine_md = match doctrine {
        Doctrine::None => None,
        Doctrine::Ddd => Some(DOCTRINE_DDD),
        Doctrine::Hexagonal => Some(DOCTRINE_HEXAGONAL),
    };
    if let Some(md) = doctrine_md
        && let Some(d) = parse(md)
    {
        skills.insert(d.name.clone(), d);
    }
    for md in [
        include_str!("../embedded/open-code-review/SKILL.md"),
        include_str!("../embedded/open-code-review-delegate/SKILL.md"),
        include_str!("../embedded/heavy/SKILL.md"),
        include_str!("../embedded/worktrees/SKILL.md"),
    ] {
        if let Some(skill) = parse(md) {
            skills.insert(skill.name.clone(), skill);
        }
    }
    let _ = global_dir; // Built-ins are self-contained; no reads from user-global skill directories.
    load_dir(&project_root.join(".makedo/skills"), &mut skills);
    if ponytail == PonytailLevel::Off {
        skills.remove("ponytail");
    } else if let Some(skill) = skills.get_mut("ponytail") {
        skill.active = true;
        let level = match ponytail {
            PonytailLevel::Lite => "lite",
            PonytailLevel::Ultra => "ultra",
            _ => "full",
        };
        let _ = write!(skill.body, "\n\nActive intensity level: **{level}**.\n");
    }
    if let Some(name) = doctrine.skill_name()
        && let Some(skill) = skills.get_mut(name)
    {
        skill.active = true;
    }
    skills.into_values().collect()
}

/// Секция system prompt из активных скиллов.
#[must_use]
pub fn render(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n# Active skills\n");
    for s in skills.iter().filter(|s| s.active) {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "\n<skill name=\"{}\">\n{}\n</skill>\n",
            s.name,
            s.body.trim()
        );
    }
    out.push_str("\nOther embedded skills are available through skill_search(query) and skill_load(name). Search for relevant skills before specialized work; load a matching skill before applying it. For code review, load open-code-review (or open-code-review-delegate when the user requests review without an external LLM). Load heavy for machine-wide resource limits and worktrees before creating, editing or finishing a worktree. Skill references resolve relative to the source SKILL.md returned by skill_load. Do not load the entire catalog.\n");
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn embedded_ponytail_parses() {
        let skills = load(
            None,
            Path::new("/nonexistent"),
            PonytailLevel::Full,
            Doctrine::None,
        );
        assert_eq!(skills.iter().filter(|s| s.active).count(), 1);
        let ponytail = skills.iter().find(|s| s.name == "ponytail").unwrap();
        assert!(ponytail.description.contains("laziest"));
        assert!(ponytail.body.contains("ladder") || ponytail.body.contains("Ladder"));
        let rendered = render(&skills);
        assert!(rendered.contains("<skill name=\"ponytail\">"));
    }

    #[test]
    fn off_disables_and_levels_annotate() {
        assert!(
            load(
                None,
                Path::new("/nonexistent"),
                PonytailLevel::Off,
                Doctrine::None
            )
            .iter()
            .all(|s| !s.active)
        );
        let ultra = load(
            None,
            Path::new("/nonexistent"),
            PonytailLevel::Ultra,
            Doctrine::None,
        );
        assert!(
            ultra
                .iter()
                .find(|s| s.name == "ponytail")
                .unwrap()
                .body
                .contains("**ultra**")
        );
    }

    #[test]
    fn project_overrides_by_name() {
        let d = std::env::temp_dir().join(format!("makedo-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let dir = d.join(".makedo/skills/ponytail");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: ponytail\ndescription: custom\n---\ncustom body\n",
        )
        .unwrap();
        let skills = load(None, &d, PonytailLevel::Full, Doctrine::None);
        assert_eq!(skills.iter().filter(|s| s.active).count(), 1);
        assert!(
            skills
                .iter()
                .find(|s| s.name == "ponytail")
                .unwrap()
                .body
                .starts_with("custom body")
        );
        // плюс новый скилл проекта
        let dir2 = d.join(".makedo/skills/deploy");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join("SKILL.md"), "---\nname: deploy\n---\nsteps\n").unwrap();
        let skills = load(None, &d, PonytailLevel::Full, Doctrine::None);
        assert_eq!(skills.len(), 6);
    }

    #[test]
    fn doctrine_ddd_activates_and_renders() {
        // ponytail + ddd = 2 скилла; ddd — про доменный язык и адаптивную строгость
        let skills = load(
            None,
            Path::new("/nonexistent"),
            PonytailLevel::Full,
            Doctrine::Ddd,
        );
        let ddd = skills.iter().find(|s| s.name == "doctrine-ddd").unwrap();
        assert!(ddd.body.contains("adaptive") || ddd.body.contains("Adaptive"));
        assert!(ddd.body.to_lowercase().contains("ubiquitous"));
        let rendered = render(&skills);
        assert!(rendered.contains("<skill name=\"doctrine-ddd\">"));
        // None — только ponytail
        let only = load(
            None,
            Path::new("/nonexistent"),
            PonytailLevel::Full,
            Doctrine::None,
        );
        assert!(only.iter().all(|s| s.name != "doctrine-ddd"));
        // hexagonal — про порты/адаптеры
        let hex = load(
            None,
            Path::new("/nonexistent"),
            PonytailLevel::Full,
            Doctrine::Hexagonal,
        );
        assert!(hex.iter().any(|s| s.name == "doctrine-hexagonal"));
    }
}

#[cfg(test)]
mod portable_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn catalog_is_embedded_and_only_active_bodies_enter_prompt() {
        let root =
            std::env::temp_dir().join(format!("makedo-portable-skills-{}", std::process::id()));
        let global = root.join(".makedo");
        let fake = global.join("skills/host-only");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(
            fake.join("SKILL.md"),
            "---\nname: host-only\n---\nHOST_SECRET\n",
        )
        .unwrap();
        let skills = load(
            Some(&global),
            &root.join("project"),
            PonytailLevel::Full,
            Doctrine::None,
        );
        assert!(
            skills
                .iter()
                .any(|s| s.name == "open-code-review" && s.body.contains("--audience agent"))
        );
        assert!(skills.iter().any(|s| s.name == "heavy"));
        assert!(skills.iter().all(|s| s.name != "host-only"));
        let prompt = render(&skills);
        assert!(!prompt.contains("# Open Code Review"));
        assert!(!prompt.contains("HOST_SECRET"));
        assert!(prompt.contains("skill_load"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
