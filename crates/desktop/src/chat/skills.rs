//! The skill catalogs the chat keeps: the user's, usable in every chat, and
//! each project's, with its `AGENTS.md`. Requests and tools read them from
//! here instead of the disk.
//!
//! Scans run on worker threads and read only frontmatter: at startup, when a
//! chat's folder is picked or a session in an unscanned folder is opened,
//! when the window regains focus (files may have changed elsewhere), after
//! the agent writes files or runs commands in a project, and when settings
//! open. A newer scan always wins over an older one still in flight. A
//! request that goes out before its catalogs arrive scans them on its own
//! worker ([`Chat::skills_scanned`] then caches the result).

use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serechat::ToolCall;

use super::{Chat, Page};
use crate::app::Action;
use crate::skills::{self, Catalog, Skill};

/// Least time between rescans caused by the window regaining focus.
const REFRESH_EVERY: Duration = Duration::from_secs(3);

/// Catalogs by location: `None` for the user's, otherwise a project folder.
#[derive(Default)]
pub(super) struct SkillCache {
    catalogs: HashMap<Option<String>, Arc<Catalog>>,
    /// The latest scan asked for, per location whose result is still due.
    pending: HashMap<Option<String>, u64>,
    next: u64,
    /// When the catalogs were last rescanned on focus.
    refreshed: Option<Instant>,
}

impl Chat {
    /// Scans `project`'s skills (`None`: the user's) on a worker.
    fn scan_skills(&mut self, project: Option<String>, actions: &mut Vec<Action>) {
        self.skills.next += 1;
        let generation = self.skills.next;
        self.skills.pending.insert(project.clone(), generation);
        actions.push(Action::ScanSkills { project, generation });
    }

    /// Scans the user's skills and `project`'s unless they are known or on their way.
    pub(super) fn ensure_skills(&mut self, project: Option<&str>, actions: &mut Vec<Action>) {
        for key in [None, project.map(str::to_owned)] {
            if !self.skills.catalogs.contains_key(&key) && !self.skills.pending.contains_key(&key) {
                self.scan_skills(key, actions);
            }
        }
    }

    /// Rescans the user's skills, the open chat's project and every project
    /// already scanned, as the files may have changed. Without `force`, at
    /// most every few seconds.
    pub fn refresh_skills(&mut self, force: bool, actions: &mut Vec<Action>) {
        let now = Instant::now();
        if !force && self.skills.refreshed.is_some_and(|at| now.duration_since(at) < REFRESH_EVERY) {
            return;
        }
        self.skills.refreshed = Some(now);
        let mut keys: Vec<Option<String>> = self.skills.catalogs.keys().cloned().collect();
        keys.extend([None, self.current().project.clone()]);
        keys.sort();
        keys.dedup();
        for key in keys {
            self.scan_skills(key, actions);
        }
    }

    /// Rescans a project after a tool call that may have changed its skills
    /// or `AGENTS.md`: a write into them, or any command.
    pub(super) fn after_tool(&mut self, project: Option<&str>, call: &ToolCall, actions: &mut Vec<Action>) {
        if let Some(project) = project
            && touches_context(call)
        {
            self.scan_skills(Some(project.to_owned()), actions);
        }
    }

    /// Stores a catalog scanned on a worker. `generation` is `None` for a
    /// scan a request made on its own, which only fills a gap.
    pub fn skills_scanned(&mut self, project: Option<String>, generation: Option<u64>, catalog: Arc<Catalog>) {
        let wanted = match generation {
            Some(generation) => self.skills.pending.get(&project) == Some(&generation),
            None => !self.skills.catalogs.contains_key(&project),
        };
        if !wanted {
            return;
        }
        if generation.is_some() {
            self.skills.pending.remove(&project);
        }
        self.skills.catalogs.insert(project, catalog);
        if self.page == Page::Settings {
            self.show_skills();
        }
    }

    /// The user's and `project`'s catalogs, as far as they are known.
    pub(super) fn skill_catalogs(&self, project: Option<&str>) -> (Option<Arc<Catalog>>, Option<Arc<Catalog>>) {
        let project = project.and_then(|p| self.skills.catalogs.get(&Some(p.to_owned())).cloned());
        (self.skills.catalogs.get(&None).cloned(), project)
    }

    /// The skills a chat in `project` can use.
    pub(super) fn skills_for(&self, project: Option<&str>) -> Arc<[Skill]> {
        let (user, project) = self.skill_catalogs(project);
        skills::merge(project.as_deref(), user.as_deref()).0.into()
    }

    /// Hands the settings page the skills the open chat can use and the
    /// problems found.
    pub(super) fn show_skills(&mut self) {
        let project = self.current().project.clone();
        let (user, catalog) = self.skill_catalogs(project.as_deref());
        let known = user.is_some() && (project.is_none() || catalog.is_some());
        let shown = known.then(|| {
            let (list, hidden) = skills::merge(catalog.as_deref(), user.as_deref());
            let warnings = catalog.iter().chain(&user).flat_map(|c| c.warnings.iter().cloned()).chain(hidden).collect();
            Catalog { skills: list, warnings, ..Catalog::default() }
        });
        self.settings.set_skills(shown);
    }
}

/// Whether `call` may have changed a project's skills or `AGENTS.md`.
fn touches_context(call: &ToolCall) -> bool {
    match call.name.as_str() {
        "run_command" | "start_process" => true,
        "write_file" | "edit_file" => {
            let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
            let path = Path::new(args.get("path").and_then(serde_json::Value::as_str).unwrap_or_default());
            path.file_name().is_some_and(|n| n == "AGENTS.md") || path.components().any(|c| c == Component::Normal(".agents".as_ref()))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Reasoning;

    fn catalog(names: &[&str]) -> Arc<Catalog> {
        let skill = |name: &&str| Skill { name: (*name).into(), description: "d".into(), path: "x".into(), scope: skills::Scope::User };
        Arc::new(Catalog { skills: names.iter().map(skill).collect(), ..Catalog::default() })
    }

    fn scans(actions: &[Action]) -> Vec<(Option<String>, Option<u64>)> {
        actions.iter().filter_map(|a| if let Action::ScanSkills { project, generation } = a { Some((project.clone(), Some(*generation))) } else { None }).collect()
    }

    #[test]
    fn catalogs_are_scanned_once_and_newest_wins() {
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), None);
        let mut actions = Vec::new();
        chat.ensure_skills(Some("/p"), &mut actions);
        chat.ensure_skills(Some("/p"), &mut actions);
        let asked = scans(&actions);
        assert_eq!(asked.len(), 2, "the user's and the project's, once each");

        // A second scan of the project is asked for before the first returns.
        let mut again = Vec::new();
        chat.scan_skills(Some("/p".into()), &mut again);
        let newer = scans(&again)[0].1;
        chat.skills_scanned(Some("/p".into()), asked[1].1, catalog(&["old"]));
        assert!(chat.skill_catalogs(Some("/p")).1.is_none(), "an older scan never lands");
        chat.skills_scanned(Some("/p".into()), newer, catalog(&["new"]));
        chat.skills_scanned(None, asked[0].1, catalog(&["mine", "new"]));
        assert_eq!(chat.skills_for(Some("/p")).iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["new", "mine"]);
        assert_eq!(chat.skills_for(None).len(), 2, "the user's skills work in every chat");

        // A request's own scan only fills gaps.
        chat.skills_scanned(Some("/p".into()), None, catalog(&[]));
        assert_eq!(chat.skills_for(Some("/p")).len(), 2);
        chat.skills_scanned(Some("/q".into()), None, catalog(&["q"]));
        assert!(chat.skills_for(Some("/q")).iter().any(|s| s.name == "q"));

        let mut actions = Vec::new();
        chat.refresh_skills(false, &mut actions);
        chat.refresh_skills(false, &mut actions);
        // Once for every known location (the user's, /p and /q), then not again so soon.
        assert_eq!(scans(&actions).len(), 3, "focus rescans are spaced out");
    }

    #[test]
    fn project_changes_are_noticed() {
        let call = |name: &str, path: &str| ToolCall { call_id: "c".into(), name: name.into(), arguments: serde_json::json!({ "path": path }).to_string() };
        assert!(touches_context(&call("write_file", "AGENTS.md")));
        assert!(touches_context(&call("edit_file", ".agents/skills/x/SKILL.md")));
        assert!(touches_context(&call("run_command", "")));
        assert!(!touches_context(&call("write_file", "src/agents.rs")));
        assert!(!touches_context(&call("read_file", "AGENTS.md")));
    }
}
