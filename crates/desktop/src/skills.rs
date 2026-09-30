//! [Agent Skills](https://agentskills.io) and a project's `AGENTS.md`.
//!
//! Skills live in `.agents/skills/<name>/SKILL.md`: the user's in the home
//! folder, usable in every chat, and a project's in its folder, where they
//! hide a user skill of the same name. Following the standard's progressive
//! disclosure, the model sees each skill's name and description up front
//! ([`prompt`]), loads its instructions with the `use_skill` tool when a task
//! calls for it ([`run`]), and reads the files it bundles (references,
//! scripts, assets) through the same tool, one at a time.
//!
//! A [`Catalog`] is what one scan of a location finds. Scanning reads only
//! each skill's frontmatter, so it is cheap; the chat screen keeps catalogs
//! and scans again when a folder is picked or files may have changed.
//! Parsing is lenient, as the standard recommends for skills written for
//! other clients: problems become [`Catalog::warnings`], and a skill is only
//! skipped when it has no description.

use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::tools;

/// Most of a `SKILL.md` read while scanning; its frontmatter must fit.
const MAX_FRONTMATTER: u64 = 64 * 1024;
/// Longest `SKILL.md` read when a skill is loaded.
const MAX_SKILL_FILE: u64 = 256 * 1024;
/// Most skill folders read per location.
const MAX_SKILLS: usize = 200;
/// Longest `AGENTS.md` included in the prompt, in bytes.
const MAX_AGENTS_MD: u64 = 64 * 1024;
/// Most bundled files listed when a skill is loaded.
const MAX_RESOURCES: usize = 50;
/// Longest description the catalog shows, in characters (the standard's limit).
const MAX_DESCRIPTION: usize = 1024;

/// Where a skill was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `.agents/skills` in the project.
    Project,
    /// `~/.agents/skills`.
    User,
}

impl Scope {
    /// Name shown in settings.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::User => "User",
        }
    }
}

/// A skill's catalog entry; its instructions are read when it is loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// Name the model loads it by.
    pub name: String,
    /// What it does and when to use it.
    pub description: String,
    /// Its `SKILL.md`.
    pub path: PathBuf,
    /// Where it was found.
    pub scope: Scope,
}

/// What one scan found: a location's skills and, for a project, its `AGENTS.md`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Usable skills, each name once.
    pub skills: Vec<Skill>,
    /// Problems found while reading them.
    pub warnings: Vec<String>,
    /// The project's `AGENTS.md` (cut at [`MAX_AGENTS_MD`]); `None` for the
    /// user's catalog or a project without one.
    pub agents_md: Option<String>,
}

/// The user's skills, in `~/.agents/skills`. Reads files: call it off the
/// UI thread.
#[must_use]
pub fn scan_user() -> Catalog {
    let mut catalog = Catalog::default();
    if let Some(home) = std::env::home_dir() {
        scan(&home.join(".agents").join("skills"), Scope::User, &mut catalog);
    }
    catalog
}

/// The skills and `AGENTS.md` of the project in `root`. Reads files: call
/// it off the UI thread.
#[must_use]
pub fn scan_project(root: &Path) -> Catalog {
    let mut catalog = Catalog { agents_md: read_agents_md(&root.join("AGENTS.md")), ..Catalog::default() };
    scan(&root.join(".agents").join("skills"), Scope::Project, &mut catalog);
    catalog
}

/// The text of an `AGENTS.md`, cut at [`MAX_AGENTS_MD`]; `None` if missing or empty.
fn read_agents_md(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take(MAX_AGENTS_MD + 1).read_to_end(&mut bytes).ok()?;
    let cut = bytes.len() as u64 > MAX_AGENTS_MD;
    bytes.truncate(usize::try_from(MAX_AGENTS_MD).unwrap_or(usize::MAX));
    let mut text = String::from_utf8_lossy(&bytes).trim().to_owned();
    if cut {
        text.push_str("\n[… the rest of AGENTS.md was left out]");
    }
    (!text.is_empty()).then_some(text)
}

/// Reads the skill folders in `dir` into `catalog`.
fn scan(dir: &Path, scope: Scope, catalog: &mut Catalog) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut folders: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.join("SKILL.md").is_file()).collect();
    folders.sort();
    if folders.len() > MAX_SKILLS {
        catalog.warnings.push(format!("{}: only the first {MAX_SKILLS} skills are read.", dir.display()));
    }
    for folder in folders.into_iter().take(MAX_SKILLS) {
        let path = folder.join("SKILL.md");
        let shown = tools::relative(dir, &path);
        match load(&path, &folder, scope) {
            Err(e) => catalog.warnings.push(format!("{shown}: {e}; skipped.")),
            Ok((skill, warnings)) => {
                catalog.warnings.extend(warnings.into_iter().map(|w| format!("{shown}: {w}.")));
                if catalog.skills.iter().any(|s| s.name == skill.name) {
                    catalog.warnings.push(format!("{shown}: another skill is already named '{}'; skipped.", skill.name));
                } else {
                    catalog.skills.push(skill);
                }
            }
        }
    }
}

/// Reads one skill's frontmatter; returns it with the problems worth mentioning.
fn load(path: &Path, folder: &Path, scope: Scope) -> Result<(Skill, Vec<String>), String> {
    let mut bytes = Vec::new();
    fs::File::open(path).and_then(|f| f.take(MAX_FRONTMATTER).read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let (fields, _) = frontmatter(&text).ok_or("no frontmatter (a `---` block with name and description at the top)")?;
    let field = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim().to_owned()).filter(|v| !v.is_empty());
    let description = field("description").ok_or("no description")?;
    let folder_name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut warnings = Vec::new();
    let name = field("name").unwrap_or_else(|| {
        warnings.push("no name; using the folder's".to_owned());
        folder_name.clone()
    });
    if name != folder_name {
        warnings.push(format!("name '{name}' differs from its folder '{folder_name}'"));
    }
    if !valid_name(&name) {
        warnings.push(format!("name '{name}' should be 1-64 lowercase letters, digits and single hyphens"));
    }
    if description.chars().count() > MAX_DESCRIPTION {
        warnings.push(format!("description longer than {MAX_DESCRIPTION} characters is cut"));
    }
    let description = description.chars().take(MAX_DESCRIPTION).collect();
    Ok((Skill { name, description, path: path.to_owned(), scope }, warnings))
}

/// Whether `name` follows the standard: 1-64 of `a-z`, `0-9` and `-`, with
/// no hyphen at either end or twice in a row.
fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// The top-level fields and the body of a `SKILL.md`. Understands the YAML
/// that skills use: plain, quoted and block (`|`, `>`) scalars, values over
/// several lines, and comments. Nested maps such as `metadata` are kept as
/// flattened text, which nothing reads. `None` without a `---` block.
fn frontmatter(text: &str) -> Option<(Vec<(String, String)>, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut offset = first.len();
    let mut yaml = Vec::new();
    let mut closed = false;
    for line in lines {
        offset += line.len();
        if matches!(line.trim_end(), "---" | "...") {
            closed = true;
            break;
        }
        yaml.push(line.trim_end_matches(['\n', '\r']));
    }
    if !closed {
        return None;
    }
    let mut fields = Vec::new();
    let mut i = 0;
    while i < yaml.len() {
        let line = yaml[i];
        i += 1;
        if line.starts_with([' ', '\t']) || line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        // Indented and blank lines that follow belong to this key.
        let start = i;
        while i < yaml.len() && (yaml[i].starts_with([' ', '\t']) || yaml[i].trim().is_empty()) {
            i += 1;
        }
        fields.push((key.trim().to_owned(), scalar(value.trim(), &yaml[start..i])));
    }
    Some((fields, &text[offset..]))
}

/// A YAML scalar that starts as `value` and continues on the `more` lines.
fn scalar(value: &str, more: &[&str]) -> String {
    match value.chars().next() {
        Some(style @ ('|' | '>')) => {
            let indent = more.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
            let lines: Vec<&str> = more.iter().map(|l| l.get(indent..).unwrap_or("").trim_end()).collect();
            let text = if style == '|' {
                lines.join("\n")
            } else {
                // Folded: lines join with spaces, blank lines are breaks.
                lines.split(|l| l.is_empty()).map(|paragraph| paragraph.join(" ")).collect::<Vec<_>>().join("\n")
            };
            text.trim().to_owned()
        }
        Some(quote @ ('"' | '\'')) => {
            let joined = std::iter::once(value).chain(more.iter().map(|l| l.trim())).collect::<Vec<_>>().join(" ");
            let mut out = String::new();
            let mut chars = joined.chars().skip(1).peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\'' if quote == '\'' && chars.next_if_eq(&'\'').is_some() => out.push('\''),
                    c if c == quote => break,
                    '\\' if quote == '"' => match chars.next() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some(other) => out.push(other),
                        None => {}
                    },
                    c => out.push(c),
                }
            }
            out
        }
        _ => {
            // Plain: continues on indented lines; ` #` starts a comment.
            let joined = std::iter::once(value).chain(more.iter().map(|l| l.trim())).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
            joined.split(" #").next().unwrap_or_default().trim().to_owned()
        }
    }
}

/// The skills a chat can use: the project's, then the user's that none of
/// them hides, with a note for each hidden one.
#[must_use]
pub fn merge(project: Option<&Catalog>, user: Option<&Catalog>) -> (Vec<Skill>, Vec<String>) {
    let mut skills: Vec<Skill> = project.map(|c| c.skills.clone()).unwrap_or_default();
    let mut hidden = Vec::new();
    for skill in user.into_iter().flat_map(|c| &c.skills) {
        if skills.iter().any(|s| s.name == skill.name) {
            hidden.push(format!("Your skill '{}' is hidden by the project's skill of the same name.", skill.name));
        } else {
            skills.push(skill.clone());
        }
    }
    (skills, hidden)
}

/// The system prompt sections for a chat: the project's `AGENTS.md` and the
/// catalog of `skills`. Empty when there is neither.
#[must_use]
pub fn prompt(agents_md: Option<&str>, skills: &[Skill]) -> String {
    let mut out = String::new();
    if let Some(text) = agents_md {
        let _ = write!(
            out,
            "\n\n## Project instructions\n\nThe project's AGENTS.md, which you must follow:\n\n<agents_md>\n{text}\n</agents_md>\n\n\
             Folders in the project may have an AGENTS.md of their own with rules for their files; read it before changing files there."
        );
    }
    if !skills.is_empty() {
        out.push_str(
            "\n\n## Skills\n\nSkills provide specialised instructions for specific tasks. When a task matches a skill's description, call \
             use_skill with its name to load its instructions before you start, then follow them. Load only the skills the task needs.\n\n\
             <available_skills>\n",
        );
        for skill in skills {
            let _ = writeln!(out, "<skill>\n<name>{}</name>\n<description>{}</description>\n</skill>", escape(&skill.name), escape(&skill.description));
        }
        out.push_str("</available_skills>");
    }
    out
}

/// Escapes text for the prompt's XML-like tags.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The `use_skill` tool's description and parameters, when there are skills.
#[must_use]
pub fn tool(skills: &[Skill]) -> Option<(&'static str, Value)> {
    if skills.is_empty() {
        return None;
    }
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    let parameters = json!({ "type": "object", "properties": {
        "name": { "type": "string", "enum": names, "description": "The skill." },
        "file": { "type": "string", "description": "A file the skill bundles, relative to its folder (e.g. references/REFERENCE.md). Leave out to load its instructions." } },
        "required": ["name"] });
    Some(("Load a skill's instructions (see Skills), or with `file`, one of the files it bundles.", parameters))
}

/// Runs a `use_skill` call with JSON `arguments` against `skills`: the
/// named skill's instructions, or one of its files.
///
/// # Errors
/// Bad arguments, an unknown skill, or a file that is missing or outside
/// the skill's folder.
pub fn run(skills: &[Skill], arguments: &str) -> Result<String, String> {
    let args: Value = serde_json::from_str(arguments).map_err(|_| "Arguments must be a JSON object.".to_owned())?;
    let name = args.get("name").and_then(Value::as_str).ok_or("Missing the 'name' argument.")?;
    let skill = skills.iter().find(|s| s.name == name).ok_or_else(|| {
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        format!("There is no skill named '{name}'. The skills are: {}.", names.join(", "))
    })?;
    let dir = skill.path.parent().ok_or("The skill has no folder.")?;
    if let Some(file) = args.get("file").and_then(Value::as_str) {
        return tools::read_file(&tools::resolve(dir, file)?, 1, 2000);
    }
    let mut bytes = Vec::new();
    fs::File::open(&skill.path)
        .and_then(|f| f.take(MAX_SKILL_FILE).read_to_end(&mut bytes))
        .map_err(|e| format!("The skill could not be read: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let body = frontmatter(&text).map_or(&*text, |(_, body)| body).trim();
    let mut resources = Vec::new();
    tools::walk(dir, 3, |path, is_dir, _| {
        if !is_dir && path != skill.path {
            resources.push(format!("  <file>{}</file>", escape(&tools::relative(dir, path))));
        }
        resources.len() < MAX_RESOURCES
    });
    let listing = if resources.is_empty() { String::new() } else { format!("\n<skill_resources>\n{}\n</skill_resources>", resources.join("\n")) };
    Ok(format!(
        "<skill_content name=\"{}\">\n{body}\n\nSkill directory: {}\nRelative paths in this skill are relative to the skill directory. Read its \
         files with use_skill and `file`; run its scripts with run_command using their full path.{listing}\n</skill_content>",
        escape(name),
        dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_like_skills_write_it() {
        let text = "---\nname: pdf-processing\ndescription: Use this skill when: the user asks about PDFs # a comment\nlicense: 'It''s MIT'\n\
                    metadata:\n  author: me\ncompatibility: >\n  Needs\n  python\n\n  and uv\n---\n# PDF\nBody";
        let (fields, body) = frontmatter(text).unwrap();
        let get = |k: &str| fields.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("name"), Some("pdf-processing"));
        assert_eq!(get("description"), Some("Use this skill when: the user asks about PDFs"), "colons in values are fine");
        assert_eq!(get("license"), Some("It's MIT"));
        assert_eq!(get("compatibility"), Some("Needs python\nand uv"));
        assert_eq!(body, "# PDF\nBody");

        let multi = "---\r\ndescription: \"Line one\n  and two\"\r\nname: x\r\n---\r\n";
        let (fields, body) = frontmatter(multi).unwrap();
        assert_eq!(fields[0].1, "Line one and two");
        assert_eq!(body, "");
        assert!(frontmatter("no frontmatter").is_none());
        assert!(frontmatter("---\nname: open").is_none(), "an unclosed block is not frontmatter");
        assert!(frontmatter("---").is_none());
    }

    #[test]
    fn names() {
        assert!(valid_name("pdf-processing") && valid_name("a1"));
        for bad in ["", "PDF", "-pdf", "pdf-", "pdf--x", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }


    #[test]
    fn scanning_merging_and_loading() {
        let root = std::env::temp_dir().join(format!("serechat-skills-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let skills = root.join(".agents").join("skills");
        fs::create_dir_all(skills.join("review").join("references")).unwrap();
        fs::write(skills.join("review").join("SKILL.md"), "---\nname: review\ndescription: Review <code>.\n---\nCheck everything.").unwrap();
        fs::write(skills.join("review").join("references").join("GUIDE.md"), "The guide.").unwrap();
        fs::create_dir_all(skills.join("broken")).unwrap();
        fs::write(skills.join("broken").join("SKILL.md"), "---\nname: broken\n---\n").unwrap();
        fs::create_dir_all(skills.join("Odd")).unwrap();
        fs::write(skills.join("Odd").join("SKILL.md"), "---\ndescription: Odd one.\n---\n").unwrap();
        fs::write(root.join("AGENTS.md"), "Use tabs.\n").unwrap();

        let project = scan_project(&root);
        assert_eq!(project.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Odd", "review"]);
        assert!(project.skills.iter().all(|s| s.scope == Scope::Project));
        assert_eq!(project.agents_md.as_deref(), Some("Use tabs."));
        assert!(project.warnings.iter().any(|w| w.contains("broken") && w.contains("no description")));
        assert!(project.warnings.iter().any(|w| w.contains("Odd") && w.contains("lowercase")));

        // A user skill of the same name is hidden; others join.
        let user = |name: &str| Skill { name: name.into(), description: "Mine.".into(), path: PathBuf::from("x"), scope: Scope::User };
        let mine = Catalog { skills: vec![user("review"), user("notes")], ..Catalog::default() };
        let (merged, hidden) = merge(Some(&project), Some(&mine));
        assert_eq!(merged.iter().map(|s| (s.name.as_str(), s.scope)).collect::<Vec<_>>(), [
            ("Odd", Scope::Project),
            ("review", Scope::Project),
            ("notes", Scope::User)
        ]);
        assert_eq!(hidden.len(), 1);
        assert_eq!(merge(None, Some(&mine)).0.len(), 2, "user skills work without a project");

        let text = prompt(project.agents_md.as_deref(), &merged);
        assert!(text.contains("Use tabs.") && text.contains("<name>review</name>") && text.contains("Review &lt;code&gt;."));
        assert!(!text.contains("Check everything."), "instructions load only when asked for");
        assert!(prompt(None, &[]).is_empty() && tool(&[]).is_none());
        assert!(tool(&merged).is_some_and(|(_, p)| p["properties"]["name"]["enum"].as_array().is_some_and(|e| e.len() == 3)));

        let loaded = run(&merged, r#"{"name":"review"}"#).unwrap();
        assert!(loaded.starts_with("<skill_content name=\"review\">\nCheck everything."));
        assert!(loaded.contains("<file>references/GUIDE.md</file>") && !loaded.contains("SKILL.md</file>"));
        assert_eq!(run(&merged, r#"{"name":"review","file":"references/GUIDE.md"}"#).unwrap(), "The guide.");
        assert!(run(&merged, r#"{"name":"review","file":"../../../AGENTS.md"}"#).is_err(), "files stay inside the skill");
        assert!(run(&merged, r#"{"name":"nope"}"#).unwrap_err().contains("review"));
        assert!(run(&merged, "{").is_err() && run(&merged, "{}").is_err());

        // Frontmatter past the scan limit is not read.
        fs::create_dir_all(skills.join("huge")).unwrap();
        let padding = "x".repeat(usize::try_from(MAX_FRONTMATTER).unwrap());
        fs::write(skills.join("huge").join("SKILL.md"), format!("---\nname: huge\nnote: {padding}\ndescription: d\n---\n")).unwrap();
        assert!(scan_project(&root).warnings.iter().any(|w| w.contains("huge") && w.contains("no frontmatter")));
        fs::remove_dir_all(&root).unwrap();
    }
}
