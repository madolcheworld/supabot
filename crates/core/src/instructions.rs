//! Instructions: the text that makes a model behave like a teammate.
//!
//! Assembled in a fixed order — base rules, the bot's own persona, the team
//! roster, its groups, its memory index, the task section, the placement note —
//! and rendered **from the store**, so a roster change is reflected on the next
//! turn without anybody editing a prompt.
//!
//! One honesty rule governs the whole file: the instructions only ever describe
//! tools the bot actually has. They are rendered from the registry's visible
//! specs, never from a hardcoded list, so a bot on a node without `shell@1` is
//! not told it can run commands.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::conversation::ConversationKind;
use swarmbot_contracts::error::DomainError;
use swarmbot_contracts::id::BotId;
use swarmbot_contracts::repo::Persistence;
use swarmbot_contracts::task::Task;
use swarmbot_contracts::tool::ToolSpec;

use crate::session::MEMORY_INDEX_LIMIT_BYTES;

/// How much of a bot's persona is quoted in another bot's roster line.
const ROSTER_LINE_LIMIT: usize = 120;

/// How much of a skill's body is injected. A skill is a procedure, not a
/// document: past this the prompt spends its budget on one bot's playbook.
pub const SKILL_BODY_LIMIT_BYTES: usize = 4 * 1024;

/// How much of *all* its skills one bot may carry into a turn.
pub const SKILLS_TOTAL_LIMIT_BYTES: usize = 12 * 1024;

pub struct InstructionRenderer {
    store: Arc<dyn Persistence>,
    /// Where the skill library lives (`<data>/skills`). `None` in a test or a
    /// brain with no library, in which case a bot's skills are names only.
    skills_root: Option<PathBuf>,
}

/// What is being rendered, beyond the bot itself.
#[derive(Default)]
pub struct RenderOptions<'a> {
    /// Set when this session runs one task of the bot's own.
    pub task: Option<&'a Task>,
    /// The tools this bot can actually call in this session.
    pub tools: &'a [ToolSpec],
    /// `MEMORY.md`, already assembled; capped here.
    pub memory_index: Option<&'a str>,
    /// Where its tools run, in one or two lines.
    pub placement: Option<&'a str>,
    /// True when bot-to-bot messaging is switched off, so the instructions
    /// stop advertising it.
    pub messaging_enabled: bool,
}

impl InstructionRenderer {
    pub fn new(store: Arc<dyn Persistence>) -> Arc<Self> {
        Arc::new(Self {
            store,
            skills_root: None,
        })
    }

    /// A renderer that also resolves skills from a library directory.
    ///
    /// A skill is a directory with a `SKILL.md` in it
    /// (`<root>/<name>/SKILL.md`, or a flat `<root>/<name>.md`), and
    /// `Bot.skills` names the ones a bot has. Before this the field was a list
    /// of names nothing read, so "this bot knows rust" was a claim with no
    /// content behind it.
    pub fn with_skills_root(
        store: Arc<dyn Persistence>,
        skills_root: impl Into<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            skills_root: Some(skills_root.into()),
        })
    }

    pub fn render(&self, bot: &Bot, options: RenderOptions<'_>) -> Result<String, DomainError> {
        let has_tools = !options.tools.is_empty();
        let mut sections: Vec<String> = Vec::new();
        sections.push(base_rules_for_tools(options.messaging_enabled, has_tools));

        if !bot.instructions.trim().is_empty() {
            sections.push(format!("## You\n{}", bot.instructions.trim()));
        }

        if !bot.skills.is_empty() {
            sections.push(self.skills_section(&bot.skills));
        }

        sections.push(self.roster_section(bot, has_tools)?);

        if let Some(groups) = self.groups_section(bot, has_tools)? {
            sections.push(groups);
        }

        if has_tools {
            sections.push(tools_section(options.tools));
        }

        if let Some(memory) = options.memory_index {
            sections.push(memory_section(memory));
        }

        if let Some(task) = options.task {
            sections.push(task_section(task));
        }

        if let Some(placement) = options.placement {
            sections.push(format!("## Where your tools run\n{placement}"));
        }

        Ok(sections.join("\n\n"))
    }

    /// One line per teammate, so a bot knows who to collaborate with.
    fn roster_section(&self, bot: &Bot, has_tools: bool) -> Result<String, DomainError> {
        let bots = self.store.list_bots()?;
        if bots.len() <= 1 {
            return Ok(
                "## Your team\nYou are the only Bot right now. Do the work yourself and \
                       report to the owner."
                    .to_string(),
            );
        }
        let mut lines = Vec::new();
        for other in &bots {
            let you = other.id == bot.id;
            let summary = first_sentence(&other.instructions, ROSTER_LINE_LIMIT);
            let marker = if you { " ← you" } else { "" };
            let skills_str = if other.skills.is_empty() {
                String::new()
            } else {
                format!(" [skills: {}]", other.skills.join(", "))
            };
            lines.push(format!(
                "- `@{}` — {}{}{}{}",
                other.name,
                other.label,
                if summary.is_empty() {
                    String::new()
                } else {
                    format!(": {summary}")
                },
                skills_str,
                marker
            ));
        }
        let guidance = if has_tools {
            "Hand a piece of work to the teammate who owns it instead of doing it yourself. \
             `handoff_task` gives them the work and brings their result back to you; \
             `message_bot` is for a question or a note. In a group chat, you can also \
             address a teammate directly with `@name`."
        } else {
            "Collaborate with teammates who own a piece of work instead of doing everything \
             alone. In a group chat, address a teammate with `@name` when you need them to act \
             or answer."
        };
        Ok(format!("## Your team\n{}\n\n{guidance}", lines.join("\n")))
    }

    /// The groups this bot is in, with the ids `post_to_group` needs.
    fn groups_section(&self, bot: &Bot, has_tools: bool) -> Result<Option<String>, DomainError> {
        let mut lines = Vec::new();
        for conversation in self.store.list_conversations()? {
            let ConversationKind::Group { members, lead } = &conversation.kind else {
                continue;
            };
            if !members.contains(&bot.id) {
                continue;
            }
            let title = conversation
                .title
                .clone()
                .unwrap_or_else(|| "untitled".to_string());
            let names = self.member_names(members)?;
            let lead_name = self
                .store
                .get_bot(lead)?
                .map(|b| b.name)
                .unwrap_or_else(|| "?".to_string());
            lines.push(format!(
                "- \"{}\" (id `{}`, members: {}, lead: @{})",
                title,
                conversation.id,
                names.join(", "),
                lead_name
            ));
        }
        if lines.is_empty() {
            return Ok(None);
        }
        let guidance = if has_tools {
            "Work that belongs to one of these groups is coordinated there, wherever the request \
             reaches you — the owner's DM, a teammate's DM, a routine, a task result. Post into \
             the group with `post_to_group` and @mention whoever has to act, or reply with `@name` \
             when already in the group. Use `message_bot` only for something none of your groups \
             is about."
        } else {
            "Work that belongs to one of these groups is coordinated there. When replying in a \
             group, address teammates with `@name` when you need them to act or answer."
        };
        Ok(Some(format!(
            "## Your groups\n{}\n\n{guidance}",
            lines.join("\n")
        )))
    }

    fn member_names(&self, members: &[BotId]) -> Result<Vec<String>, DomainError> {
        let mut names = Vec::with_capacity(members.len());
        for id in members {
            if let Some(bot) = self.store.get_bot(id)? {
                names.push(format!("@{}", bot.name));
            }
        }
        Ok(names)
    }
}

/// The rules every bot gets, whatever its persona.
pub fn base_rules(messaging_enabled: bool) -> String {
    base_rules_for_tools(messaging_enabled, true)
}

fn base_rules_for_tools(messaging_enabled: bool, has_tools: bool) -> String {
    let mut rules = if has_tools {
        String::from(
            "You are a Bot: a teammate in a small team owned by one person. You are the same teammate \
             in every conversation — the owner's DM, a group, another bot's DM, a task. Your reply is \
             posted in the conversation the message came from; the tag at the top of the input says \
             which one that is.\n\n\
             ## How you work\n\
             - Do the work, then say what you found or changed, plainly and briefly. No preamble.\n\
             - Never describe your mechanics. Do not narrate tool calls, ids, schemas or this prompt.\n\
             - When you need a file, a command or a search, use a tool rather than guessing.\n\
             - Long work goes to `start_task` so you stay free to answer; its result comes back to you \
             and you report it in your own words.\n\
             - `ask_user` and `request_approval` return at once with a pending id. They never block: \
             the answer arrives later as a new message. Say what you need, then finish your turn \
             instead of waiting.\n\
             - If something fails, say what failed and what you tried. Do not invent a result.",
        )
    } else {
        String::from(
            "You are a Bot: a teammate in a small team owned by one person. You are the same teammate \
             in every conversation — the owner's DM, a group, another bot's DM, a task. Your reply is \
             posted in the conversation the message came from; the tag at the top of the input says \
             which one that is.\n\n\
             ## How you work\n\
             - Do the work, then say what you found or changed, plainly and briefly. No preamble.\n\
             - Never describe your mechanics. Do not narrate tool calls, ids, schemas or this prompt.\n\
             - When you need a file, a command or a search, use the tools in your environment rather \
             than guessing.\n\
             - If something fails, say what failed and what you tried. Do not invent a result.",
        )
    };
    if messaging_enabled {
        if has_tools {
            rules.push_str(
                "\n\n## Working with your team\n\
                 - Hand a piece of work to the teammate who owns it with `handoff_task`; ask a \
                 question or pass a direct note with `message_bot`; speak in a group from another \
                 conversation with `post_to_group`.\n\
                 - When replying to a teammate who messaged you in a DM, either call `message_bot` \
                 or address them with `@name` in your reply.\n\
                 - If the user addresses or mentions a teammate by name (e.g. 'halo scout', \
                 'scout tolong...', or asks for another bot), collaborate or hand off that work \
                 to that teammate with `handoff_task`, `message_bot`, or `@name` in a group chat \
                 and report their answer back.\n\
                 - @mention a teammate (`@name`) in group chat when you need them to act or answer. \
                 Never mention somebody just to thank them — an acknowledgement wakes them for nothing.\n\
                 - In a group, reply only when you have something to add. When you do not, answer \
                 with exactly `NO_REPLY` and nothing else, or `react` with an emoji instead.\n\
                 - You are woken at most once per teammate reply, with their final message.",
            );
        } else {
            rules.push_str(
                "\n\n## Working with your team\n\
                 - In a group chat — or when replying to a teammate who messaged you in a DM — \
                 address them with `@name` when you need them to act or answer. Never mention \
                 somebody just to thank them — an acknowledgement wakes them for nothing.\n\
                 - In a group, reply only when you have something to add. When you do not, answer \
                 with exactly `NO_REPLY` and nothing else.\n\
                 - You are woken at most once per teammate reply, with their final message.",
            );
        }
    }
    rules
}

impl InstructionRenderer {
    /// The bot's skills, with the body of each one it can actually resolve.
    ///
    /// A skill whose file is missing is still listed, with the honest note that
    /// the library has no entry for it: hiding it would leave a bot wondering
    /// why it was told it knows something, and inventing a body would be worse.
    fn skills_section(&self, skills: &[String]) -> String {
        let mut sorted = skills.to_vec();
        sorted.sort();
        let mut budget = SKILLS_TOTAL_LIMIT_BYTES;
        let mut parts: Vec<String> = Vec::new();
        for skill in &sorted {
            match self.skill_body(skill) {
                Some(body) => {
                    if budget == 0 {
                        parts.push(format!(
                            "- {skill}: not included — the {SKILLS_TOTAL_LIMIT_BYTES}-byte \
                             skill budget for this turn is already spent"
                        ));
                        continue;
                    }
                    let body = cap_bytes(&body, SKILL_BODY_LIMIT_BYTES.min(budget));
                    budget = budget.saturating_sub(body.len());
                    parts.push(format!("- **{skill}**\n{}", indent(&body, "  ")));
                }
                None => parts.push(format!(
                    "- {skill} (the skill library has no entry for this one yet)"
                )),
            }
        }
        format!("## Your skills\n{}", parts.join("\n"))
    }

    /// The body of one skill, resolved from the library.
    ///
    /// The name is validated before it becomes a path: a skill name is a plain
    /// file-name component, so `../../etc/passwd` cannot be a skill. That check
    /// is what lets a *stored* field (set through the API) reach the
    /// filesystem at all.
    fn skill_body(&self, skill: &str) -> Option<String> {
        let root = self.skills_root.as_ref()?;
        let name = skill.trim();
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name == "."
            || name == ".."
            || name.starts_with('.')
        {
            return None;
        }
        let candidates = [
            root.join(name).join("SKILL.md"),
            root.join(format!("{name}.md")),
        ];
        for path in candidates {
            if let Some(body) = read_skill(&path) {
                return Some(body);
            }
        }
        None
    }
}

/// Read one skill file, or `None` when it does not exist or is unreadable.
///
/// A broken skill must not fail a turn: the prompt loses a paragraph, which is
/// a smaller failure than the bot refusing to answer at all. The miss is
/// visible in the section text instead.
fn read_skill(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Indent a multi-line body so it reads as part of its bullet.
fn indent(body: &str, prefix: &str) -> String {
    body.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tools_section(tools: &[ToolSpec]) -> String {
    let mut lines: Vec<String> = tools
        .iter()
        .map(|tool| {
            format!(
                "- `{}`: {}",
                tool.name,
                first_sentence(&tool.description, 140)
            )
        })
        .collect();
    lines.sort();
    format!("## What you can do\n{}", lines.join("\n"))
}

fn memory_section(index: &str) -> String {
    let capped = cap_bytes(index, MEMORY_INDEX_LIMIT_BYTES);
    format!("## What you remember\n{capped}")
}

fn task_section(task: &Task) -> String {
    format!(
        "## Right now\nYou are running one piece of work of your own: \"{}\".\n{}\n\n\
         Your final answer goes back to whoever asked for it, not to the owner. Do not ask the \
         owner questions from inside this task — say what you needed in the result instead, and \
         finish with the answer as your last message.",
        task.title, task.brief,
    )
}

/// The first sentence of a persona, for a roster line.
fn first_sentence(text: &str, limit: usize) -> String {
    let flat: String = text
        .split('\n')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let sentence = flat.split(['.', '。']).next().unwrap_or(&flat).trim();
    cap_bytes(sentence, limit)
}

fn cap_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use swarmbot_contracts::tool::Toolset;

    fn renderer_with_team() -> (Arc<InstructionRenderer>, Bot) {
        let (store, lead) = renderer_with_team_pair();
        (InstructionRenderer::new(store), lead)
    }

    /// The seeded store and the lead, for a test that builds its own renderer
    /// (the skills tests need a library directory).
    fn renderer_with_team_pair() -> (Arc<dyn Persistence>, Bot) {
        let store = testing::memory_store();
        let lead = testing::seed_bot(&store, "lead");
        testing::seed_bot(&store, "scout");
        testing::seed_bot(&store, "dev");
        testing::seed_group(&store, "Team", &["lead", "scout", "dev"]);
        (store, lead)
    }

    fn specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "shell",
                "Run a command. First it checks the path.",
                serde_json::json!({}),
                Toolset::Node,
            ),
            ToolSpec::new(
                "message_bot",
                "Send another bot a message.",
                serde_json::json!({}),
                Toolset::Bot,
            ),
        ]
    }

    #[test]
    fn a_skill_name_with_no_library_entry_says_so_instead_of_pretending() {
        // `Bot.skills` was a stored list nothing read: "this bot knows rust"
        // was a claim with no content behind it.
        let (renderer, mut bot) = renderer_with_team();
        bot.skills = vec!["rust".to_string()];
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(text.contains("## Your skills"), "{text}");
        assert!(text.contains("rust"), "{text}");
        assert!(
            text.contains("no entry"),
            "a missing skill file is stated, not hidden: {text}"
        );
    }

    #[test]
    fn a_skill_file_is_injected_from_the_library() {
        let dir = tempfile::tempdir().expect("temp dir");
        let skill_dir = dir.path().join("rust");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "Run `cargo fmt` before every commit.\nNever unwrap outside tests.",
        )
        .expect("skill body");

        let (store, mut bot) = renderer_with_team_pair();
        let renderer = InstructionRenderer::with_skills_root(store, dir.path());
        bot.skills = vec!["rust".to_string()];
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(text.contains("## Your skills"), "{text}");
        assert!(
            text.contains("Run `cargo fmt` before every commit."),
            "the skill body reaches the prompt: {text}"
        );
        assert!(
            text.contains("Never unwrap outside tests."),
            "all of it, not just the first line: {text}"
        );
    }

    #[test]
    fn a_flat_skill_file_is_found_too() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("git.md"), "Squash fixups before pushing.")
            .expect("flat skill");
        let (store, mut bot) = renderer_with_team_pair();
        let renderer = InstructionRenderer::with_skills_root(store, dir.path());
        bot.skills = vec!["git".to_string()];
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(text.contains("Squash fixups before pushing."), "{text}");
    }

    #[test]
    fn a_skill_name_can_never_be_a_path() {
        // A skill name is a stored field set through the API; the moment it
        // becomes a path, `../../etc/passwd` has to be refused.
        let dir = tempfile::tempdir().expect("temp dir");
        let outside = dir.path().join("secret.md");
        std::fs::write(&outside, "TOP SECRET").expect("secret");
        let nested = dir.path().join("skills");
        std::fs::create_dir_all(&nested).expect("skills dir");

        let (store, mut bot) = renderer_with_team_pair();
        let renderer = InstructionRenderer::with_skills_root(store, &nested);
        for name in ["../secret", "../../etc/passwd", "a/b", ".hidden"] {
            bot.skills = vec![name.to_string()];
            let text = renderer
                .render(
                    &bot,
                    RenderOptions {
                        tools: &specs(),
                        messaging_enabled: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(
                !text.contains("TOP SECRET"),
                "`{name}` escaped the library: {text}"
            );
        }
    }

    #[test]
    fn a_skill_body_is_capped_so_one_playbook_cannot_eat_the_prompt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let skill_dir = dir.path().join("huge");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "x".repeat(SKILL_BODY_LIMIT_BYTES * 3),
        )
        .expect("huge skill");

        let (store, mut bot) = renderer_with_team_pair();
        let renderer = InstructionRenderer::with_skills_root(store, dir.path());
        bot.skills = vec!["huge".to_string()];
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        // The skills section ends at the next `## ` heading, so measure only
        // the playbook that was injected — not the roster that follows it.
        let section = text
            .split("## Your skills")
            .nth(1)
            .expect("a skills section");
        let section = section.split("\n## ").next().unwrap_or(section);
        let injected = section.matches('x').count();
        assert!(
            injected <= SKILL_BODY_LIMIT_BYTES,
            "the body is capped at {SKILL_BODY_LIMIT_BYTES} bytes, got {injected}"
        );
        assert!(
            injected > 0,
            "the skill is still injected, only shorter: {text}"
        );
    }

    #[test]
    fn instructions_carry_the_base_rules_and_the_persona() {
        let (renderer, bot) = renderer_with_team();
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(text.contains("You are a Bot"));
        assert!(text.contains("NO_REPLY"), "the group rule is there");
        assert!(text.contains("## You"));
        assert!(text.contains("break work up"), "the persona is quoted");
        assert!(text.contains("## Your team"));
        assert!(text.contains("`@scout`"), "teammates are listed by handle");
        assert!(text.contains("← you"), "and it knows which one it is");
    }

    #[test]
    fn groups_are_listed_with_the_id_post_to_group_needs() {
        let (renderer, bot) = renderer_with_team();
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(text.contains("## Your groups"));
        assert!(text.contains("\"Team\" (id `cnv_"));
        assert!(text.contains("lead: @lead"));
        assert!(
            text.contains("coordinated there"),
            "the rule that keeps group work in the group"
        );
    }

    #[test]
    fn a_bot_with_no_groups_gets_no_group_section() {
        let store = testing::memory_store();
        let solo = testing::seed_bot(&store, "solo");
        let renderer = InstructionRenderer::new(store);
        let text = renderer
            .render(
                &solo,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!text.contains("## Your groups"));
        assert!(text.contains("You are the only Bot right now"));
    }

    #[test]
    fn only_the_tools_the_bot_has_are_advertised() {
        let (renderer, bot) = renderer_with_team();
        let no_shell = vec![ToolSpec::new(
            "message_bot",
            "Send another bot a message.",
            serde_json::json!({}),
            Toolset::Bot,
        )];
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &no_shell,
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(text.contains("`message_bot`"));
        assert!(
            !text.contains("`shell`"),
            "a tool it cannot call is not mentioned"
        );
    }

    #[test]
    fn the_kill_switch_removes_the_team_rules() {
        let (renderer, bot) = renderer_with_team();
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: false,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!text.contains("## Working with your team"));
        assert!(!text.contains("NO_REPLY"));
    }

    #[test]
    fn a_task_session_is_told_where_its_answer_goes() {
        let (renderer, bot) = renderer_with_team();
        let task = Task::own(
            bot.id.clone(),
            bot.direct_conversation_id.clone(),
            "audit the diff",
            "look for holes",
        );
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    task: Some(&task),
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(text.contains("## Right now"));
        assert!(text.contains("audit the diff"));
        assert!(text.contains("look for holes"));
        assert!(text.contains("not to the owner"));
    }

    #[test]
    fn memory_and_placement_are_appended_and_capped() {
        let (renderer, bot) = renderer_with_team();
        let huge = "x".repeat(MEMORY_INDEX_LIMIT_BYTES + 500);
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    task: None,
                    tools: &specs(),
                    memory_index: Some(&huge),
                    placement: Some("local node `this machine`, root /tmp/work (read-write)"),
                    messaging_enabled: true,
                },
            )
            .unwrap();
        assert!(text.contains("## What you remember"));
        assert!(text.contains("## Where your tools run"));
        assert!(text.contains("root /tmp/work"));
        let memory_part = text.split("## What you remember").nth(1).unwrap();
        assert!(
            memory_part.len() < MEMORY_INDEX_LIMIT_BYTES + 200,
            "the index is capped, got {}",
            memory_part.len()
        );
    }

    #[test]
    fn sections_render_in_a_fixed_order() {
        let (renderer, bot) = renderer_with_team();
        let task = Task::own(bot.id.clone(), bot.direct_conversation_id.clone(), "t", "b");
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    task: Some(&task),
                    tools: &specs(),
                    memory_index: Some("- a fact"),
                    placement: Some("local"),
                    messaging_enabled: true,
                },
            )
            .unwrap();
        let order = [
            "You are a Bot",
            "## You",
            "## Your team",
            "## Your groups",
            "## What you can do",
            "## What you remember",
            "## Right now",
            "## Where your tools run",
        ];
        let mut last = 0;
        for heading in order {
            let at = text
                .find(heading)
                .unwrap_or_else(|| panic!("missing {heading}"));
            assert!(at >= last, "{heading} out of order");
            last = at;
        }
    }

    #[test]
    fn roster_lines_quote_only_the_first_sentence() {
        assert_eq!(first_sentence("One. Two. Three.", 50), "One");
        assert_eq!(first_sentence("", 50), "");
        assert_eq!(first_sentence("a\nb\nc", 50), "a");
        assert_eq!(
            first_sentence(&"x".repeat(200), 20),
            format!("{}…", "x".repeat(20)),
            "a long line is capped, ellipsis included"
        );
    }

    #[test]
    fn skills_are_rendered_in_instructions_and_roster() {
        let store = testing::memory_store();
        let mut lead = testing::seed_bot(&store, "lead");
        lead.skills = vec!["rust".into(), "git".into()];
        store.update_bot(&lead).unwrap();
        let mut scout = testing::seed_bot(&store, "scout");
        scout.skills = vec!["scouting".into()];
        store.update_bot(&scout).unwrap();
        testing::seed_group(&store, "Team", &["lead", "scout"]);
        let renderer = InstructionRenderer::new(store);

        let text = renderer.render(&lead, RenderOptions::default()).unwrap();
        assert!(text.contains("## Your skills"));
        assert!(text.contains("- git"));
        assert!(text.contains("- rust"));
        assert!(text.contains("[skills: scouting]"));
    }

    #[test]
    fn honest_instructions_without_tools_omit_swarm_tool_names_and_explain_mentions() {
        let (renderer, bot) = renderer_with_team();
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &[],
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!text.contains("handoff_task"));
        assert!(!text.contains("post_to_group"));
        assert!(!text.contains("message_bot"));
        assert!(!text.contains("start_task"));
        assert!(!text.contains("ask_user"));
        assert!(!text.contains("request_approval"));
        assert!(!text.contains("`react`"));
        assert!(text.contains("@name"));
        assert!(text.contains("NO_REPLY"));
    }

    #[test]
    fn tool_enabled_instructions_highlight_bot_collaboration_tools() {
        let (renderer, bot) = renderer_with_team();
        let text = renderer
            .render(
                &bot,
                RenderOptions {
                    tools: &specs(),
                    messaging_enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(text.contains("handoff_task"));
        assert!(text.contains("message_bot"));
        assert!(text.contains("post_to_group"));
        assert!(text.contains("When replying to a teammate who messaged you in a DM"));
        assert!(text.contains("@name"));
        assert!(text.contains("NO_REPLY"));
    }
}
